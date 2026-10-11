//! One `claude -p` process for one turn: its command line, its output, its deadline and its
//! end. The chat and the checks of a review run through here and differ only in the sink that
//! receives what the program says and in the session and places they use.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clusia_core::PrRef;
use clusia_core::config::Harness;
use clusia_harness::{
    AgentEvent, BridgeSpec, ClaudeCode, ParseState, SessionArg, TurnSpec, parse_line,
};
use clusia_protocol::{AgentErrorKind, TurnOrigin};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use crate::state::Shared;

/// How long a process gets to leave after SIGTERM before SIGKILL.
pub(crate) const KILL_GRACE: Duration = Duration::from_secs(2);
/// How much of the program's standard error is kept for the error line.
const STDERR_TAIL: usize = 2048;

/// Who asked for a running turn to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    User,
    Shutdown,
    Ended,
}

impl Why {
    /// What the chat says about a turn that was stopped for this reason.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Why::User => "Stopped by you",
            Why::Shutdown => "The daemon stopped while this turn was running",
            Why::Ended => "Stopped because the review ended",
        }
    }
}

/// Asks a turn to stop, and says why.
pub(crate) struct Stop {
    notify: Notify,
    why: Mutex<Why>,
    fired: AtomicBool,
}

impl Stop {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            notify: Notify::new(),
            why: Mutex::new(Why::User),
            fired: AtomicBool::new(false),
        })
    }

    pub(crate) fn fire(&self, why: Why) {
        *self.why.lock().unwrap_or_else(|p| p.into_inner()) = why;
        self.fired.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    pub(crate) fn why(&self) -> Why {
        *self.why.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether the turn was asked to stop, now or earlier.
    pub(crate) fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    /// Resolves once the turn is asked to stop; a stop asked before is not lost.
    pub(crate) async fn notified(&self) {
        self.notify.notified().await;
    }
}

/// Where the `claude` command is: the program set in Config › Harness, else the daemon's own
/// override, else `claude` on the daemon's `PATH` or in the usual install folders. A name that
/// is nowhere is returned as is, so starting it fails with "not found".
pub(crate) fn program_path(shared: &Shared, harness: &Harness) -> PathBuf {
    let configured = harness
        .program
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let name = match (configured, &shared.claude_program) {
        (Some(program), _) => program,
        (None, Some(program)) => return program.clone(),
        (None, None) => "claude",
    };
    if name.contains('/') {
        return PathBuf::from(name);
    }
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    on_path
        .iter()
        .chain(&shared.harness_search_paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Reads what the program wrote to standard error, keeping only the end.
async fn read_tail(stderr: Option<ChildStderr>) -> String {
    let Some(mut stderr) = stderr else {
        return String::new();
    };
    let mut tail = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.extend_from_slice(&buffer[..n]);
                if tail.len() > STDERR_TAIL {
                    let extra = tail.len() - STDERR_TAIL;
                    tail.drain(..extra);
                }
            }
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}

/// What a program that wrote no result line said on standard error, as an error kind.
pub(crate) fn classify_stderr(text: &str) -> Option<AgentErrorKind> {
    let text = text.to_lowercase();
    if text.contains("not logged in") || text.contains("/login") || text.contains("invalid api key")
    {
        Some(AgentErrorKind::NotSignedIn)
    } else if text.contains("usage limit") || text.contains("rate limit") {
        Some(AgentErrorKind::UsageLimit)
    } else {
        None
    }
}

fn signal_group(pid: Option<u32>, signal: i32) {
    if let Some(pid) = pid {
        // SAFETY: `killpg` only sends a signal. The group is the one the child leads
        // (`process_group(0)` at spawn), so no other process is addressed.
        unsafe {
            libc::killpg(pid as i32, signal);
        }
    }
}

/// Whether the child has ended, without reaping it: until it is reaped its pid, and so the id
/// of the group it leads, cannot be reused, so the group can still be signalled safely.
pub(crate) fn has_exited(pid: u32) -> bool {
    loop {
        // SAFETY: `waitid` only writes into `info`, which is zeroed and owned here; `WNOWAIT`
        // leaves the child waitable for `Child::wait`.
        let (found, info) = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let found = libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            );
            (found, info)
        };
        if found == 0 {
            return info.si_signo == libc::SIGCHLD;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            // A signal arrived during the call: it says nothing about the child, so ask again.
            Some(libc::EINTR) => continue,
            // No such child: it is already reaped, so there is nothing left to wait for.
            Some(libc::ECHILD) => return true,
            _ => return false,
        }
    }
}

/// Waits up to `limit` for the child to end, leaving it unreaped; whether it ended.
async fn exited_within(pid: u32, limit: Duration) -> bool {
    let give_up = Instant::now() + limit;
    loop {
        if has_exited(pid) {
            return true;
        }
        if Instant::now() >= give_up {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// SIGKILL to whatever is left of the child's group (tool processes it started that outlive
/// it), then reaps the child. Only called while the child is not yet reaped.
async fn sweep_and_reap(child: &mut Child, pid: Option<u32>) -> Option<ExitStatus> {
    signal_group(pid, libc::SIGKILL);
    child.wait().await.ok()
}

/// SIGTERM to the child's whole group, SIGKILL to the group once the child ended or after
/// `KILL_GRACE`, then reaps the child. The child is reaped last, so its group id is never
/// reused while it is signalled.
async fn terminate(child: &mut Child) {
    let pid = child.id();
    signal_group(pid, libc::SIGTERM);
    if let Some(pid) = pid {
        exited_within(pid, KILL_GRACE).await;
    }
    sweep_and_reap(child, pid).await;
}

#[derive(Clone, Copy)]
enum Ending {
    Eof,
    Stopped,
    TimedOut,
}

/// A future that borrows its sink, so a sink can be used through `&mut dyn TurnSink`.
pub(crate) type Boxed<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// What a turn tells whoever runs it. Every call comes from the one task that runs the turn.
pub(crate) trait TurnSink: Send {
    /// No place was free: the turn waits for one. Called at most once.
    fn waiting(&mut self);
    /// The program started.
    fn started(&mut self);
    /// One event of the program's output.
    fn event<'a>(&'a mut self, event: AgentEvent) -> Boxed<'a>;
    /// The turn itself failed or was cut short (a bad setting, no program, a stop, a deadline);
    /// an error line of the program arrives through `event` instead.
    fn error(&mut self, kind: AgentErrorKind, message: String);
    /// Lets out whatever text the sink still holds back.
    fn release(&mut self);
    /// Writes down whatever the sink has not yet written.
    fn flush(&mut self);
    /// The program printed its result line.
    fn saw_final(&self) -> bool;
    /// An error was reported, by the program or by the turn.
    fn failed(&self) -> bool;
}

/// Which session a turn runs in.
pub(crate) enum SessionSource {
    /// The review's chat: `--resume` its session, or `--session-id` for the first turn.
    Chat,
    /// A copy of the review's session that the chat never sees: `--resume <session>
    /// --fork-session --session-id <new id>`, or a session of its own when the review has none.
    /// Never recorded as the review's session.
    Fork,
    /// This session, whatever the review holds.
    #[allow(dead_code)]
    Given(SessionArg),
}

/// One turn to run.
pub(crate) struct TurnRun {
    pub pr: PrRef,
    /// The turn id, unique in the review: from `Sessions::next_turn`, or the chat's own.
    pub turn: u64,
    pub prompt: String,
    /// The prompt to use instead when a `Fork` finds no session to copy.
    pub fresh_prompt: Option<String>,
    pub session: SessionSource,
    /// Who the permission requests of this turn say asked. A turn that is not the chat is known
    /// to `Sessions` while its program runs, so the bridge's requests are accepted.
    pub origin: TurnOrigin,
    /// The places this turn waits for one of.
    pub places: Arc<Semaphore>,
    /// How long the program may run; the harness's turn limit when `None`.
    pub limit: Option<Duration>,
    pub stop: Arc<Stop>,
}

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnEnd {
    /// The program printed its result and no error was reported.
    Done,
    /// An error was reported, or the program ended without a result.
    Failed,
    /// Stopped before it said its result.
    Stopped(Why),
    /// The program ran past the limit before it said its result.
    TimedOut,
}

/// Waits for one of `places`; `None` when stopped while waiting.
async fn wait_for_place(
    places: &Arc<Semaphore>,
    stop: &Stop,
    sink: &mut dyn TurnSink,
) -> Option<OwnedSemaphorePermit> {
    if let Ok(permit) = places.clone().try_acquire_owned() {
        return Some(permit);
    }
    sink.waiting();
    tokio::select! {
        permit = places.clone().acquire_owned() => permit.ok(),
        _ = stop.notified() => None,
    }
}

/// Runs one turn: waits for a place, starts the program, hands its output to `sink` and ends
/// it when it stops, when the limit passes or when asked. No process of the turn outlives this
/// call, and whatever permission it still asked for is cancelled.
pub(crate) async fn run(shared: &Arc<Shared>, run: TurnRun, sink: &mut dyn TurnSink) -> TurnEnd {
    let TurnRun {
        pr,
        turn,
        prompt,
        fresh_prompt,
        session,
        origin,
        places,
        limit,
        stop,
    } = run;
    let Some(_place) = wait_for_place(&places, &stop, sink).await else {
        sink.error(
            AgentErrorKind::Interrupted,
            stop.why().message().to_string(),
        );
        return TurnEnd::Stopped(stop.why());
    };
    // A stop that came while the turn waited, or just as it got its place, starts nothing.
    if stop.fired() {
        sink.error(
            AgentErrorKind::Interrupted,
            stop.why().message().to_string(),
        );
        return TurnEnd::Stopped(stop.why());
    }
    if origin != TurnOrigin::Chat {
        shared.sessions.register_check(&pr, turn, origin);
    }
    let end = execute(
        shared,
        &pr,
        turn,
        prompt,
        fresh_prompt,
        session,
        limit,
        &stop,
        sink,
    )
    .await;
    // Whatever the turn was still asking, it asks no more.
    crate::permissions::cancel_turn(shared, &pr, turn).await;
    if origin != TurnOrigin::Chat {
        shared.sessions.unregister_check(&pr, turn);
    }
    end
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    shared: &Arc<Shared>,
    pr: &PrRef,
    turn: u64,
    prompt: String,
    fresh_prompt: Option<String>,
    session: SessionSource,
    limit: Option<Duration>,
    stop: &Arc<Stop>,
    sink: &mut dyn TurnSink,
) -> TurnEnd {
    let harness = shared.config.read().await.harness.clone();
    let extra_args = match harness.extra_args_list() {
        Ok(args) => args,
        Err(message) => {
            sink.error(
                AgentErrorKind::Crashed,
                format!("Config › Harness: {message}"),
            );
            return TurnEnd::Failed;
        }
    };
    let cwd = shared.paths.worktree_for(pr);
    let Some(bridge_program) = shared
        .bridge_program
        .clone()
        .or_else(|| std::env::current_exe().ok())
    else {
        sink.error(
            AgentErrorKind::Crashed,
            "Clúsia cannot find its own program to ask you for permission".to_string(),
        );
        return TurnEnd::Failed;
    };
    // Claude Code gives up on a tool call after `MCP_TOOL_TIMEOUT`; the question to the
    // reviewer may take until our own deadline.
    let mut base_env: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    base_env.retain(|(key, _)| key != "MCP_TOOL_TIMEOUT");
    base_env.push((
        "MCP_TOOL_TIMEOUT".into(),
        ((u64::from(harness.permission_timeout_secs) + 30) * 1000)
            .to_string()
            .into(),
    ));
    // Requests of this turn show, and are judged by, the sandbox its program runs with, even
    // when the setting changes before the turn ends.
    shared.sessions.set_turn_sandbox(pr, turn, harness.sandbox);
    let (session, prompt) = match session {
        SessionSource::Chat => (shared.sessions.session_for(shared, pr), prompt),
        SessionSource::Fork => {
            let session = shared.sessions.fork_session(shared, pr);
            let prompt = match (&session, fresh_prompt) {
                (SessionArg::New(_), Some(fresh)) => fresh,
                _ => prompt,
            };
            (session, prompt)
        }
        SessionSource::Given(session) => (session, prompt),
    };
    let spec = TurnSpec {
        program: program_path(shared, &harness),
        prompt,
        cwd: cwd.clone(),
        session,
        use_cli_permissions: harness.use_cli_permissions,
        extra_args,
        base_env,
        bridge: Some(BridgeSpec {
            program: bridge_program,
            socket: shared.paths.socket(),
            pr: pr.to_string(),
            turn,
        }),
        sandbox: harness.sandbox,
        // No rule goes on the command line: every request comes to the daemon, which decides
        // it with the review's rules.
        rules: Vec::new(),
    };
    let mut command = Command::from(ClaudeCode::command(&spec));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let kind = AgentErrorKind::NotInstalled;
            sink.error(kind, kind.default_message().to_string());
            return TurnEnd::Failed;
        }
        Err(e) => {
            let kind = AgentErrorKind::Crashed;
            sink.error(kind, format!("{}: {e}", kind.default_message()));
            return TurnEnd::Failed;
        }
    };
    sink.started();

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = tokio::spawn(read_tail(child.stderr.take()));
    let mut stdout = BufReader::new(stdout);
    // Bytes, not text: a line that is not UTF-8 is converted lossily instead of ending the read.
    let mut line = Vec::new();
    let mut parse = ParseState::with_root(cwd);
    let limit = limit.unwrap_or_else(|| Duration::from_secs(u64::from(harness.turn_timeout_secs)));
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let started = Instant::now();
    let ending = loop {
        tokio::select! {
            read = stdout.read_until(b'\n', &mut line) => match read {
                Ok(n) if n > 0 => {
                    let text = String::from_utf8_lossy(&line);
                    for event in parse_line(text.trim_end_matches(['\n', '\r']), &mut parse) {
                        sink.event(event).await;
                    }
                    line.clear();
                }
                _ => break Ending::Eof,
            },
            _ = stop.notified() => break Ending::Stopped,
            _ = &mut deadline => break Ending::TimedOut,
        }
    };
    // The turn is over for the agent: nobody is left to hear an answer.
    crate::permissions::cancel_turn(shared, pr, turn).await;
    let tail = |task: tokio::task::JoinHandle<String>| async move {
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
    };
    // What the sink knew before the turn added its own ending: a turn that already said its
    // result keeps that ending, and so does one that already failed.
    let (said_result, had_failed) = (sink.saw_final(), sink.failed());
    match ending {
        Ending::Eof => {
            let pid = child.id();
            let status = match pid {
                Some(id) if !exited_within(id, KILL_GRACE).await => {
                    terminate(&mut child).await;
                    None
                }
                _ => sweep_and_reap(&mut child, pid).await,
            };
            ended(sink, status, &tail(stderr).await);
        }
        Ending::Stopped => {
            terminate(&mut child).await;
            sink.release();
            if !said_result && !had_failed {
                sink.error(
                    AgentErrorKind::Interrupted,
                    stop.why().message().to_string(),
                );
            }
        }
        Ending::TimedOut if said_result || had_failed => {
            terminate(&mut child).await;
            sink.release();
        }
        Ending::TimedOut => {
            terminate(&mut child).await;
            sink.release();
            sink.error(
                AgentErrorKind::Interrupted,
                format!(
                    "The turn ran past its limit of {}s and was stopped",
                    limit.as_secs()
                ),
            );
        }
    }
    sink.flush();
    tracing::debug!(pr = %pr, turn, elapsed_ms = started.elapsed().as_millis() as u64, "agent turn ended");
    match (said_result, had_failed, ending) {
        (true, false, _) => TurnEnd::Done,
        (_, true, _) | (_, _, Ending::Eof) => TurnEnd::Failed,
        (_, _, Ending::Stopped) => TurnEnd::Stopped(stop.why()),
        (_, _, Ending::TimedOut) => TurnEnd::TimedOut,
    }
}

/// The program ended without a result line and without an error of its own: says what its
/// exit and its standard error tell.
fn ended(sink: &mut dyn TurnSink, status: Option<ExitStatus>, stderr: &str) {
    sink.release();
    if sink.saw_final() || sink.failed() {
        return;
    }
    let kind = classify_stderr(stderr).unwrap_or(AgentErrorKind::Crashed);
    let said = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty());
    let mut message = kind.default_message().to_string();
    match (said, status.and_then(|s| s.code())) {
        (Some(said), _) => message = format!("{message}: {said}"),
        (None, Some(0)) => message = "Claude Code ended without an answer".to_string(),
        (None, Some(code)) => message = format!("{message} (exit code {code})"),
        (None, None) => {}
    }
    sink.error(kind, message);
}
