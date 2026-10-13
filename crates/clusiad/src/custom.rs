//! A program of the user's own as the agent: one process per turn, spoken to in JSON lines. The
//! turn line goes to its standard input, which stays open for the answers to its permission
//! requests; what it writes to standard output is read line by line within the protocol's
//! caps, and its end, by itself or by a stop, leaves no process of its group behind.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clusia_core::PrRef;
use clusia_core::config::Harness;
use clusia_core::permissions::inside_worktree;
use clusia_harness::custom::{
    self, AskKind, CustomErrorKind, CustomTurn, LineReader, Output, Overflow, SEND_LIMIT,
    STOP_GRACE,
};
use clusia_harness::{Agent, ROLE_PROMPT};
use clusia_protocol::{AgentErrorKind, TurnOrigin};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::task::JoinHandle;

use crate::permissions::{self, PermissionAsk};
use crate::state::Shared;
use crate::turns::{self, KILL_GRACE, Stop, TurnEnd, TurnSink, Why};

/// The most of a name or an id that a log line or an error keeps.
const SHOWN: usize = 120;

/// Where the program is: what Config › Harness holds, which must be an absolute path. A bare
/// name would be searched on the daemon's `PATH`, and a relative path would start something out of
/// the pull request's checkout, which is the program's working directory.
pub(crate) fn program_of(harness: &Harness) -> Result<PathBuf, String> {
    let program = harness
        .program
        .as_deref()
        .map(str::trim)
        .filter(|program| !program.is_empty())
        .ok_or_else(|| "Set the custom command's program in Config › Harness".to_string())?;
    if !Path::new(program).is_absolute() {
        return Err(format!(
            "{program} is not an absolute path. Set it in Config › Harness"
        ));
    }
    Ok(PathBuf::from(program))
}

/// `<program> <extra_args>` in `cwd`, with `base_env` minus what must not reach the program: the
/// GitHub tokens and every `CLUSIA_*`.
pub(crate) fn command(
    program: &Path,
    extra_args: &[String],
    cwd: &Path,
    base_env: &[(OsString, OsString)],
) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command.args(extra_args).current_dir(cwd);
    Agent::Custom.set_env(&mut command, base_env);
    command
}

/// `text` with no control character, cut to what a log line or an error keeps.
fn shown(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(SHOWN)
        .collect()
}

/// The pipe to the program's standard input, shared by the turn and by every answer task. A write
/// that the program does not read must not hold the lock for ever (a stop would wait for it), so
/// every write is bounded.
struct Link {
    stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
}

impl Link {
    fn new(stdin: ChildStdin) -> Arc<Self> {
        Arc::new(Self {
            stdin: Arc::new(tokio::sync::Mutex::new(Some(stdin))),
        })
    }

    /// Takes the pipe before anything else can write and sends `line` first, from a task of its
    /// own: a program that never reads must not hold the turn, and no answer can overtake the
    /// turn line because they all wait for the lock this task already holds.
    async fn send_first(&self, line: String) -> JoinHandle<()> {
        let mut stdin = self.stdin.clone().lock_owned().await;
        tokio::spawn(async move {
            let write = async {
                let Some(pipe) = stdin.as_mut() else {
                    return;
                };
                let _ = pipe.write_all(line.as_bytes()).await;
                let _ = pipe.flush().await;
            };
            if tokio::time::timeout(SEND_LIMIT, write).await.is_err() {
                // A cut write may have left half a line in the pipe.
                stdin.take();
            }
        })
    }

    /// Writes one line; an error says why it could not.
    async fn send(&self, line: &str) -> Result<(), String> {
        let write = async {
            let mut stdin = self.stdin.lock().await;
            let Some(stdin) = stdin.as_mut() else {
                return Err("the program no longer reads its input".to_string());
            };
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            stdin.flush().await.map_err(|e| e.to_string())
        };
        match tokio::time::timeout(SEND_LIMIT, write).await {
            Ok(sent) => sent,
            Err(_) => {
                // The cut write may have left half a line in the pipe, so nothing sent after it
                // can be trusted.
                self.close().await;
                Err("the program does not read its input".to_string())
            }
        }
    }

    /// Closes the program's input: what it still has to read ends there.
    async fn close(&self) {
        self.stdin.lock().await.take();
    }
}

/// `path` as the worktree `root` shows it: relative when it lies under it.
fn relative_to(path: &str, root: &Path) -> String {
    let path = Path::new(path);
    let inside = path.strip_prefix(root).ok().or_else(|| {
        let root = root.canonicalize().ok()?;
        path.strip_prefix(root).ok()
    });
    inside.unwrap_or(path).display().to_string()
}

/// What a request of the program becomes for the reviewer: the tool and the input `ask` takes, or
/// the tool and the summary of a request that nobody is asked about.
fn judge(
    root: &Path,
    kind: AskKind,
    detail: &str,
    reason: Option<String>,
    paths: &[String],
) -> Result<(&'static str, Value), (&'static str, String)> {
    match kind {
        AskKind::Command if detail.trim().is_empty() => {
            Err(("Bash", "a command with no text".to_string()))
        }
        AskKind::Command => Ok(("Bash", json!({ "command": detail, "description": reason }))),
        AskKind::Edit => {
            if paths.is_empty() {
                return Err(("Edit", "an edit with no listed files".to_string()));
            }
            if let Some(outside) = paths
                .iter()
                .find(|path| !inside_worktree(Path::new(path), root))
            {
                return Err(("Edit", shown(outside)));
            }
            let shown_paths: Vec<String> =
                paths.iter().map(|path| relative_to(path, root)).collect();
            Ok((
                "Edit",
                json!({
                    "file_path": shown_paths[0],
                    "paths": shown_paths,
                    "description": reason,
                }),
            ))
        }
        AskKind::Other => Ok(("Other", json!({ "detail": detail, "description": reason }))),
        AskKind::Unknown(kind) => Err(("Other", format!("a request of kind {}", shown(&kind)))),
    }
}

/// One request of the program, put to the reviewer through the same model as every other request
/// of the agent: the rules of the review, the deadline, the window and the notification.
struct Approval {
    shared: Arc<Shared>,
    link: Arc<Link>,
    pr: PrRef,
    turn: u64,
    stop: Arc<Stop>,
    id: String,
}

impl Approval {
    /// Puts the request to the reviewer and answers the program. Only an answer from the
    /// reviewer or a rule of the review allows: an error, a timeout and a stop all deny.
    async fn run(self, tool: &'static str, input: Value) {
        let asked = permissions::ask(
            &self.shared,
            PermissionAsk {
                pr: self.pr.clone(),
                turn: self.turn,
                tool: tool.to_string(),
                input,
            },
        )
        .await;
        let allow = matches!(&asked, Ok(decision) if decision.allow) && !self.stop.fired();
        let _ = self.link.send(&custom::reply_line(&self.id, allow)).await;
    }
}

/// Judges one request in the read loop and starts the task that answers it. A request with no id
/// or an id seen before is refused and gets no answer: an id is answered once. Any other refused
/// request gets one `deny`.
#[allow(clippy::too_many_arguments)]
fn on_permission(
    shared: &Arc<Shared>,
    link: &Arc<Link>,
    pr: &PrRef,
    turn: u64,
    stop: &Arc<Stop>,
    seen: &mut HashSet<String>,
    id: String,
    kind: AskKind,
    detail: String,
    reason: Option<String>,
    paths: Vec<String>,
) -> Option<JoinHandle<()>> {
    if id.is_empty() {
        permissions::refuse(shared, pr, turn, "Other", "a request with no id");
        return None;
    }
    if !seen.insert(id.clone()) {
        let summary = format!("a second request with the id {}", shown(&id));
        permissions::refuse(shared, pr, turn, "Other", &summary);
        return None;
    }
    let root = shared.paths.worktree_for(pr);
    match judge(&root, kind, &detail, reason, &paths) {
        Ok((tool, input)) => {
            let approval = Approval {
                shared: shared.clone(),
                link: link.clone(),
                pr: pr.clone(),
                turn,
                stop: stop.clone(),
                id,
            };
            Some(tokio::spawn(approval.run(tool, input)))
        }
        Err((tool, summary)) => {
            permissions::refuse(shared, pr, turn, tool, &summary);
            let link = link.clone();
            Some(tokio::spawn(async move {
                let _ = link.send(&custom::reply_line(&id, false)).await;
            }))
        }
    }
}

/// Waits for the answers in `asked` to go out, each for a moment at most.
async fn settle_answers(asked: &mut Vec<JoinHandle<()>>) {
    for mut task in asked.drain(..) {
        if tokio::time::timeout(Duration::from_secs(1), &mut task)
            .await
            .is_err()
        {
            // Left running it could still answer after the end of the turn.
            task.abort();
            tracing::debug!("a permission answer did not finish");
        }
    }
}

/// A program of the turn, owned until it is reaped: a turn that is dropped before that takes the
/// whole group with it instead of leaving what the program started behind.
struct Held {
    child: Child,
    /// Once reaped its pid, and so the group id, may be reused, so it is never signalled again.
    reaped: bool,
}

impl Drop for Held {
    fn drop(&mut self) {
        if !self.reaped {
            turns::signal_group(self.child.id(), libc::SIGKILL);
        }
    }
}

/// Whether the turn was stopped because the daemon or the review is going away: nobody is left
/// to wait for the program, so its grace shrinks to `KILL_GRACE`.
fn hurried(stop: &Stop) -> bool {
    stop.fired() && matches!(stop.why(), Why::Shutdown | Why::Ended)
}

/// Waits up to `grace` for the program to end, leaving it unreaped. A stop for a reason that
/// `hurried` names cuts what is left of the wait to `KILL_GRACE`, whenever it comes.
async fn wait_exit(pid: u32, grace: Duration, stop: &Stop) -> bool {
    let mut deadline = Instant::now() + grace;
    let mut cut = false;
    loop {
        if turns::has_exited(pid) {
            return true;
        }
        if !cut && hurried(stop) {
            cut = true;
            deadline = deadline.min(Instant::now() + KILL_GRACE);
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// SIGTERM to the program's group, SIGKILL to what is left of it after `grace`, then reaps it.
async fn end_group(held: &mut Held, grace: Duration, stop: &Stop) {
    let pid = held.child.id();
    turns::signal_group(pid, libc::SIGTERM);
    if let Some(pid) = pid {
        wait_exit(pid, grace, stop).await;
    }
    turns::sweep_and_reap(&mut held.child, pid).await;
    held.reaped = true;
}

/// Waits up to `STOP_GRACE` for the program to leave on its own, ends it if it does not, and
/// reaps it with whatever its group still holds.
async fn leave(held: &mut Held, stop: &Stop) {
    let pid = held.child.id();
    match pid {
        Some(id) if !wait_exit(id, STOP_GRACE, stop).await => {
            end_group(held, STOP_GRACE, stop).await
        }
        _ => {
            turns::sweep_and_reap(&mut held.child, pid).await;
            held.reaped = true;
        }
    }
}

#[derive(Clone, Copy)]
enum Ending {
    /// A `final` or an `error` line arrived.
    Ended,
    Eof,
    Overflow,
    Stopped,
    TimedOut,
}

/// Runs one turn on the custom program: starts it, writes the turn line, hands what it says to
/// `sink` until it ends the turn, stops, runs past `limit` or writes more than the caps allow.
/// No process of the turn outlives this call, and whatever permission it still asked for is
/// answered `deny` while the program can still read it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_turn(
    shared: &Arc<Shared>,
    pr: &PrRef,
    turn: u64,
    harness: &Harness,
    name: String,
    prompt: String,
    session_id: Option<String>,
    limit: Option<Duration>,
    stop: &Arc<Stop>,
    sink: &mut dyn TurnSink,
) -> TurnEnd {
    // The program has no sandbox, so a request of this turn is shown and judged as unsandboxed.
    shared.sessions.set_turn_sandbox(pr, turn, false);
    let program = match program_of(harness) {
        Ok(program) => program,
        Err(message) => {
            sink.error(AgentErrorKind::NotInstalled, message);
            return TurnEnd::Failed;
        }
    };
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
    let mut base_env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    base_env.extend(shared.harness_env.iter().cloned());
    let mut spawn = Command::from(command(&program, &extra_args, &cwd, &base_env));
    spawn
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let child = match spawn.spawn() {
        Ok(child) => child,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            sink.error(
                AgentErrorKind::NotInstalled,
                format!(
                    "{} was not found. Set it in Config › Harness",
                    program.display()
                ),
            );
            return TurnEnd::Failed;
        }
        Err(e) => {
            sink.error(AgentErrorKind::Crashed, format!("{name}: {e}"));
            return TurnEnd::Failed;
        }
    };
    sink.started();
    let mut held = Held {
        child,
        reaped: false,
    };
    let child = &mut held.child;

    let link = Link::new(child.stdin.take().expect("stdin is piped"));
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = tokio::spawn(turns::read_tail(child.stderr.take()));
    let limit = limit.unwrap_or_else(|| Duration::from_secs(u64::from(harness.turn_timeout_secs)));
    let first = custom::turn_line(
        &prompt,
        ROLE_PROMPT,
        session_id.as_deref(),
        &cwd,
        limit.as_secs(),
    );
    let mut asked: Vec<JoinHandle<()>> = vec![link.send_first(first).await];

    let chat = shared.sessions.origin_of(pr, turn) == Some(TurnOrigin::Chat);
    let mut lines = LineReader::new(BufReader::new(stdout));
    let mut fold = CustomTurn::new(name.clone(), Instant::now());
    let mut seen: HashSet<String> = HashSet::new();
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let began = Instant::now();
    let ending = loop {
        tokio::select! {
            read = lines.next() => match read {
                Ok(Some(line)) => {
                    let out = match custom::read_line(&line) {
                        Output::Permission { id, kind, detail, reason, paths } => {
                            asked.extend(on_permission(
                                shared, &link, pr, turn, stop, &mut seen, id, kind, detail, reason, paths,
                            ));
                            continue;
                        }
                        out => out,
                    };
                    if chat && matches!(out, Output::Error { kind: CustomErrorKind::Session, .. }) {
                        crate::sessions::forget_session(shared, pr).await;
                    }
                    for event in fold.on(out) {
                        sink.event(event).await;
                    }
                    if fold.ended() {
                        break Ending::Ended;
                    }
                }
                Ok(None) => break Ending::Eof,
                Err(Overflow::Line | Overflow::Lines | Overflow::Bytes) => break Ending::Overflow,
            },
            _ = stop.notified() => break Ending::Stopped,
            _ = &mut deadline => break Ending::TimedOut,
        }
    };
    // Nothing more is read: a program that goes on writing after its last line gets a closed pipe
    // instead of a full one.
    drop(lines);
    // The turn is over for the program: nobody is left to hear an answer, so what it still asks
    // is denied now, on the pipe, before the pipe closes.
    permissions::cancel_turn(shared, pr, turn).await;
    settle_answers(&mut asked).await;
    link.close().await;

    let (said_result, had_failed) = (sink.saw_final(), sink.failed());
    match ending {
        Ending::Ended => {
            leave(&mut held, stop).await;
            sink.release();
        }
        Ending::Eof => {
            leave(&mut held, stop).await;
            sink.release();
            if !said_result && !had_failed {
                let tail = tokio::time::timeout(Duration::from_secs(1), &mut stderr)
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default();
                let said = tail
                    .lines()
                    .rev()
                    .map(str::trim)
                    .find(|line| !line.is_empty());
                if fold.seen_json() {
                    let message = match said {
                        Some(said) => format!("{name} stopped unexpectedly: {said}"),
                        None => format!("{name} stopped unexpectedly"),
                    };
                    sink.error(AgentErrorKind::Crashed, message);
                } else {
                    sink.error(
                        AgentErrorKind::Unparsed,
                        format!("{name} answered in a format Clúsia does not know"),
                    );
                }
            }
        }
        Ending::Overflow => {
            // Said first: the chat shows the error at once, not after the program is ended.
            sink.release();
            sink.error(
                AgentErrorKind::Crashed,
                format!("{name} wrote more than Clúsia reads in one turn"),
            );
            end_group(&mut held, STOP_GRACE, stop).await;
        }
        Ending::Stopped => {
            end_group(&mut held, STOP_GRACE, stop).await;
            sink.release();
            if !said_result && !had_failed {
                sink.error(
                    AgentErrorKind::Interrupted,
                    stop.why().message().to_string(),
                );
            }
        }
        Ending::TimedOut if said_result || had_failed => {
            end_group(&mut held, STOP_GRACE, stop).await;
            sink.release();
        }
        Ending::TimedOut => {
            end_group(&mut held, STOP_GRACE, stop).await;
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
    stderr.abort();
    sink.flush();
    tracing::debug!(pr = %pr, turn, elapsed_ms = began.elapsed().as_millis() as u64, "custom turn ended");
    match (said_result, had_failed, ending) {
        (true, false, _) => TurnEnd::Done,
        (_, true, _) | (_, _, Ending::Eof | Ending::Overflow | Ending::Ended) => TurnEnd::Failed,
        (_, _, Ending::Stopped) => TurnEnd::Stopped(stop.why()),
        (_, _, Ending::TimedOut) => TurnEnd::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use clusia_core::config::HarnessKind;
    use clusia_core::{Config, Paths, Review};
    use clusia_harness::AgentEvent;
    use clusia_harness::testkit::{CustomScript, CustomTurnScript, FakeCustom};
    use clusia_store::save_review;
    use tokio::sync::Semaphore;

    use super::*;
    use crate::turns::{Boxed, SessionSource, TurnRun};

    fn pr() -> PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    fn turn() -> CustomTurnScript {
        CustomTurnScript::new()
    }

    fn shared_for(paths: Paths, config: Config) -> Arc<Shared> {
        let options = crate::options::DaemonOptions {
            claude_program: Some("/nonexistent/claude".into()),
            codex_program: Some("/nonexistent/codex".into()),
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
            bridge_program: None,
            harness_env: Vec::new(),
        };
        Arc::new(Shared::new(paths, config, options))
    }

    struct Lab {
        _home: tempfile::TempDir,
        fake: tempfile::TempDir,
        shared: Arc<Shared>,
    }

    fn lab(script: CustomScript, tweak: impl FnOnce(&mut Config)) -> Lab {
        let home = tempfile::tempdir().unwrap();
        let fake = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(fake.path(), script);
        let mut config = Config::default();
        config.harness.kind = HarnessKind::Custom;
        config.harness.program = Some(program.display().to_string());
        config.harness.custom_name = "Echo".into();
        tweak(&mut config);
        let shared = shared_for(Paths::new(home.path()), config);
        let review = Review::new(pr(), "t".into(), "b".into(), "h".into(), 1);
        save_review(&shared.paths, &review).unwrap();
        std::fs::create_dir_all(shared.paths.worktree_for(&pr())).unwrap();
        Lab {
            _home: home,
            fake,
            shared,
        }
    }

    /// Remembers what a run that is not the chat was told.
    #[derive(Default)]
    struct Recorder {
        texts: String,
        errors: Vec<(AgentErrorKind, String)>,
        saw_final: bool,
        failed: bool,
    }

    impl TurnSink for Recorder {
        fn waiting(&mut self) {}

        fn started(&mut self) {}

        fn event<'a>(&'a mut self, event: AgentEvent) -> Boxed<'a> {
            Box::pin(async move {
                match event {
                    AgentEvent::Text(text) => self.texts.push_str(&text),
                    AgentEvent::Final { is_error, .. } => {
                        self.saw_final = true;
                        self.failed |= is_error;
                    }
                    AgentEvent::Error { kind, message } => {
                        self.failed = true;
                        self.errors.push((kind, message));
                    }
                    _ => {}
                }
            })
        }

        fn error(&mut self, kind: AgentErrorKind, message: String) {
            self.failed = true;
            self.errors.push((kind, message));
        }

        fn release(&mut self) {}

        fn flush(&mut self) {}

        fn saw_final(&self) -> bool {
            self.saw_final
        }

        fn failed(&self) -> bool {
            self.failed
        }
    }

    /// Runs a security check turn through `turns::run`, so the daemon knows the turn while it
    /// runs and its permission requests are accepted.
    fn run_check(
        lab: &Lab,
        limit: Option<Duration>,
    ) -> tokio::task::JoinHandle<(TurnEnd, Recorder)> {
        let shared = lab.shared.clone();
        let run = TurnRun {
            pr: pr(),
            turn: 1,
            prompt: "Check this pull request.".into(),
            fresh_prompt: Some("Read .clusia/review.md first. Check this pull request.".into()),
            session: SessionSource::Fork,
            origin: TurnOrigin::Security,
            places: Arc::new(Semaphore::new(1)),
            limit,
            stop: Stop::new(),
        };
        tokio::spawn(async move {
            let mut sink = Recorder::default();
            let end = turns::run(&shared, run, &mut sink).await;
            (end, sink)
        })
    }

    async fn pid_of(lab: &Lab) -> u32 {
        let dir = lab.fake.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            FakeCustom::wait_for_calls(&dir, 1, Duration::from_secs(5))
                .remove(0)
                .pid
        })
        .await
        .unwrap()
    }

    fn group_alive(pid: u32) -> bool {
        // SAFETY: signal 0 only checks whether the group exists; nothing is sent.
        unsafe { libc::killpg(pid as i32, 0) == 0 }
    }

    #[test]
    fn the_program_must_be_set_and_an_absolute_path() {
        let with = |program: Option<&str>| Harness {
            kind: HarnessKind::Custom,
            program: program.map(str::to_string),
            ..Harness::default()
        };
        assert_eq!(
            program_of(&with(None)).unwrap_err(),
            "Set the custom command's program in Config › Harness"
        );
        assert_eq!(
            program_of(&with(Some("  "))).unwrap_err(),
            "Set the custom command's program in Config › Harness"
        );
        assert_eq!(
            program_of(&with(Some("./echo.py"))).unwrap_err(),
            "./echo.py is not an absolute path. Set it in Config › Harness"
        );
        assert_eq!(
            program_of(&with(Some(" /opt/echo.py "))).unwrap(),
            PathBuf::from("/opt/echo.py")
        );
        assert_eq!(
            program_of(&with(Some("echo-harness"))).unwrap_err(),
            "echo-harness is not an absolute path. Set it in Config › Harness",
            "a bare name would be searched on PATH"
        );
    }

    #[test]
    fn custom_env_has_no_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("program");
        std::fs::write(&program, "#!/bin/sh\nexec /usr/bin/env\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env: Vec<(OsString, OsString)> = [
            ("GH_TOKEN", "t1"),
            ("GITHUB_TOKEN", "t2"),
            ("GH_ENTERPRISE_TOKEN", "t3"),
            ("CLUSIA_HOME", "/x"),
            ("CLUSIA_GITHUB_TOKEN", "t4"),
            ("OPENAI_API_KEY", "mine"),
            ("MCP_TOOL_TIMEOUT", "7"),
            ("KEEP", "x"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let out = command(&program, &[], dir.path(), &env).output().unwrap();
        let seen = String::from_utf8_lossy(&out.stdout);
        for kept in ["KEEP=x", "OPENAI_API_KEY=mine", "MCP_TOOL_TIMEOUT=7"] {
            assert!(seen.contains(kept), "{kept} belongs to the program: {seen}");
        }
        for dropped in [
            "GH_TOKEN=",
            "GITHUB_TOKEN=",
            "GH_ENTERPRISE_TOKEN=",
            "CLUSIA_",
        ] {
            assert!(
                !seen.contains(dropped),
                "{dropped} reached the program: {seen}"
            );
        }
    }

    #[test]
    fn the_extra_arguments_follow_the_program() {
        let dir = tempfile::tempdir().unwrap();
        let args = vec!["--model".to_string(), "small".to_string()];
        let command = command(Path::new("/opt/p"), &args, dir.path(), &[]);
        let argv: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, ["--model", "small"]);
        assert_eq!(command.get_current_dir(), Some(dir.path()));
    }

    #[tokio::test]
    async fn custom_deadline_kills_the_turn() {
        let lab = lab(
            CustomScript::new().turn(turn().session("s1").delay_ms(60_000)),
            |_| {},
        );
        let task = run_check(&lab, Some(Duration::from_secs(1)));
        let pid = pid_of(&lab).await;
        let (end, sink) = task.await.unwrap();
        assert_eq!(end, TurnEnd::TimedOut);
        assert_eq!(
            sink.errors,
            [(
                AgentErrorKind::Interrupted,
                "The turn ran past its limit of 1s and was stopped".to_string()
            )]
        );
        assert!(!group_alive(pid), "the group is gone with the turn");
    }

    #[tokio::test]
    async fn custom_permission_timeout_denies() {
        let lab = lab(
            CustomScript::new().turn(
                turn()
                    .session("s1")
                    .ask_command("p1", "cargo test")
                    .final_("Done."),
            ),
            |config| config.harness.permission_timeout_secs = 1,
        );
        let (end, sink) = run_check(&lab, Some(Duration::from_secs(30)))
            .await
            .unwrap();
        assert_eq!(end, TurnEnd::Done, "{:?}", sink.errors);
        assert_eq!(
            FakeCustom::decisions(lab.fake.path()),
            [("p1".to_string(), "deny".to_string())],
            "nobody answered, so the clock did"
        );
    }

    #[tokio::test]
    async fn custom_oversized_line_is_one_error_not_a_hang() {
        let lab = lab(
            CustomScript::new().turn(turn().session("s1").oversized_line().hang()),
            |_| {},
        );
        let task = run_check(&lab, Some(Duration::from_secs(30)));
        let pid = pid_of(&lab).await;
        let (end, sink) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Failed);
        assert_eq!(
            sink.errors,
            [(
                AgentErrorKind::Crashed,
                "Echo wrote more than Clúsia reads in one turn".to_string()
            )]
        );
        assert!(
            !group_alive(pid),
            "the program that ignores SIGTERM is killed"
        );
    }

    #[tokio::test]
    async fn custom_garbage_without_json_is_unparsed_and_its_text_is_kept() {
        let lab = lab(CustomScript::new().turn(turn().garbage()), |_| {});
        let (end, sink) = run_check(&lab, Some(Duration::from_secs(30)))
            .await
            .unwrap();
        assert_eq!(end, TurnEnd::Failed);
        assert!(!sink.texts.is_empty(), "the lines are shown as text");
        assert_eq!(
            sink.errors,
            [(
                AgentErrorKind::Unparsed,
                "Echo answered in a format Clúsia does not know".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn custom_a_check_asks_the_reviewer_like_the_chat() {
        let lab = lab(
            CustomScript::new().turn(
                turn()
                    .session("s1")
                    .ask_other("p1", "Send a report")
                    .final_("Done."),
            ),
            |config| config.harness.permission_timeout_secs = 1,
        );
        let mut events = lab.shared.events.subscribe();
        let task = run_check(&lab, Some(Duration::from_secs(30)));
        let mut asked = None;
        while asked.is_none() {
            let (_, event) = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("an event arrives")
                .expect("the channel stays open");
            if let clusia_protocol::Event::PermissionRequested {
                tool,
                summary,
                origin,
                ..
            } = event
            {
                asked = Some((tool, summary, origin));
            }
        }
        assert_eq!(
            asked,
            Some((
                "Other".to_string(),
                "Send a report".to_string(),
                TurnOrigin::Security
            ))
        );
        task.await.unwrap();
    }
}
