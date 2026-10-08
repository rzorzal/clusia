//! Agent turns: one `claude -p` process per message, run one at a time for each review.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clusia_core::PrRef;
use clusia_core::config::Harness;
use clusia_harness::{
    AgentEvent, ClaudeCode, ParseState, SessionArg, TurnSpec, extract_suggestions, parse_line,
    parse_probe, probe_command,
};
use clusia_protocol::{
    AgentErrorKind, AgentLogEntry, ErrorCode, Event, Outcome, ProbeResult, ProtocolError, Reply,
    SessionStateKind, Suggestion, topics,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};

use crate::agent_log;
use crate::agent_stream::StreamFilter;
use crate::reviews;
use crate::state::Shared;
use crate::sync::now_unix;

/// Turns running at the same time across all reviews; the rest wait.
pub(crate) const MAX_CONCURRENT_TURNS: usize = 3;
/// How long a process gets to leave after SIGTERM before SIGKILL.
pub(crate) const KILL_GRACE: Duration = Duration::from_secs(2);
const PROBE_LIMIT: Duration = Duration::from_secs(10);
/// How much of the program's standard error is kept for the error line.
const STDERR_TAIL: usize = 2048;

/// What a turn asks the agent. `shown` is false for the prompts the daemon writes itself,
/// which the chat never displays as something the user said.
#[derive(Clone)]
pub(crate) struct Prompt {
    pub text: String,
    pub shown: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A turn is running and another one already waits.
    Busy,
    NoReview(String),
    Stopping,
}

impl Refusal {
    pub(crate) fn into_outcome(self) -> Outcome {
        let (code, message) = match self {
            Refusal::Busy => (
                ErrorCode::Busy,
                AgentErrorKind::Busy.default_message().into(),
            ),
            Refusal::NoReview(message) => (ErrorCode::InvalidState, message),
            Refusal::Stopping => (ErrorCode::InvalidState, "clusiad is stopping".to_string()),
        };
        Outcome::Err(ProtocolError::new(code, message))
    }
}

/// Who asked for a running turn to end.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Why {
    User,
    Shutdown,
}

impl Why {
    /// What the chat says about a turn that was stopped for this reason.
    fn message(self) -> &'static str {
        match self {
            Why::User => "Stopped by you",
            Why::Shutdown => "The daemon stopped while this turn was running",
        }
    }
}

/// Asks a turn to stop, and says why.
struct Stop {
    notify: Notify,
    why: Mutex<Why>,
}

impl Stop {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            notify: Notify::new(),
            why: Mutex::new(Why::User),
        })
    }

    fn fire(&self, why: Why) {
        *self.why.lock().unwrap_or_else(|p| p.into_inner()) = why;
        self.notify.notify_one();
    }

    fn why(&self) -> Why {
        *self.why.lock().unwrap_or_else(|p| p.into_inner())
    }
}

struct Active {
    stop: Arc<Stop>,
}

struct Queued {
    turn: u64,
    prompt: Prompt,
}

#[derive(Default)]
struct Slot {
    /// The highest turn id handed out for this review.
    last_turn: u64,
    running: Option<Active>,
    queued: Option<Queued>,
    /// The id a first turn starts its session with, kept until the CLI confirms the session.
    fresh_session: Option<String>,
}

/// The running agent turns of every review.
pub(crate) struct Sessions {
    table: Mutex<HashMap<PrRef, Slot>>,
    permits: Arc<Semaphore>,
    /// How many review drivers are running; shutdown waits for zero.
    live: watch::Sender<usize>,
    closing: AtomicBool,
}

impl Default for Sessions {
    fn default() -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_TURNS)),
            live: watch::channel(0).0,
            closing: AtomicBool::new(false),
        }
    }
}

/// Counts a review driver in `Sessions::live` while it lives.
struct Live(Arc<Shared>);

impl Live {
    fn new(shared: &Arc<Shared>) -> Self {
        shared.sessions.live.send_modify(|n| *n += 1);
        Self(shared.clone())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.sessions.live.send_modify(|n| *n -= 1);
    }
}

fn announce(shared: &Shared, pr: &PrRef, state: SessionStateKind) {
    shared.publish(
        topics::AGENT,
        Event::SessionState {
            pr: pr.clone(),
            state,
        },
    );
}

impl Sessions {
    /// Starts a turn for `pr`, or queues it behind the running one. Returns the turn id.
    pub(crate) async fn submit(
        shared: &Arc<Shared>,
        pr: &PrRef,
        prompt: Prompt,
    ) -> Result<u64, Refusal> {
        let sessions = &shared.sessions;
        if sessions.closing.load(Ordering::SeqCst) {
            return Err(Refusal::Stopping);
        }
        let has_review = matches!(reviews::load_stored(shared, pr), Ok(Some(_)));
        if !has_review || !shared.paths.worktree_for(pr).is_dir() {
            return Err(Refusal::NoReview(format!(
                "no review for {pr}; open it first"
            )));
        }
        let log = shared.paths.agent_log(pr);
        let floor = {
            let log = log.clone();
            tokio::task::spawn_blocking(move || agent_log::last_turn(&log))
                .await
                .unwrap_or(0)
        };
        let (turn, start) = {
            let mut table = sessions.table.lock().unwrap_or_else(|p| p.into_inner());
            if sessions.closing.load(Ordering::SeqCst) {
                return Err(Refusal::Stopping);
            }
            let slot = table.entry(pr.clone()).or_default();
            if slot.running.is_some() && slot.queued.is_some() {
                return Err(Refusal::Busy);
            }
            slot.last_turn = slot.last_turn.max(floor) + 1;
            let turn = slot.last_turn;
            if slot.running.is_some() {
                slot.queued = Some(Queued {
                    turn,
                    prompt: prompt.clone(),
                });
                (turn, None)
            } else {
                let stop = Stop::new();
                slot.running = Some(Active { stop: stop.clone() });
                // Counted before the lock is released, so a shutdown that follows waits for it.
                (turn, Some((stop, Live::new(shared))))
            }
        };
        if prompt.shown {
            Out::new(shared, pr, turn).log(AgentLogEntry::User {
                at: now_unix(),
                turn,
                text: prompt.text.clone(),
            });
        }
        match start {
            Some((stop, live)) => {
                tokio::spawn(drive(live, shared.clone(), pr.clone(), turn, prompt, stop));
            }
            None => announce(shared, pr, SessionStateKind::Queued),
        }
        Ok(turn)
    }

    /// Stops the running turn of `pr` and drops the one waiting behind it.
    pub(crate) fn cancel(&self, shared: &Shared, pr: &PrRef) {
        self.stop_with(shared, pr, Why::User);
    }

    fn stop_with(&self, shared: &Shared, pr: &PrRef, why: Why) {
        let (stop, dropped) = {
            let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
            let Some(slot) = table.get_mut(pr) else {
                return;
            };
            (
                slot.running.as_ref().map(|active| active.stop.clone()),
                slot.queued.take(),
            )
        };
        if let Some(queued) = dropped {
            Out::new(shared, pr, queued.turn).error(
                AgentErrorKind::Interrupted,
                format!("{} before it started", why.message()),
            );
        }
        if let Some(stop) = stop {
            stop.fire(why);
        }
    }

    /// Where the session of `pr` stands right now.
    #[cfg(test)]
    pub(crate) fn state(&self, pr: &PrRef) -> SessionStateKind {
        let table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        match table.get(pr) {
            None => SessionStateKind::None,
            Some(slot) if slot.running.is_some() => SessionStateKind::Running,
            Some(_) => SessionStateKind::Ready,
        }
    }

    /// Stops every turn and waits until no process of theirs is left. New turns are refused.
    pub(crate) async fn shutdown(&self, shared: &Shared) {
        self.closing.store(true, Ordering::SeqCst);
        let dropped: Vec<(PrRef, u64)> = {
            let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
            let mut dropped = Vec::new();
            for (pr, slot) in table.iter_mut() {
                if let Some(queued) = slot.queued.take() {
                    dropped.push((pr.clone(), queued.turn));
                }
                if let Some(active) = &slot.running {
                    active.stop.fire(Why::Shutdown);
                }
            }
            dropped
        };
        for (pr, turn) in dropped {
            Out::new(shared, &pr, turn).error(
                AgentErrorKind::Interrupted,
                format!("{} before it started", Why::Shutdown.message()),
            );
        }
        let mut live = self.live.subscribe();
        let _ = tokio::time::timeout(
            KILL_GRACE + Duration::from_secs(3),
            live.wait_for(|n| *n == 0),
        )
        .await;
    }

    /// Called when a turn ended: the queued turn takes over, or the session is ready.
    fn next_after(&self, shared: &Shared, pr: &PrRef) -> Option<(u64, Prompt, Arc<Stop>)> {
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        let slot = table.get_mut(pr)?;
        match slot.queued.take() {
            Some(queued) => {
                let stop = Stop::new();
                slot.running = Some(Active { stop: stop.clone() });
                Some((queued.turn, queued.prompt, stop))
            }
            None => {
                slot.running = None;
                announce(shared, pr, SessionStateKind::Ready);
                None
            }
        }
    }

    /// `--resume` for a session the CLI knows, `--session-id` with a fresh uuid otherwise.
    fn session_for(&self, shared: &Shared, pr: &PrRef) -> SessionArg {
        if let Ok(Some(review)) = reviews::load_stored(shared, pr)
            && let Some(id) = review.harness_session
        {
            return SessionArg::Resume(id);
        }
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        let slot = table.entry(pr.clone()).or_default();
        let id = slot
            .fresh_session
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
        SessionArg::New(id.clone())
    }

    fn session_confirmed(&self, pr: &PrRef) {
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(slot) = table.get_mut(pr) {
            slot.fresh_session = None;
        }
    }
}

/// Runs the turns of one review, one after the other, until none waits.
async fn drive(
    _live: Live,
    shared: Arc<Shared>,
    pr: PrRef,
    turn: u64,
    prompt: Prompt,
    stop: Arc<Stop>,
) {
    let mut next = Some((turn, prompt, stop));
    while let Some((turn, prompt, stop)) = next {
        let task = tokio::spawn(run_turn(shared.clone(), pr.clone(), turn, prompt, stop));
        if task.await.is_err() {
            tracing::error!(pr = %pr, turn, "an agent turn failed inside the daemon");
            Out::new(&shared, &pr, turn).error(
                AgentErrorKind::Crashed,
                AgentErrorKind::Crashed.default_message().to_string(),
            );
        }
        next = shared.sessions.next_after(&shared, &pr);
    }
}

/// Publishes what a turn does and writes it to the agent log.
struct Out<'a> {
    shared: &'a Shared,
    pr: &'a PrRef,
    turn: u64,
    /// Streamed text not yet written to the log, which keeps one entry per stretch of text.
    text: String,
}

impl<'a> Out<'a> {
    fn new(shared: &'a Shared, pr: &'a PrRef, turn: u64) -> Self {
        Self {
            shared,
            pr,
            turn,
            text: String::new(),
        }
    }

    fn log(&self, entry: AgentLogEntry) {
        let path = self.shared.paths.agent_log(self.pr);
        if let Err(e) = agent_log::append(&path, &entry) {
            tracing::warn!(error = %e, pr = %self.pr, "cannot append to the agent log");
        }
    }

    fn send(&self, event: Event) {
        self.shared.publish(topics::AGENT, event);
    }

    fn flush(&mut self) {
        if !self.text.is_empty() {
            let text = std::mem::take(&mut self.text);
            self.log(AgentLogEntry::Text {
                at: now_unix(),
                turn: self.turn,
                text,
            });
        }
    }

    fn chunk(&mut self, text: String) {
        self.text.push_str(&text);
        self.send(Event::AgentChunk {
            pr: self.pr.clone(),
            turn: self.turn,
            text,
        });
    }

    fn tool(&mut self, summary: String) {
        self.flush();
        self.log(AgentLogEntry::ToolUse {
            at: now_unix(),
            turn: self.turn,
            summary: summary.clone(),
        });
        self.send(Event::AgentToolUse {
            pr: self.pr.clone(),
            turn: self.turn,
            summary,
        });
    }

    fn denied(&mut self, tool: String, detail: String) {
        self.flush();
        self.log(AgentLogEntry::Denied {
            at: now_unix(),
            turn: self.turn,
            tool: tool.clone(),
            detail: detail.clone(),
        });
        self.send(Event::AgentDenied {
            pr: self.pr.clone(),
            turn: self.turn,
            tool,
            detail,
        });
    }

    fn suggestion(&mut self, suggestion: Suggestion) {
        self.flush();
        self.log(AgentLogEntry::Suggestion {
            at: now_unix(),
            turn: self.turn,
            suggestion: suggestion.clone(),
        });
        self.send(Event::AgentSuggestion {
            pr: self.pr.clone(),
            turn: self.turn,
            suggestion,
        });
    }

    fn done(&mut self, duration_ms: u64) {
        self.flush();
        self.log(AgentLogEntry::Done {
            at: now_unix(),
            turn: self.turn,
            duration_ms,
        });
        self.send(Event::AgentDone {
            pr: self.pr.clone(),
            turn: self.turn,
            duration_ms,
        });
    }

    fn error(&mut self, kind: AgentErrorKind, message: String) {
        self.flush();
        self.log(AgentLogEntry::Error {
            at: now_unix(),
            turn: self.turn,
            kind,
            message: message.clone(),
        });
        self.send(Event::AgentError {
            pr: self.pr.clone(),
            turn: self.turn,
            kind,
            message,
        });
    }
}

/// Where the `claude` command is: the program set in Config › Harness, else the daemon's own
/// override, else `claude` on the daemon's `PATH` or in the usual install folders. A name that
/// is nowhere is returned as is, so starting it fails with "not found".
fn program_path(shared: &Shared, harness: &Harness) -> PathBuf {
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
fn classify_stderr(text: &str) -> Option<AgentErrorKind> {
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

/// SIGTERM to the child's whole group, SIGKILL to the group after `KILL_GRACE`, then waits for
/// the child. Once the child is reaped its group id may be reused, so nothing is sent after that.
async fn terminate(child: &mut Child) {
    let pid = child.id();
    signal_group(pid, libc::SIGTERM);
    if tokio::time::timeout(KILL_GRACE, child.wait())
        .await
        .is_err()
    {
        signal_group(pid, libc::SIGKILL);
        let _ = child.wait().await;
    }
}

enum Ending {
    Eof,
    Stopped,
    TimedOut,
}

/// The state of one turn while its output is read.
struct Run<'a> {
    shared: &'a Arc<Shared>,
    out: Out<'a>,
    filter: StreamFilter,
    saw_final: bool,
    failed: bool,
    /// Some answer text already went out as chunks.
    streamed: bool,
}

impl Run<'_> {
    /// Lets out the text the suggestion filter still holds.
    fn release(&mut self) {
        let rest = self.filter.finish();
        if !rest.is_empty() {
            self.streamed = true;
            self.out.chunk(rest);
        }
    }

    async fn handle(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(text) => {
                let shown = self.filter.push(&text);
                if !shown.is_empty() {
                    self.streamed = true;
                    self.out.chunk(shown);
                }
            }
            AgentEvent::ToolUse(summary) => self.out.tool(summary),
            AgentEvent::Denied { tool, detail } => self.out.denied(tool, detail),
            AgentEvent::SessionId(id) => remember_session(self.shared, self.out.pr, &id).await,
            AgentEvent::Error { kind, message } => {
                self.failed = true;
                self.out.error(kind, message);
            }
            AgentEvent::Final {
                text,
                duration_ms,
                is_error,
            } => {
                self.saw_final = true;
                self.release();
                let (clean, suggestions) = extract_suggestions(&text);
                if !self.streamed && !clean.trim().is_empty() {
                    self.streamed = true;
                    self.out.chunk(clean);
                }
                if is_error && !self.failed {
                    self.failed = true;
                    let message = match text.trim() {
                        "" => AgentErrorKind::Crashed.default_message().to_string(),
                        said => said.to_string(),
                    };
                    self.out.error(AgentErrorKind::Crashed, message);
                }
                let known = clusia_store::agent::load_agent_state(&self.shared.paths, self.out.pr)
                    .unwrap_or_default();
                let mut sent = HashSet::new();
                for suggestion in suggestions {
                    let hidden = known.dismissed.contains(&suggestion.id)
                        || known.accepted.contains(&suggestion.id);
                    if !hidden && sent.insert(suggestion.id.clone()) {
                        self.out.suggestion(suggestion);
                    }
                }
                if !self.failed {
                    self.out.done(duration_ms);
                }
            }
        }
    }

    /// The program ended without a result line and without an error of its own.
    fn ended(&mut self, status: Option<ExitStatus>, stderr: &str) {
        self.release();
        if self.saw_final || self.failed {
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
        self.failed = true;
        self.out.error(kind, message);
    }
}

/// The CLI confirmed the session: from now on turns resume it. Stored in the review file.
async fn remember_session(shared: &Shared, pr: &PrRef, id: &str) {
    let _guard = reviews::lock(shared, pr).await;
    if let Ok(Some(mut review)) = reviews::load_stored(shared, pr)
        && review.harness_session.as_deref() != Some(id)
    {
        review.harness_session = Some(id.to_string());
        if reviews::save(shared, &review).is_err() {
            tracing::warn!(pr = %pr, "cannot store the agent session in the review");
        }
    }
    shared.sessions.session_confirmed(pr);
}

/// Waits for one of the `MAX_CONCURRENT_TURNS` places; `None` when stopped while waiting.
async fn wait_for_place(shared: &Shared, pr: &PrRef, stop: &Stop) -> Option<OwnedSemaphorePermit> {
    let permits = shared.sessions.permits.clone();
    if let Ok(permit) = permits.clone().try_acquire_owned() {
        return Some(permit);
    }
    announce(shared, pr, SessionStateKind::Queued);
    tokio::select! {
        permit = permits.acquire_owned() => permit.ok(),
        _ = stop.notify.notified() => None,
    }
}

async fn run_turn(shared: Arc<Shared>, pr: PrRef, turn: u64, prompt: Prompt, stop: Arc<Stop>) {
    let mut out = Out::new(&shared, &pr, turn);
    let harness = shared.config.read().await.harness.clone();
    let Some(_place) = wait_for_place(&shared, &pr, &stop).await else {
        out.error(
            AgentErrorKind::Interrupted,
            stop.why().message().to_string(),
        );
        return;
    };
    let extra_args = match harness.extra_args_list() {
        Ok(args) => args,
        Err(message) => {
            out.error(
                AgentErrorKind::Crashed,
                format!("Config › Harness: {message}"),
            );
            return;
        }
    };
    let cwd = shared.paths.worktree_for(&pr);
    let spec = TurnSpec {
        program: program_path(&shared, &harness),
        prompt: prompt.text,
        cwd: cwd.clone(),
        session: shared.sessions.session_for(&shared, &pr),
        use_cli_permissions: harness.use_cli_permissions,
        extra_args,
        base_env: std::env::vars_os().collect(),
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
            out.error(kind, kind.default_message().to_string());
            return;
        }
        Err(e) => {
            let kind = AgentErrorKind::Crashed;
            out.error(kind, format!("{}: {e}", kind.default_message()));
            return;
        }
    };
    announce(&shared, &pr, SessionStateKind::Running);

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = tokio::spawn(read_tail(child.stderr.take()));
    let mut lines = BufReader::new(stdout).lines();
    let mut parse = ParseState::with_root(cwd);
    let mut run = Run {
        shared: &shared,
        out,
        filter: StreamFilter::default(),
        saw_final: false,
        failed: false,
        streamed: false,
    };
    let limit = Duration::from_secs(u64::from(harness.turn_timeout_secs));
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let started = Instant::now();
    let ending = loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    for event in parse_line(&line, &mut parse) {
                        run.handle(event).await;
                    }
                }
                _ => break Ending::Eof,
            },
            _ = stop.notify.notified() => break Ending::Stopped,
            _ = &mut deadline => break Ending::TimedOut,
        }
    };
    let tail = |task: tokio::task::JoinHandle<String>| async move {
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
    };
    match ending {
        Ending::Eof => {
            let status = match tokio::time::timeout(KILL_GRACE, child.wait()).await {
                Ok(status) => status.ok(),
                Err(_) => {
                    terminate(&mut child).await;
                    None
                }
            };
            run.ended(status, &tail(stderr).await);
        }
        Ending::Stopped => {
            terminate(&mut child).await;
            run.release();
            run.out.error(
                AgentErrorKind::Interrupted,
                stop.why().message().to_string(),
            );
        }
        Ending::TimedOut => {
            terminate(&mut child).await;
            run.release();
            run.out.error(
                AgentErrorKind::Interrupted,
                format!(
                    "The turn ran past its limit of {}s and was stopped",
                    limit.as_secs()
                ),
            );
        }
    }
    run.out.flush();
    tracing::debug!(pr = %pr, turn, elapsed_ms = started.elapsed().as_millis() as u64, "agent turn ended");
}

/// Closes the turns a stopped daemon left without an ending, so a replayed chat never shows one
/// that runs forever. The sessions themselves are kept: the next turn resumes them.
pub(crate) fn close_unfinished(shared: &Shared) {
    let Ok(files) = std::fs::read_dir(shared.paths.agent_dir()) else {
        return;
    };
    for path in files.flatten().map(|file| file.path()) {
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        for turn in agent_log::unfinished_turns(&agent_log::read(&path)) {
            let entry = AgentLogEntry::Error {
                at: now_unix(),
                turn,
                kind: AgentErrorKind::Interrupted,
                message: Why::Shutdown.message().to_string(),
            };
            if let Err(e) = agent_log::append(&path, &entry) {
                tracing::warn!(error = %e, file = %path.display(), "cannot close an unfinished turn");
            }
        }
    }
}

pub(crate) async fn send(shared: &Arc<Shared>, pr: &PrRef, text: &str) -> Outcome {
    let text = text.trim();
    if text.is_empty() {
        return Outcome::Err(ProtocolError::new(
            ErrorCode::BadRequest,
            "the message is empty",
        ));
    }
    let prompt = Prompt {
        text: text.to_string(),
        shown: true,
    };
    match Sessions::submit(shared, pr, prompt).await {
        Ok(turn) => Outcome::Ok(Reply::AgentTurn { turn }),
        Err(refusal) => refusal.into_outcome(),
    }
}

/// The chat to replay: every entry, except the suggestions already accepted or dismissed, so a
/// window that opens later shows only the ones still waiting.
pub(crate) async fn log(shared: &Shared, pr: &PrRef) -> Outcome {
    let path = shared.paths.agent_log(pr);
    let handled = clusia_store::agent::load_agent_state(&shared.paths, pr).unwrap_or_default();
    match tokio::task::spawn_blocking(move || agent_log::read(&path)).await {
        Ok(entries) => Outcome::Ok(Reply::AgentLog(
            entries
                .into_iter()
                .filter(|entry| match entry {
                    AgentLogEntry::Suggestion { suggestion, .. } => {
                        !handled.accepted.contains(&suggestion.id)
                            && !handled.dismissed.contains(&suggestion.id)
                    }
                    _ => true,
                })
                .collect(),
        )),
        Err(e) => Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("cannot read the agent log: {e}"),
        )),
    }
}

/// Asks the program for its version: the Config › Harness probe card.
pub(crate) async fn probe(shared: &Shared) -> Outcome {
    let harness = shared.config.read().await.harness.clone();
    let program = program_path(shared, &harness);
    let started = Instant::now();
    let env: Vec<_> = std::env::vars_os().collect();
    let mut command = Command::from(probe_command(&program, &env));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let found: Result<String, String> =
        match tokio::time::timeout(PROBE_LIMIT, command.output()).await {
            Err(_) => Err(format!(
                "Claude Code did not answer within {} seconds",
                PROBE_LIMIT.as_secs()
            )),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(AgentErrorKind::NotInstalled.default_message().to_string())
            }
            Ok(Err(e)) => Err(e.to_string()),
            Ok(Ok(output)) if output.status.success() => {
                parse_probe(&String::from_utf8_lossy(&output.stdout))
            }
            Ok(Ok(output)) => {
                let said = String::from_utf8_lossy(&output.stderr);
                Err(match said.trim() {
                    "" => format!("Claude Code exited with {}", output.status),
                    said => said.to_string(),
                })
            }
        };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let program = program.display().to_string();
    Outcome::Ok(Reply::Probe(match found {
        Ok(version) => ProbeResult {
            ok: true,
            version: Some(version),
            program,
            elapsed_ms,
            error: None,
        },
        Err(error) => ProbeResult {
            ok: false,
            version: None,
            program,
            elapsed_ms,
            error: Some(error),
        },
    }))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clusia_core::{Config, Paths, Review};
    use clusia_harness::testkit::{FakeClaude, Script, Turn};
    use clusia_protocol::AgentLogEntry;
    use clusia_store::save_review;
    use tokio::sync::broadcast;

    use super::*;

    fn shared_for(paths: Paths, config: Config) -> Arc<Shared> {
        let options = crate::options::DaemonOptions {
            claude_program: Some("/opt/option/claude".into()),
            github_api: Some("http://127.0.0.1:9".into()),
            github_token: None,
            gh_program: "/nonexistent/gh".into(),
            secrets: Arc::new(clusia_platform::MemoryStore::default()),
            background_sync: false,
            tray_program: None,
            spawner: Arc::new(crate::spawner::RecordingSpawner::default()),
            media_extra_hosts: Vec::new(),
            media_allow_local: false,
            media_resolve: Vec::new(),
            giphy_api: Some("http://127.0.0.1:9".into()),
            harness_search_paths: Vec::new(),
        };
        Arc::new(Shared::new(paths, config, options))
    }

    type Events = broadcast::Receiver<(String, Event)>;

    struct Lab {
        home: tempfile::TempDir,
        fake: tempfile::TempDir,
        shared: Arc<Shared>,
        events: Events,
    }

    fn pr(n: u64) -> PrRef {
        format!("acme/widgets#{n}").parse().unwrap()
    }

    fn lab(script: Script) -> Lab {
        lab_with(script, |_| {})
    }

    fn lab_with(script: Script, tweak: impl FnOnce(&mut Config)) -> Lab {
        let home = tempfile::tempdir().unwrap();
        let fake = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(fake.path(), script);
        let mut config = Config::default();
        config.harness.program = Some(program.display().to_string());
        tweak(&mut config);
        let shared = shared_for(Paths::new(home.path()), config);
        let events = shared.events.subscribe();
        let lab = Lab {
            home,
            fake,
            shared,
            events,
        };
        lab.add_review(&pr(7));
        lab
    }

    impl Lab {
        fn add_review(&self, pr: &PrRef) {
            let review = Review::new(pr.clone(), "t".into(), "b".into(), "h".into(), 1);
            save_review(&self.shared.paths, &review).unwrap();
            std::fs::create_dir_all(self.shared.paths.worktree_for(pr)).unwrap();
        }

        async fn send(&self, pr: &PrRef, text: &str) -> Result<u64, Refusal> {
            let prompt = Prompt {
                text: text.into(),
                shown: true,
            };
            Sessions::submit(&self.shared, pr, prompt).await
        }

        fn calls(&self) -> Vec<clusia_harness::testkit::Call> {
            FakeClaude::calls(self.fake.path())
        }

        fn log(&self, pr: &PrRef) -> Vec<AgentLogEntry> {
            agent_log::read(&self.shared.paths.agent_log(pr))
        }
    }

    /// Waits for `count` runs of the fake without blocking the test's runtime thread.
    async fn wait_calls(lab: &Lab, count: usize) -> Vec<clusia_harness::testkit::Call> {
        let dir = lab.fake.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            FakeClaude::wait_for_calls(&dir, count, Duration::from_secs(5))
        })
        .await
        .unwrap()
    }

    /// The agent events up to and including the first one `last` accepts.
    async fn until(events: &mut Events, last: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut seen = Vec::new();
        loop {
            let (topic, event) = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("an agent event arrives")
                .expect("the channel stays open");
            if topic != topics::AGENT {
                continue;
            }
            let end = last(&event);
            seen.push(event);
            if end {
                return seen;
            }
        }
    }

    fn ready(pr: &PrRef) -> impl Fn(&Event) -> bool + '_ {
        move |e| matches!(e, Event::SessionState { pr: p, state: SessionStateKind::Ready } if p == pr)
    }

    fn state_is(pr: &PrRef, wanted: SessionStateKind) -> impl Fn(&Event) -> bool + '_ {
        move |e| matches!(e, Event::SessionState { pr: p, state } if p == pr && *state == wanted)
    }

    fn texts(events: &[Event]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                Event::AgentChunk { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn error_kind(events: &[Event]) -> Option<AgentErrorKind> {
        events.iter().find_map(|e| match e {
            Event::AgentError { kind, .. } => Some(*kind),
            _ => None,
        })
    }

    #[tokio::test]
    async fn a_turn_streams_logs_and_ends_ready() {
        let mut lab = lab(Script::one(Turn::answer("Looks fine.")));
        let pr = pr(7);
        let turn = lab.send(&pr, "What changed?").await.unwrap();
        assert_eq!(turn, 1);
        let events = until(&mut lab.events, ready(&pr)).await;
        assert!(matches!(
            &events[0],
            Event::SessionState {
                state: SessionStateKind::Running,
                ..
            }
        ));
        assert_eq!(texts(&events), "Looks fine.");
        assert!(events.iter().any(|e| matches!(
            e,
            Event::AgentDone {
                turn: 1,
                duration_ms: 5,
                ..
            }
        )));
        let log = lab.log(&pr);
        assert!(
            matches!(&log[0], AgentLogEntry::User { turn: 1, text, .. } if text == "What changed?")
        );
        assert!(matches!(&log[1], AgentLogEntry::Text { text, .. } if text == "Looks fine."));
        assert!(matches!(&log[2], AgentLogEntry::Done { turn: 1, .. }));
        let call = &lab.calls()[0];
        assert!(call.argv.contains(&"What changed?".to_string()));
        assert_eq!(
            call.cwd.canonicalize().unwrap(),
            lab.shared.paths.worktree_for(&pr).canonicalize().unwrap(),
            "the agent runs in the worktree"
        );
    }

    #[tokio::test]
    async fn the_first_turn_starts_the_session_and_the_next_resumes_it() {
        let mut lab = lab(Script::turns(vec![
            Turn::answer("one"),
            Turn::answer("two"),
        ]));
        let pr = pr(7);
        lab.send(&pr, "first").await.unwrap();
        until(&mut lab.events, ready(&pr)).await;
        lab.send(&pr, "second").await.unwrap();
        until(&mut lab.events, ready(&pr)).await;
        let calls = lab.calls();
        let flag_value = |argv: &[String], flag: &str| {
            let at = argv.iter().position(|a| a == flag)?;
            argv.get(at + 1).cloned()
        };
        let id = flag_value(&calls[0].argv, "--session-id").expect("a new session");
        assert_eq!(flag_value(&calls[1].argv, "--resume"), Some(id.clone()));
        assert_eq!(flag_value(&calls[1].argv, "--session-id"), None);
        let stored = reviews::load_stored(&lab.shared, &pr)
            .ok()
            .flatten()
            .unwrap();
        assert_eq!(stored.harness_session, Some(id));
    }

    #[tokio::test]
    async fn a_second_send_queues_and_a_third_is_refused() {
        let mut lab = lab(Script::one(Turn::hanging()));
        let pr = pr(7);
        assert_eq!(lab.send(&pr, "one").await, Ok(1));
        assert_eq!(lab.send(&pr, "two").await, Ok(2));
        let queued = until(&mut lab.events, state_is(&pr, SessionStateKind::Queued)).await;
        assert!(!queued.is_empty());
        wait_calls(&lab, 1).await;
        assert_eq!(lab.send(&pr, "three").await, Err(Refusal::Busy));
        assert_eq!(
            Refusal::Busy.into_outcome(),
            Outcome::Err(ProtocolError::new(
                ErrorCode::Busy,
                "The agent is busy: wait for the current turn"
            ))
        );
        lab.shared.sessions.cancel(&lab.shared, &pr);
        until(&mut lab.events, ready(&pr)).await;
        assert_eq!(lab.calls().len(), 1, "the queued turn never started");
    }

    #[tokio::test]
    async fn the_queued_turn_runs_after_the_first() {
        let mut lab = lab(Script::turns(vec![
            Turn::answer("first").delay_ms(150),
            Turn::answer("second"),
        ]));
        let pr = pr(7);
        lab.send(&pr, "one").await.unwrap();
        lab.send(&pr, "two").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        assert_eq!(texts(&events), "firstsecond");
        let dones: Vec<u64> = events
            .iter()
            .filter_map(|e| match e {
                Event::AgentDone { turn, .. } => Some(*turn),
                _ => None,
            })
            .collect();
        assert_eq!(dones, [1, 2]);
        let readies = events.iter().filter(|e| ready(&pr)(e)).count();
        assert_eq!(readies, 1, "Ready only once the queue is empty");
    }

    #[tokio::test]
    async fn stop_kills_the_turn() {
        let mut lab = lab(Script::one(Turn::hanging()));
        let pr = pr(7);
        lab.send(&pr, "think").await.unwrap();
        let call = wait_calls(&lab, 1).await.remove(0);
        assert!(FakeClaude::is_running(call.pid));
        lab.shared.sessions.cancel(&lab.shared, &pr);
        let events = until(&mut lab.events, ready(&pr)).await;
        assert_eq!(error_kind(&events), Some(AgentErrorKind::Interrupted));
        assert!(!FakeClaude::is_running(call.pid), "the process is gone");
        assert!(
            !events.iter().any(|e| matches!(e, Event::AgentDone { .. })),
            "a stopped turn is not done"
        );
        assert!(matches!(
            lab.log(&pr).last(),
            Some(AgentLogEntry::Error {
                kind: AgentErrorKind::Interrupted,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_program_that_ignores_sigterm_is_killed() {
        let mut lab = lab(Script::one(Turn::hanging().ignore_term()));
        let pr = pr(7);
        lab.send(&pr, "think").await.unwrap();
        let call = wait_calls(&lab, 1).await.remove(0);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let stopped_at = Instant::now();
        lab.shared.sessions.cancel(&lab.shared, &pr);
        until(&mut lab.events, ready(&pr)).await;
        assert!(stopped_at.elapsed() >= KILL_GRACE, "SIGTERM got its grace");
        assert!(!FakeClaude::is_running(call.pid));
    }

    #[tokio::test]
    async fn the_timeout_stops_the_turn() {
        let mut lab = lab_with(Script::one(Turn::hanging()), |c| {
            c.harness.turn_timeout_secs = 1;
        });
        let pr = pr(7);
        lab.send(&pr, "think").await.unwrap();
        let call = wait_calls(&lab, 1).await.remove(0);
        let events = until(&mut lab.events, ready(&pr)).await;
        let message = events.iter().find_map(|e| match e {
            Event::AgentError {
                kind: AgentErrorKind::Interrupted,
                message,
                ..
            } => Some(message.clone()),
            _ => None,
        });
        assert!(message.unwrap().contains("limit of 1s"));
        assert!(!FakeClaude::is_running(call.pid));
    }

    #[tokio::test]
    async fn crash_mid_stream_is_reported() {
        let mut lab = lab(Script::one(
            Turn::fixture("crash_mid_stream")
                .exit(1)
                .stderr("boom: out of memory\n"),
        ));
        let pr = pr(7);
        lab.send(&pr, "go").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        assert_eq!(error_kind(&events), Some(AgentErrorKind::Crashed));
        let said = events.iter().find_map(|e| match e {
            Event::AgentError { message, .. } => Some(message.clone()),
            _ => None,
        });
        assert!(said.unwrap().contains("boom: out of memory"));
        assert!(!events.iter().any(|e| matches!(e, Event::AgentDone { .. })));
        assert!(
            texts(&events).contains("Looking at the diff"),
            "what arrived before the crash stays"
        );
        assert_eq!(lab.shared.sessions.state(&pr), SessionStateKind::Ready);
    }

    #[tokio::test]
    async fn the_next_message_after_a_crash_still_works() {
        let mut lab = lab(Script::turns(vec![
            Turn::lines(&[]).exit(1),
            Turn::answer("back"),
        ]));
        let pr = pr(7);
        lab.send(&pr, "one").await.unwrap();
        until(&mut lab.events, ready(&pr)).await;
        lab.send(&pr, "two").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        assert_eq!(texts(&events), "back");
    }

    #[tokio::test]
    async fn a_missing_program_says_so() {
        let mut lab = lab_with(Script::one(Turn::answer("x")), |c| {
            c.harness.program = Some("/nonexistent/claude".into());
        });
        let pr = pr(7);
        lab.send(&pr, "hi").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        assert_eq!(error_kind(&events), Some(AgentErrorKind::NotInstalled));
        assert!(events.iter().any(|e| matches!(
            e,
            Event::AgentError { message, .. } if message == "Install Claude Code or set its path"
        )));
    }

    #[tokio::test]
    async fn not_signed_in_and_usage_limit_each_get_their_line() {
        let cases = [
            (
                Turn::lines(&[])
                    .exit(1)
                    .stderr("Not logged in · Please run /login\n"),
                AgentErrorKind::NotSignedIn,
            ),
            (
                Turn::lines(&[])
                    .exit(1)
                    .stderr("5-hour usage limit reached\n"),
                AgentErrorKind::UsageLimit,
            ),
        ];
        for (turn, kind) in cases {
            let mut lab = lab(Script::one(turn));
            let pr = pr(7);
            lab.send(&pr, "hi").await.unwrap();
            let events = until(&mut lab.events, ready(&pr)).await;
            assert_eq!(error_kind(&events), Some(kind));
        }
    }

    #[tokio::test]
    async fn shutdown_logs_the_turn_it_interrupts_and_the_one_that_waited() {
        let lab = lab(Script::one(Turn::hanging()));
        let pr = pr(7);
        lab.send(&pr, "a").await.unwrap();
        lab.send(&pr, "b").await.unwrap();
        wait_calls(&lab, 1).await;
        lab.shared.sessions.shutdown(&lab.shared).await;
        let mut interrupted: Vec<(u64, String)> = lab
            .log(&pr)
            .into_iter()
            .filter_map(|entry| match entry {
                AgentLogEntry::Error {
                    turn,
                    kind: AgentErrorKind::Interrupted,
                    message,
                    ..
                } => Some((turn, message)),
                _ => None,
            })
            .collect();
        interrupted.sort();
        assert_eq!(
            interrupted
                .iter()
                .map(|(turn, _)| *turn)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(
            interrupted
                .iter()
                .all(|(_, message)| message.starts_with("The daemon stopped")),
            "{interrupted:?}"
        );
    }

    #[tokio::test]
    async fn a_turn_a_stopped_daemon_left_unfinished_is_closed_as_interrupted() {
        let home = tempfile::tempdir().unwrap();
        let shared = shared_for(Paths::new(home.path()), Config::default());
        let pr = pr(7);
        let path = shared.paths.agent_log(&pr);
        let user = |turn| AgentLogEntry::User {
            at: 1,
            turn,
            text: "hi".into(),
        };
        agent_log::append(&path, &user(1)).unwrap();
        agent_log::append(
            &path,
            &AgentLogEntry::Done {
                at: 2,
                turn: 1,
                duration_ms: 1,
            },
        )
        .unwrap();
        agent_log::append(&path, &user(2)).unwrap();
        agent_log::append(
            &path,
            &AgentLogEntry::Text {
                at: 3,
                turn: 2,
                text: "par".into(),
            },
        )
        .unwrap();

        close_unfinished(&shared);

        let log = agent_log::read(&path);
        assert!(matches!(
            log.last(),
            Some(AgentLogEntry::Error {
                turn: 2,
                kind: AgentErrorKind::Interrupted,
                ..
            })
        ));
        let count = log.len();
        close_unfinished(&shared);
        assert_eq!(agent_log::read(&path).len(), count, "closed only once");
    }

    #[test]
    fn standard_error_is_classified() {
        assert_eq!(
            classify_stderr("Error: Not logged in"),
            Some(AgentErrorKind::NotSignedIn)
        );
        assert_eq!(
            classify_stderr("You hit the usage limit"),
            Some(AgentErrorKind::UsageLimit)
        );
        assert_eq!(classify_stderr("segmentation fault"), None);
    }

    #[tokio::test]
    async fn suggestions_leave_the_text_and_become_events() {
        let answer = "Two things.\n```clusia-suggestion\n{\"file\":\"src/a.rs\",\"line\":3,\"body\":\"Why?\"}\n```\nThat is all.\n";
        let mut lab = lab(Script::one(Turn::answer(answer)));
        let pr = pr(7);
        lab.send(&pr, "review").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        assert_eq!(texts(&events), "Two things.\nThat is all.\n");
        let suggestions: Vec<&Suggestion> = events
            .iter()
            .filter_map(|e| match e {
                Event::AgentSuggestion { suggestion, .. } => Some(suggestion),
                _ => None,
            })
            .collect();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].file, "src/a.rs");
        let suggested_before_done = events
            .iter()
            .position(|e| matches!(e, Event::AgentSuggestion { .. }))
            < events
                .iter()
                .position(|e| matches!(e, Event::AgentDone { .. }));
        assert!(suggested_before_done);
        assert!(
            lab.log(&pr)
                .iter()
                .any(|e| matches!(e, AgentLogEntry::Suggestion { .. }))
        );
    }

    #[tokio::test]
    async fn an_invalid_suggestion_stays_in_the_text() {
        let answer = "Maybe:\n```clusia-suggestion\n{not json}\n```\n";
        let mut lab = lab(Script::one(Turn::answer(answer)));
        let pr = pr(7);
        lab.send(&pr, "review").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        assert_eq!(texts(&events), answer);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::AgentSuggestion { .. }))
        );
    }

    #[tokio::test]
    async fn a_suggestion_dismissed_or_accepted_before_is_not_shown_again() {
        let answer =
            "```clusia-suggestion\n{\"file\":\"src/a.rs\",\"line\":3,\"body\":\"Why?\"}\n```\n";
        let mut lab = lab(Script::one(Turn::answer(answer)));
        let pr = pr(7);
        lab.send(&pr, "one").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        let id = events
            .iter()
            .find_map(|e| match e {
                Event::AgentSuggestion { suggestion, .. } => Some(suggestion.id.clone()),
                _ => None,
            })
            .unwrap();
        let mut state = clusia_store::agent::load_agent_state(&lab.shared.paths, &pr).unwrap();
        state.dismissed.insert(id);
        clusia_store::agent::save_agent_state(&lab.shared.paths, &pr, &state).unwrap();
        lab.send(&pr, "again").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::AgentSuggestion { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::AgentDone { turn: 2, .. }))
        );
    }

    #[tokio::test]
    async fn three_turns_run_at_once_and_the_fourth_waits() {
        let mut lab = lab(Script::one(Turn::hanging()));
        let all = [pr(7), pr(8), pr(9), pr(10)];
        for pr in &all[1..] {
            lab.add_review(pr);
        }
        for pr in &all {
            lab.send(pr, "go").await.unwrap();
        }
        let queued = until(&mut lab.events, |e| {
            matches!(
                e,
                Event::SessionState {
                    state: SessionStateKind::Queued,
                    ..
                }
            )
        })
        .await;
        let Some(Event::SessionState { pr: waiting, .. }) = queued.last() else {
            panic!("a review is waiting for a place");
        };
        let waiting = waiting.clone();
        wait_calls(&lab, 3).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(lab.calls().len(), MAX_CONCURRENT_TURNS, "the fourth waits");
        let running = all.iter().find(|pr| **pr != waiting).unwrap();
        lab.shared.sessions.cancel(&lab.shared, running);
        wait_calls(&lab, 4).await;
        lab.shared.sessions.shutdown(&lab.shared).await;
    }

    #[tokio::test]
    async fn shutdown_leaves_no_agent_process() {
        let lab = lab(Script::one(Turn::hanging().ignore_term()));
        lab.add_review(&pr(8));
        lab.send(&pr(7), "a").await.unwrap();
        lab.send(&pr(8), "b").await.unwrap();
        let calls = wait_calls(&lab, 2).await;
        lab.shared.sessions.shutdown(&lab.shared).await;
        for call in &calls {
            assert!(
                !FakeClaude::is_running(call.pid),
                "pid {} survived",
                call.pid
            );
        }
        assert_eq!(lab.send(&pr(7), "late").await, Err(Refusal::Stopping));
    }

    #[tokio::test]
    async fn no_review_no_turn() {
        let lab = lab(Script::one(Turn::answer("x")));
        let refused = lab.send(&pr(99), "hi").await;
        assert_eq!(
            refused,
            Err(Refusal::NoReview(
                "no review for acme/widgets#99; open it first".into()
            ))
        );
        std::fs::remove_dir_all(lab.shared.paths.worktree_for(&pr(7))).unwrap();
        assert!(matches!(
            lab.send(&pr(7), "hi").await,
            Err(Refusal::NoReview(_))
        ));
        assert!(lab.calls().is_empty());
    }

    #[tokio::test]
    async fn turn_ids_continue_from_the_log() {
        let lab = lab(Script::one(Turn::answer("x")));
        let pr = pr(7);
        agent_log::append(
            &lab.shared.paths.agent_log(&pr),
            &AgentLogEntry::Done {
                at: 1,
                turn: 5,
                duration_ms: 1,
            },
        )
        .unwrap();
        assert_eq!(lab.send(&pr, "hi").await, Ok(6));
    }

    #[tokio::test]
    async fn the_probe_reports_the_version_or_why_not() {
        let lab = lab(Script::one(Turn::answer("x")));
        let Outcome::Ok(Reply::Probe(found)) = probe(&lab.shared).await else {
            panic!("a probe reply");
        };
        assert!(found.ok, "{found:?}");
        assert_eq!(found.version.as_deref(), Some("2.1.294"));
        assert!(Path::new(&found.program).ends_with("claude"));
        assert!(lab.calls().is_empty(), "the probe is not a turn");

        let missing = lab_with(Script::one(Turn::answer("x")), |c| {
            c.harness.program = Some("/nonexistent/claude".into());
        });
        let Outcome::Ok(Reply::Probe(found)) = probe(&missing.shared).await else {
            panic!("a probe reply");
        };
        assert!(!found.ok);
        assert_eq!(
            found.error.as_deref(),
            Some("Install Claude Code or set its path")
        );
    }

    #[tokio::test]
    async fn the_log_command_replays_the_chat() {
        let mut lab = lab(Script::one(Turn::answer("hello")));
        let pr = pr(7);
        lab.send(&pr, "hi").await.unwrap();
        until(&mut lab.events, ready(&pr)).await;
        let Outcome::Ok(Reply::AgentLog(entries)) = log(&lab.shared, &pr).await else {
            panic!("a log reply");
        };
        assert_eq!(entries.len(), 3);
        assert!(matches!(&entries[0], AgentLogEntry::User { .. }));
    }

    #[tokio::test]
    async fn an_empty_message_is_refused() {
        let lab = lab(Script::one(Turn::answer("x")));
        let outcome = send(&lab.shared, &pr(7), "  \n").await;
        assert!(matches!(
            outcome,
            Outcome::Err(ProtocolError {
                code: ErrorCode::BadRequest,
                ..
            })
        ));
        assert!(lab.calls().is_empty());
        let _ = &lab.home;
    }

    #[tokio::test]
    async fn the_log_command_leaves_out_handled_suggestions() {
        let answer =
            "```clusia-suggestion\n{\"file\":\"src/a.rs\",\"line\":3,\"body\":\"Why?\"}\n```\n";
        let mut lab = lab(Script::one(Turn::answer(answer)));
        let pr = pr(7);
        lab.send(&pr, "go").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        let id = events
            .iter()
            .find_map(|e| match e {
                Event::AgentSuggestion { suggestion, .. } => Some(suggestion.id.clone()),
                _ => None,
            })
            .unwrap();
        let waiting = |outcome: Outcome| match outcome {
            Outcome::Ok(Reply::AgentLog(entries)) => entries
                .iter()
                .filter(|e| matches!(e, AgentLogEntry::Suggestion { .. }))
                .count(),
            other => panic!("a log reply, got {other:?}"),
        };
        assert_eq!(waiting(log(&lab.shared, &pr).await), 1);
        let mut state = clusia_store::agent::load_agent_state(&lab.shared.paths, &pr).unwrap();
        state.accepted.insert(id);
        clusia_store::agent::save_agent_state(&lab.shared.paths, &pr, &state).unwrap();
        assert_eq!(waiting(log(&lab.shared, &pr).await), 0);
    }

    #[test]
    fn the_program_comes_from_the_config_then_the_option_then_the_search() {
        let home = tempfile::tempdir().unwrap();
        let shared = shared_for(Paths::new(home.path()), Config::default());
        let mut harness = Harness::default();
        assert_eq!(
            program_path(&shared, &harness),
            Path::new("/opt/option/claude"),
            "no setting: the daemon's own override"
        );
        harness.program = Some(" /usr/local/bin/claude ".into());
        assert_eq!(
            program_path(&shared, &harness),
            Path::new("/usr/local/bin/claude")
        );
        harness.program = Some("no-such-claude-anywhere".into());
        assert_eq!(
            program_path(&shared, &harness),
            Path::new("no-such-claude-anywhere"),
            "a name that is nowhere stays a bare name"
        );
    }
}
