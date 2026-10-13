//! `codex app-server`: one process per open review, which the chat and the checks of that
//! review share. Requests go out as JSON lines on its standard input and every line it writes
//! comes back through one reader task, which hands answers to the request that waits for them
//! and notifications to the turn that runs on the thread they name.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use clusia_core::PrRef;
use clusia_core::permissions::inside_worktree;
use clusia_harness::codex::{self, CodexTurn, Decision, Incoming, Note, ServerAsk};
use clusia_harness::{
    Agent, AgentEvent, CodexServerSpec, ROLE_PROMPT, SessionArg, codex_server_command,
};
use clusia_protocol::AgentErrorKind;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::permissions::{self, PermissionAsk};
use crate::state::Shared;
use crate::turns::{self, KILL_GRACE, Stop, TurnEnd, TurnSink, Why};

/// How long a server may sit with no turn before the sweep stops it.
pub(crate) const IDLE: Duration = Duration::from_secs(1800);
/// How often the sweep looks.
const SWEEP: Duration = Duration::from_secs(60);
/// How long an interrupted turn gets to say it ended before the process is killed.
const INTERRUPT_GRACE: Duration = Duration::from_secs(10);
/// How long a process gets to leave after its input closes before it is killed.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// How long a request waits for its answer.
const CALL_LIMIT: Duration = Duration::from_secs(60);
/// How long a write to the process may take before it counts as not reading its input.
const SEND_LIMIT: Duration = Duration::from_secs(10);
/// A line longer than this is not read: nothing Codex says is that long.
const MAX_LINE: usize = 16 * 1024 * 1024;
/// How much of the process's standard error is kept for the error line.
const STDERR_TAIL: usize = 2048;

/// What the reader hands to the turn that runs on a thread.
enum Routed {
    Note(Note),
    Ask {
        id: Value,
        ask: ServerAsk,
    },
    /// The process is gone.
    Closed,
}

/// The pipes of one process. The reader task and every request share it.
struct Link {
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    next_id: AtomicU64,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    routes: Mutex<HashMap<String, mpsc::UnboundedSender<Routed>>>,
    dead: AtomicBool,
}

impl Link {
    fn new(stdin: ChildStdin) -> Arc<Self> {
        Arc::new(Self {
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            next_id: AtomicU64::new(1),
            waiting: Mutex::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
        })
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Writes one line. A process that stops reading with its pipe full would hold the lock
    /// for ever, and with it every interrupt and kill, so the write is bounded.
    async fn send(&self, message: &Value) -> Result<(), String> {
        let write = async {
            let mut stdin = self.stdin.lock().await;
            let Some(stdin) = stdin.as_mut() else {
                return Err("Codex is not running".to_string());
            };
            stdin
                .write_all(codex::to_line(message).as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            stdin.flush().await.map_err(|e| e.to_string())
        };
        match tokio::time::timeout(SEND_LIMIT, write).await {
            Ok(sent) => sent,
            Err(_) => {
                // The cut write may have left half a line in the pipe, so nothing sent after it
                // can be trusted: the next lease starts a new server.
                self.close();
                Err("Codex does not read its input".to_string())
            }
        }
    }

    /// Sends the request `build` makes for a fresh id and waits for its answer.
    async fn call(&self, build: impl FnOnce(u64) -> Value) -> Result<Value, String> {
        let id = self.next_id();
        let (answer, wait) = oneshot::channel();
        self.waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, answer);
        if let Err(e) = self.send(&build(id)).await {
            self.waiting
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(CALL_LIMIT, wait).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("Codex stopped before it answered".to_string()),
            Err(_) => {
                self.waiting
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                Err("Codex did not answer in time".to_string())
            }
        }
    }

    /// Claims `thread` for one turn: what Codex says about it arrives on the receiver.
    fn route(self: &Arc<Self>, thread: &str) -> (Route, mpsc::UnboundedReceiver<Routed>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(thread.to_string(), sender);
        let route = Route {
            link: self.clone(),
            thread: thread.to_string(),
        };
        (route, receiver)
    }

    /// Gives `item` to the turn on `thread`; gives it back when no turn runs there.
    fn deliver(&self, thread: Option<&str>, item: Routed) -> Option<Routed> {
        let routes = self.routes.lock().unwrap_or_else(|p| p.into_inner());
        match thread.and_then(|thread| routes.get(thread)) {
            Some(sender) => sender.send(item).err().map(|failed| failed.0),
            None => Some(item),
        }
    }

    /// The process ended: nobody waits for an answer any more, and every turn is told.
    fn close(&self) {
        self.dead.store(true, Ordering::SeqCst);
        self.waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        for sender in self
            .routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            let _ = sender.send(Routed::Closed);
        }
    }
}

/// A turn's claim on a thread, released when it ends.
struct Route {
    link: Arc<Link>,
    thread: String,
}

impl Drop for Route {
    fn drop(&mut self) {
        self.link
            .routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.thread);
    }
}

/// Reads what Codex writes until it ends, and routes every line. A line that is not JSON, or
/// that says something this daemon does not know, is skipped.
async fn read_loop(stdout: ChildStdout, link: Arc<Link>) {
    let mut stdout = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match stdout.read_until(b'\n', &mut line).await {
            Ok(n) if n > 0 => {}
            _ => break,
        }
        if line.len() > MAX_LINE {
            tracing::debug!(
                bytes = line.len(),
                "skipped an app-server line that is too long"
            );
            continue;
        }
        let text = String::from_utf8_lossy(&line);
        match codex::read_line(text.trim_end_matches(['\n', '\r'])) {
            Incoming::Response { id, result } => {
                let waiter = link
                    .waiting
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(result);
                }
            }
            Incoming::Notification { thread_id, note } => {
                let _ = link.deliver(thread_id.as_deref(), Routed::Note(note));
            }
            Incoming::ServerRequest { id, thread_id, ask } => {
                if let Some(Routed::Ask { id, ask }) =
                    link.deliver(thread_id.as_deref(), Routed::Ask { id, ask })
                {
                    // No turn runs on that thread, so nobody can decide: nothing is allowed.
                    let _ = link.send(&refusal(&id, &ask)).await;
                }
            }
            Incoming::Ignored => {}
        }
    }
    link.close();
}

/// Keeps the end of what the process writes to standard error.
async fn collect_stderr(mut stderr: ChildStderr, tail: Arc<Mutex<Vec<u8>>>) {
    let mut buffer = [0u8; 1024];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut tail = tail.lock().unwrap_or_else(|p| p.into_inner());
                tail.extend_from_slice(&buffer[..n]);
                if tail.len() > STDERR_TAIL {
                    let extra = tail.len() - STDERR_TAIL;
                    tail.drain(..extra);
                }
            }
        }
    }
}

/// What a request of the server gets when nobody decides it: a command or a file change is
/// declined, and a request that has no decision to give gets a JSON-RPC error.
fn refusal(id: &Value, ask: &ServerAsk) -> Value {
    match ask {
        ServerAsk::Command { .. } | ServerAsk::FileChange { .. } => {
            codex::approval_reply(id, Decision::Decline)
        }
        ServerAsk::Other(method) => {
            codex::error_reply(id, -32601, &format!("Clúsia does not handle {method}"))
        }
    }
}

/// `path` as the worktree `root` shows it: relative when it lies under it.
fn relative_to(path: &str, root: &Path) -> String {
    let path = Path::new(path);
    let shown = path.strip_prefix(root).ok().or_else(|| {
        let root = root.canonicalize().ok()?;
        path.strip_prefix(root).ok()
    });
    shown.unwrap_or(path).display().to_string()
}

/// One approval request of the server, put to the reviewer through the same model as every
/// other request of the agent: the rules of the review, the deadline, the window and the
/// notification.
struct Approval {
    shared: Arc<Shared>,
    link: Arc<Link>,
    pr: PrRef,
    turn: u64,
    stop: Arc<Stop>,
    id: Value,
    ask: ServerAsk,
    /// The paths the file change touches, as Codex listed them.
    paths: Vec<String>,
}

impl Approval {
    /// Puts the request to the reviewer and answers the server. A command is a `Bash` request
    /// and a file change an `Edit`; only an answer from the reviewer or a rule of the review
    /// accepts, and Codex is never asked to remember a decision.
    async fn run(self) {
        let reply = match &self.ask {
            ServerAsk::Command {
                command,
                reason,
                cwd,
                ..
            } => {
                let root = self.shared.paths.worktree_for(&self.pr);
                let text = command.as_deref().unwrap_or_default();
                let cwd = cwd.as_deref().filter(|cwd| !cwd.is_empty());
                if text.trim().is_empty()
                    || cwd.is_some_and(|cwd| !inside_worktree(Path::new(cwd), &root))
                {
                    // Nobody can judge a command with no text, and a rule of the review must
                    // never cover one that runs outside the worktree, so nobody is asked.
                    permissions::refuse(&self.shared, &self.pr, self.turn, "Bash", text);
                    codex::approval_reply(&self.id, self.refused())
                } else {
                    let folder = cwd
                        .map(|cwd| relative_to(cwd, &root))
                        .filter(|folder| !folder.is_empty() && Path::new(folder).is_relative());
                    let description = match (folder, reason) {
                        (Some(folder), Some(reason)) => Some(format!("In {folder}: {reason}")),
                        (Some(folder), None) => Some(format!("In {folder}")),
                        (None, reason) => reason.clone(),
                    };
                    let input = json!({ "command": text, "description": description });
                    codex::approval_reply(&self.id, self.decide("Bash", input).await)
                }
            }
            ServerAsk::FileChange {
                reason, grant_root, ..
            } => {
                let root = self.shared.paths.worktree_for(&self.pr);
                let shown: Vec<String> = self
                    .paths
                    .iter()
                    .map(|path| relative_to(path, &root))
                    .collect();
                let outside = shown
                    .iter()
                    .find(|path| !inside_worktree(Path::new(path), &root));
                if shown.is_empty() || grant_root.is_some() || outside.is_some() {
                    // Nobody can judge a change whose files are unknown, that asks to write
                    // under a root for the session or that reaches outside the worktree, so
                    // nobody is asked and nothing is accepted.
                    let summary = match (grant_root, outside, shown.first()) {
                        (Some(grant), _, _) => format!("write access under {grant}"),
                        (None, Some(path), _) | (None, None, Some(path)) => path.clone(),
                        (None, None, None) => "a file change with no listed files".to_string(),
                    };
                    permissions::refuse(&self.shared, &self.pr, self.turn, "Edit", &summary);
                    codex::approval_reply(&self.id, self.refused())
                } else {
                    let checked = shown[0].clone();
                    let input = json!({
                        "file_path": checked,
                        "paths": shown,
                        "description": reason,
                    });
                    codex::approval_reply(&self.id, self.decide("Edit", input).await)
                }
            }
            other => refusal(&self.id, other),
        };
        let _ = self.link.send(&reply).await;
    }

    /// The answer to a request nobody was asked about.
    fn refused(&self) -> Decision {
        if self.stop.fired() {
            Decision::Cancel
        } else {
            Decision::Decline
        }
    }

    async fn decide(&self, tool: &str, input: Value) -> Decision {
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
        match asked {
            // A stop ends the turn, not just this request.
            _ if self.stop.fired() => Decision::Cancel,
            Ok(decision) if decision.allow => Decision::Accept,
            _ => Decision::Decline,
        }
    }
}

/// Why a server could not be started.
enum StartError {
    NotInstalled,
    Failed(String),
}

/// What it takes to start a server.
struct Start {
    program: PathBuf,
    cwd: PathBuf,
    extra_args: Vec<String>,
    base_env: Vec<(OsString, OsString)>,
}

/// One `codex app-server` process.
struct Server {
    link: Arc<Link>,
    /// `None` once the process was reaped: its group id may be reused, so it is never
    /// signalled again.
    child: tokio::sync::Mutex<Option<Child>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    /// The turns that hold the server now.
    active: AtomicUsize,
    last_used: Mutex<Instant>,
    /// The threads this process started, resumed or forked: it holds them, so they need no
    /// `thread/resume`.
    loaded: Mutex<HashSet<String>>,
}

impl Server {
    /// Starts the process and does the handshake.
    async fn start(start: Start) -> Result<Arc<Self>, StartError> {
        let mut command = Command::from(codex_server_command(&CodexServerSpec {
            program: start.program,
            cwd: start.cwd,
            extra_args: start.extra_args,
            base_env: start.base_env,
        }));
        command.kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StartError::NotInstalled
            } else {
                StartError::Failed(e.to_string())
            }
        })?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let tail = Arc::new(Mutex::new(Vec::new()));
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(collect_stderr(stderr, tail.clone()));
        }
        let link = Link::new(stdin);
        tokio::spawn(read_loop(stdout, link.clone()));
        let server = Arc::new(Self {
            link,
            child: tokio::sync::Mutex::new(Some(child)),
            stderr: tail,
            active: AtomicUsize::new(0),
            last_used: Mutex::new(Instant::now()),
            loaded: Mutex::new(HashSet::new()),
        });
        let shaken = match server.link.call(codex::initialize).await {
            Ok(_) => server.link.send(&codex::initialized()).await,
            Err(e) => Err(e),
        };
        if let Err(e) = shaken {
            server.kill().await;
            let said = server.last_stderr_line();
            return Err(StartError::Failed(match said {
                Some(said) => format!("{e}: {said}"),
                None => e,
            }));
        }
        Ok(server)
    }

    fn holds(&self, thread: &str) -> bool {
        self.loaded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(thread)
    }

    fn hold(&self, thread: &str) {
        self.loaded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(thread.to_string());
    }

    fn lease(self: &Arc<Self>) -> Lease {
        self.active.fetch_add(1, Ordering::SeqCst);
        Lease(self.clone())
    }

    fn last_stderr_line(&self) -> Option<String> {
        let tail = self.stderr.lock().unwrap_or_else(|p| p.into_inner());
        String::from_utf8_lossy(&tail)
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string)
    }

    /// Ends the process the polite way: its input closes, which makes it leave; whatever is
    /// left of its group after `CLOSE_GRACE` is killed, and the process is reaped.
    async fn stop(&self) {
        // A write that is stuck holds the lock: the group is signalled all the same.
        if let Ok(mut stdin) = self.link.stdin.try_lock() {
            stdin.take();
        }
        let Some(mut child) = self.child.lock().await.take() else {
            return;
        };
        let pid = child.id();
        if let Some(pid) = pid {
            turns::exited_within(pid, CLOSE_GRACE).await;
        }
        turns::sweep_and_reap(&mut child, pid).await;
        self.link.stdin.lock().await.take();
    }

    /// Ends the process now: SIGTERM to its group, SIGKILL after a moment.
    async fn kill(&self) {
        // The process goes first: a write that is stuck on it fails then and frees the lock.
        let child = self.child.lock().await.take();
        if let Some(mut child) = child {
            turns::terminate(&mut child, KILL_GRACE).await;
        }
        self.link.stdin.lock().await.take();
    }
}

impl Drop for Server {
    /// A server dropped with its process still held, as when a start is abandoned, takes the
    /// whole group with it.
    fn drop(&mut self) {
        if let Some(child) = self.child.get_mut().as_ref() {
            turns::signal_group(child.id(), libc::SIGKILL);
        }
    }
}

/// A turn's hold on a server: the server counts as busy while it lives.
struct Lease(Arc<Server>);

impl Drop for Lease {
    fn drop(&mut self) {
        *self.0.last_used.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct SlotState {
    server: Option<Arc<Server>>,
    /// The slot left the table: whoever finds it so takes a new one.
    retired: bool,
}

/// The server of one review. Starting it holds the slot, so two turns never start two.
#[derive(Default)]
struct Slot {
    state: tokio::sync::Mutex<SlotState>,
}

/// The servers of every review.
pub(crate) struct Servers {
    slots: Mutex<HashMap<PrRef, Arc<Slot>>>,
    closing: AtomicBool,
    sweeping: AtomicBool,
    interrupt_grace_ms: AtomicU64,
}

impl Default for Servers {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            closing: AtomicBool::new(false),
            sweeping: AtomicBool::new(false),
            interrupt_grace_ms: AtomicU64::new(INTERRUPT_GRACE.as_millis() as u64),
        }
    }
}

impl Servers {
    fn interrupt_grace(&self) -> Duration {
        Duration::from_millis(self.interrupt_grace_ms.load(Ordering::SeqCst))
    }

    #[cfg(test)]
    pub(crate) fn set_interrupt_grace(&self, grace: Duration) {
        self.interrupt_grace_ms
            .store(grace.as_millis() as u64, Ordering::SeqCst);
    }

    fn slot_of(&self, pr: &PrRef) -> Arc<Slot> {
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(pr.clone())
            .or_default()
            .clone()
    }

    /// The running server of `pr`, started if there is none or it died.
    async fn lease(
        &self,
        shared: &Arc<Shared>,
        pr: &PrRef,
        start: Start,
    ) -> Result<(Lease, Arc<Server>), StartError> {
        if self.closing.load(Ordering::SeqCst) {
            return Err(StartError::Failed("The daemon is stopping".to_string()));
        }
        let mut start = Some(start);
        loop {
            // A shutdown that began while this turn waited for the slot leaves nothing to start.
            if self.closing.load(Ordering::SeqCst) {
                return Err(StartError::Failed("The daemon is stopping".to_string()));
            }
            let slot = self.slot_of(pr);
            let mut state = slot.state.lock().await;
            if state.retired {
                continue;
            }
            if self.closing.load(Ordering::SeqCst) {
                return Err(StartError::Failed("The daemon is stopping".to_string()));
            }
            if let Some(server) = state.server.as_ref()
                && !server.link.dead.load(Ordering::SeqCst)
            {
                return Ok((server.lease(), server.clone()));
            }
            if let Some(old) = state.server.take() {
                old.stop().await;
            }
            let server = Server::start(start.take().expect("one start")).await?;
            state.server = Some(server.clone());
            self.start_sweeper(shared);
            return Ok((server.lease(), server));
        }
    }

    /// Ends the server of `pr`, if it has one. A turn that still runs on it fails.
    pub(crate) async fn close(&self, pr: &PrRef) {
        let slot = self
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(pr);
        if let Some(slot) = slot {
            let mut state = slot.state.lock().await;
            state.retired = true;
            if let Some(server) = state.server.take() {
                drop(state);
                server.stop().await;
            }
        }
    }

    /// Ends every server and refuses to start another.
    pub(crate) async fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        let slots: Vec<Arc<Slot>> = self
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
            .map(|(_, slot)| slot)
            .collect();
        let mut stopping = JoinSet::new();
        for slot in slots {
            stopping.spawn(async move {
                let mut state = slot.state.lock().await;
                state.retired = true;
                if let Some(server) = state.server.take() {
                    drop(state);
                    server.stop().await;
                }
            });
        }
        while stopping.join_next().await.is_some() {}
    }

    /// Ends the servers that no turn holds and that were last used `IDLE` before `now`, and
    /// the ones whose process died.
    pub(crate) async fn sweep(&self, now: Instant) {
        let slots: Vec<(PrRef, Arc<Slot>)> = self
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(pr, slot)| (pr.clone(), slot.clone()))
            .collect();
        for (pr, slot) in slots {
            let mut state = slot.state.lock().await;
            let stale = state.server.as_ref().is_some_and(|server| {
                let last = *server.last_used.lock().unwrap_or_else(|p| p.into_inner());
                server.active.load(Ordering::SeqCst) == 0
                    && (server.link.dead.load(Ordering::SeqCst)
                        || now.saturating_duration_since(last) >= IDLE)
            });
            if !stale {
                continue;
            }
            state.retired = true;
            let server = state.server.take();
            drop(state);
            {
                let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
                if slots.get(&pr).is_some_and(|held| Arc::ptr_eq(held, &slot)) {
                    slots.remove(&pr);
                }
            }
            if let Some(server) = server {
                server.stop().await;
            }
        }
    }

    /// Starts the task that sweeps every minute, once. It ends with the daemon.
    fn start_sweeper(&self, shared: &Arc<Shared>) {
        if self.sweeping.swap(true, Ordering::SeqCst) {
            return;
        }
        let shared: Weak<Shared> = Arc::downgrade(shared);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SWEEP).await;
                let Some(shared) = shared.upgrade() else {
                    break;
                };
                if *shared.shutdown.borrow() {
                    break;
                }
                shared.codex.sweep(Instant::now()).await;
            }
        });
    }
}

/// Ends the server of `pr`: the review was published, discarded or closed.
pub(crate) async fn close(shared: &Shared, pr: &PrRef) {
    shared.codex.close(pr).await;
}

/// The daemon stops: no process of Codex stays.
pub(crate) async fn shutdown(shared: &Shared) {
    shared.codex.shutdown().await;
}

/// How a turn's loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    Completed,
    Stopped,
    TimedOut,
    Crashed,
}

/// Waits for the approval answers in `asked` to go out, each for a moment at most.
async fn settle_answers(asked: &mut Vec<JoinHandle<()>>) {
    for mut task in asked.drain(..) {
        if tokio::time::timeout(Duration::from_secs(1), &mut task)
            .await
            .is_err()
        {
            // Left running it could still answer after the interrupt, or after the turn.
            task.abort();
            tracing::debug!("an approval answer did not finish");
        }
    }
}

/// Asks Codex to end the turn. What the reviewer is still asked is cancelled first and
/// answered, or Codex would wait for an answer that no longer matters.
async fn interrupt(
    shared: &Arc<Shared>,
    pr: &PrRef,
    turn: u64,
    asked: &mut Vec<JoinHandle<()>>,
    link: &Link,
    thread: &str,
    codex_turn: &str,
) {
    crate::permissions::cancel_turn(shared, pr, turn).await;
    settle_answers(asked).await;
    let id = link.next_id();
    let _ = link
        .send(&codex::turn_interrupt(id, thread, codex_turn))
        .await;
}

/// How long a stopped turn waits for Codex to end it before the process is killed: less when
/// the daemon is stopping.
fn grace_for(shared: &Shared, stop: &Stop) -> Duration {
    if stop.why() == Why::Shutdown {
        KILL_GRACE
    } else {
        shared.codex.interrupt_grace()
    }
}

/// A stop that came before the turn could start: it ends `Stopped`, like one that comes later.
fn stopped_early(stop: &Stop, sink: &mut dyn TurnSink) -> TurnEnd {
    sink.error(
        AgentErrorKind::Interrupted,
        stop.why().message().to_string(),
    );
    TurnEnd::Stopped(stop.why())
}

/// What an error that ended the process says: its kind and the line the chat shows.
fn crash_error(server: &Server) -> (AgentErrorKind, String) {
    let said = server.last_stderr_line();
    let kind = codex::classify_codex(said.as_deref().unwrap_or_default());
    let message = Agent::Codex.message_for(kind);
    match said {
        Some(said) => (kind, format!("{message}: {said}")),
        None => (kind, message),
    }
}

/// Runs one turn on Codex: starts the review's server if it has none, opens the turn's thread
/// (`New` starts one, `Resume` loads one, `Fork` copies one), and hands the events to `sink`
/// until the turn completes, is stopped or runs past `limit`. Only the chat's own thread is
/// reported as the review's session. No process of the turn outlives a stop that Codex ignores.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_turn(
    shared: &Arc<Shared>,
    pr: &PrRef,
    turn: u64,
    session: SessionArg,
    prompt: String,
    limit: Option<Duration>,
    stop: &Arc<Stop>,
    sink: &mut dyn TurnSink,
) -> TurnEnd {
    let harness = turns::harness_for(shared, pr).await;
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
    let start = Start {
        program: turns::program_path(shared, &harness),
        cwd: cwd.clone(),
        extra_args,
        base_env: std::env::vars_os().collect(),
    };
    // A stop while the server starts ends the turn without waiting for the handshake: the start
    // is dropped, and a process it began goes with it.
    let leased = tokio::select! {
        leased = shared.codex.lease(shared, pr, start) => leased,
        _ = stop.notified() => return stopped_early(stop, sink),
    };
    let (_lease, server) = match leased {
        Ok(leased) => leased,
        Err(StartError::NotInstalled) => {
            let kind = AgentErrorKind::NotInstalled;
            sink.error(kind, Agent::Codex.message_for(kind));
            return TurnEnd::Failed;
        }
        Err(StartError::Failed(message)) => {
            let kind = AgentErrorKind::Crashed;
            sink.error(
                kind,
                format!("{}: {message}", Agent::Codex.message_for(kind)),
            );
            return TurnEnd::Failed;
        }
    };
    sink.started();
    let link = &server.link;

    let params = codex::ThreadParams {
        cwd: cwd.clone(),
        developer_instructions: ROLE_PROMPT.to_string(),
    };
    let held = match &session {
        SessionArg::Resume(thread) if server.holds(thread) => Some(thread.clone()),
        _ => None,
    };
    let thread = match held {
        Some(thread) => thread,
        None => {
            let request = async {
                match &session {
                    SessionArg::New(_) => link.call(|id| codex::thread_start(id, &params)).await,
                    SessionArg::Resume(thread) => {
                        link.call(|id| codex::thread_resume(id, thread, &params))
                            .await
                    }
                    SessionArg::Fork { parent, .. } => {
                        link.call(|id| codex::thread_fork(id, parent, &params))
                            .await
                    }
                }
            };
            let opened = tokio::select! {
                opened = request => opened,
                _ = stop.notified() => return stopped_early(stop, sink),
            };
            match opened.map(|result| codex::thread_id_of(&result)) {
                Ok(Some(thread)) => {
                    server.hold(&thread);
                    thread
                }
                Ok(None) => {
                    let kind = AgentErrorKind::Unparsed;
                    sink.error(kind, Agent::Codex.message_for(kind));
                    return TurnEnd::Failed;
                }
                Err(message) => {
                    let kind = codex::classify_codex(&message);
                    let text = Agent::Codex.message_for(kind);
                    sink.error(kind, format!("{text}: {message}"));
                    return TurnEnd::Failed;
                }
            }
        }
    };
    if !matches!(session, SessionArg::Fork { .. }) {
        sink.event(AgentEvent::SessionId(thread.clone())).await;
    }

    let (_route, mut notes) = link.route(&thread);
    // A stop that comes while `turn/start` is out waits for its answer, up to the grace: if a
    // turn id comes back, the turn is interrupted like any other, and the other turns of the
    // review go on.
    let (requested, stopped_meanwhile) = {
        let call = link.call(|id| codex::turn_start(id, &thread, &prompt));
        tokio::pin!(call);
        tokio::select! {
            requested = &mut call => (Some(requested), false),
            _ = stop.notified() => {
                let answered = tokio::time::timeout(grace_for(shared, stop), &mut call).await;
                (answered.ok(), true)
            }
        }
    };
    let Some(requested) = requested else {
        // No answer in time: only the process can end a turn that may be starting.
        server.kill().await;
        return stopped_early(stop, sink);
    };
    let started = match requested.map(|result| codex::turn_id_of(&result)) {
        Ok(Some(started)) => started,
        // A stop that came meanwhile wins over what the turn would have said.
        Ok(None) | Err(_) if stopped_meanwhile => return stopped_early(stop, sink),
        Ok(None) => {
            let kind = AgentErrorKind::Unparsed;
            sink.error(kind, Agent::Codex.message_for(kind));
            return TurnEnd::Failed;
        }
        Err(message) => {
            let kind = codex::classify_codex(&message);
            let text = Agent::Codex.message_for(kind);
            sink.error(kind, format!("{text}: {message}"));
            return TurnEnd::Failed;
        }
    };

    let limit = limit.unwrap_or_else(|| Duration::from_secs(u64::from(harness.turn_timeout_secs)));
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let began = Instant::now();
    let mut fold = CodexTurn::new(cwd);
    let mut asked: Vec<JoinHandle<()>> = Vec::new();
    // Why the turn was interrupted, and when the process stops being waited for.
    let mut interrupted: Option<Ending> = None;
    let mut give_up = tokio::time::Instant::now() + Duration::from_secs(3600);
    if stopped_meanwhile {
        interrupt(shared, pr, turn, &mut asked, link, &thread, &started).await;
        interrupted = Some(Ending::Stopped);
        give_up = tokio::time::Instant::now() + grace_for(shared, stop);
    }
    let ending = loop {
        tokio::select! {
            routed = notes.recv() => match routed {
                Some(Routed::Note(note)) => {
                    let completed = matches!(note, Note::TurnCompleted { .. });
                    if let Some(cause) = interrupted {
                        if completed {
                            break cause;
                        }
                        continue;
                    }
                    for event in fold.on(note) {
                        sink.event(event).await;
                    }
                    if completed {
                        break Ending::Completed;
                    }
                }
                Some(Routed::Ask { id, ask }) => {
                    let paths = match &ask {
                        ServerAsk::FileChange { item_id, .. } => fold.paths_of(item_id),
                        _ => Vec::new(),
                    };
                    let approval = Approval {
                        shared: shared.clone(),
                        link: link.clone(),
                        pr: pr.clone(),
                        turn,
                        stop: stop.clone(),
                        id,
                        ask,
                        paths,
                    };
                    asked.push(tokio::spawn(approval.run()));
                }
                Some(Routed::Closed) | None => {
                    break interrupted.unwrap_or(if stop.fired() {
                        Ending::Stopped
                    } else {
                        Ending::Crashed
                    });
                }
            },
            _ = stop.notified(), if interrupted.is_none() => {
                interrupt(shared, pr, turn, &mut asked, link, &thread, &started).await;
                interrupted = Some(Ending::Stopped);
                give_up = tokio::time::Instant::now() + grace_for(shared, stop);
            }
            _ = &mut deadline, if interrupted.is_none() => {
                interrupt(shared, pr, turn, &mut asked, link, &thread, &started).await;
                interrupted = Some(Ending::TimedOut);
                give_up = tokio::time::Instant::now() + shared.codex.interrupt_grace();
            }
            _ = tokio::time::sleep_until(give_up), if interrupted.is_some() => {
                // Codex did not end the turn when asked: the process goes.
                server.kill().await;
                break interrupted.unwrap_or(Ending::Stopped);
            }
        }
    };
    // The turn is over for the agent: nobody is left to hear an answer.
    crate::permissions::cancel_turn(shared, pr, turn).await;
    settle_answers(&mut asked).await;
    let (said_result, had_failed) = (sink.saw_final(), sink.failed());
    match ending {
        Ending::Completed => {
            sink.release();
            if !said_result && !had_failed {
                let kind = AgentErrorKind::Crashed;
                sink.error(
                    kind,
                    format!("{} ended without an answer", Agent::Codex.name()),
                );
            }
        }
        Ending::Stopped => {
            sink.release();
            if !said_result && !had_failed {
                sink.error(
                    AgentErrorKind::Interrupted,
                    stop.why().message().to_string(),
                );
            }
        }
        Ending::TimedOut if said_result || had_failed => sink.release(),
        Ending::TimedOut => {
            sink.release();
            sink.error(
                AgentErrorKind::Interrupted,
                format!(
                    "The turn ran past its limit of {}s and was stopped",
                    limit.as_secs()
                ),
            );
        }
        Ending::Crashed => {
            sink.release();
            if !said_result && !had_failed {
                let (kind, message) = crash_error(&server);
                sink.error(kind, message);
            }
            server.stop().await;
        }
    }
    sink.flush();
    tracing::debug!(pr = %pr, turn, elapsed_ms = began.elapsed().as_millis() as u64, "codex turn ended");
    match (said_result, had_failed, ending) {
        (true, false, _) => TurnEnd::Done,
        (_, true, _) | (_, _, Ending::Completed | Ending::Crashed) => TurnEnd::Failed,
        (_, _, Ending::Stopped) => TurnEnd::Stopped(stop.why()),
        (_, _, Ending::TimedOut) => TurnEnd::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clusia_core::config::HarnessKind;
    use clusia_core::{Config, Paths, Review};
    use clusia_harness::testkit::{CodexCall, CodexScript, CodexTurn as Scripted, FakeCodex};
    use clusia_protocol::{Event, PermissionAnswerKind, PermissionOutcome, TurnOrigin};
    use clusia_store::save_review;
    use tokio::sync::Semaphore;

    use super::*;
    use crate::turns::{Boxed, SessionSource, TurnRun};

    fn pr(n: u64) -> PrRef {
        format!("acme/widgets#{n}").parse().unwrap()
    }

    struct Lab {
        _home: tempfile::TempDir,
        fake: tempfile::TempDir,
        shared: Arc<Shared>,
    }

    fn lab(script: CodexScript) -> Lab {
        lab_with(script, |_| {})
    }

    fn lab_with(script: CodexScript, tweak: impl FnOnce(&mut Config)) -> Lab {
        lab_in(tempfile::tempdir().unwrap(), script, tweak)
    }

    /// A lab in `home`, for a script that needs to name a path of the worktree.
    fn lab_in(
        home: tempfile::TempDir,
        script: CodexScript,
        tweak: impl FnOnce(&mut Config),
    ) -> Lab {
        let fake = tempfile::tempdir().unwrap();
        let program = FakeCodex::install(fake.path(), script);
        let mut config = Config::default();
        config.harness.kind = HarnessKind::Codex;
        config.harness.program = Some(program.display().to_string());
        tweak(&mut config);
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
        };
        let shared = Arc::new(Shared::new(Paths::new(home.path()), config, options));
        let lab = Lab {
            _home: home,
            fake,
            shared,
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

        fn calls(&self) -> Vec<CodexCall> {
            FakeCodex::calls(self.fake.path())
        }

        /// Waits until the process `n` of the fake was sent a request called `method`.
        async fn wait_request(&self, n: usize, method: &str) {
            for _ in 0..100 {
                if self
                    .calls()
                    .get(n)
                    .is_some_and(|call| call.methods().iter().any(|m| m == method))
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("process {n} never got {method}");
        }

        /// Runs one turn of review 7 as the chat does, with `session`.
        fn start(
            &self,
            session: SessionArg,
            limit: Option<Duration>,
        ) -> (JoinHandle<(TurnEnd, Recorder)>, Arc<Stop>) {
            self.start_for(&pr(7), session, limit)
        }

        fn start_for(
            &self,
            pr: &PrRef,
            session: SessionArg,
            limit: Option<Duration>,
        ) -> (JoinHandle<(TurnEnd, Recorder)>, Arc<Stop>) {
            self.start_as(pr, 1, TurnOrigin::Chat, session, limit)
        }

        /// A turn that the permission model knows (a check is registered while it runs).
        fn start_as(
            &self,
            pr: &PrRef,
            turn: u64,
            origin: TurnOrigin,
            session: SessionArg,
            limit: Option<Duration>,
        ) -> (JoinHandle<(TurnEnd, Recorder)>, Arc<Stop>) {
            let stop = Stop::new();
            let run = TurnRun {
                pr: pr.clone(),
                turn,
                prompt: "Is the expiry checked?".into(),
                fresh_prompt: None,
                session: SessionSource::Given(session),
                origin,
                places: Arc::new(Semaphore::new(2)),
                limit,
                stop: stop.clone(),
            };
            let shared = self.shared.clone();
            let task = tokio::spawn(async move {
                let mut sink = Recorder::default();
                let end = turns::run(&shared, run, &mut sink).await;
                (end, sink)
            });
            (task, stop)
        }

        async fn run(&self, session: SessionArg) -> (TurnEnd, Recorder) {
            self.start(session, None).0.await.unwrap()
        }
    }

    /// What a turn told its sink.
    #[derive(Default)]
    struct Recorder {
        started: bool,
        text: String,
        finals: Vec<String>,
        errors: Vec<(AgentErrorKind, String)>,
        session_ids: Vec<String>,
        tools: Vec<String>,
        failed: bool,
    }

    impl TurnSink for Recorder {
        fn waiting(&mut self) {}

        fn started(&mut self) {
            self.started = true;
        }

        fn event<'a>(&'a mut self, event: AgentEvent) -> Boxed<'a> {
            Box::pin(async move {
                match event {
                    AgentEvent::Text(text) => self.text.push_str(&text),
                    AgentEvent::ToolUse(tool) => self.tools.push(tool),
                    AgentEvent::SessionId(id) => self.session_ids.push(id),
                    AgentEvent::Final { text, is_error, .. } => {
                        self.finals.push(text);
                        self.failed |= is_error;
                    }
                    AgentEvent::Error { kind, message } => {
                        self.failed = true;
                        self.errors.push((kind, message));
                    }
                    AgentEvent::Denied { .. } => {}
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
            !self.finals.is_empty()
        }

        fn failed(&self) -> bool {
            self.failed
        }
    }

    fn new_thread() -> SessionArg {
        SessionArg::New("ignored".into())
    }

    /// Waits until the process is gone.
    async fn gone(pid: u32) -> bool {
        for _ in 0..80 {
            if !FakeCodex::is_running(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    fn same_folder(a: &Path, b: &Path) -> bool {
        a.canonicalize().unwrap() == b.canonicalize().unwrap()
    }

    #[tokio::test]
    async fn a_turn_streams_its_text_and_reports_its_thread() {
        let lab = lab(CodexScript::one(Scripted::answer("Looks fine.")));
        let (end, sink) = lab.run(new_thread()).await;
        assert_eq!(end, TurnEnd::Done);
        assert!(sink.started);
        assert_eq!(sink.text, "Looks fine.");
        assert_eq!(sink.finals, ["Looks fine."]);
        assert_eq!(sink.session_ids, ["thr-1"]);
        let calls = lab.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].argv, ["app-server"]);
        assert!(same_folder(
            &calls[0].cwd,
            &lab.shared.paths.worktree_for(&pr(7))
        ));
        assert_eq!(
            calls[0].methods(),
            ["initialize", "initialized", "thread/start", "turn/start"]
        );
    }

    #[tokio::test]
    async fn a_thread_carries_the_sandbox_the_approval_policy_and_the_role() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        lab.run(new_thread()).await;
        let calls = lab.calls();
        let start = &calls[0].requests("thread/start")[0]["params"];
        assert_eq!(start["sandbox"], "read-only");
        assert_eq!(start["approvalPolicy"], "on-request");
        assert_eq!(start["developerInstructions"], clusia_harness::ROLE_PROMPT);
        assert_eq!(
            start["config"],
            serde_json::json!({"features": {"hooks": false}, "notify": [], "project_doc_max_bytes": 0, "mcp_servers": {}})
        );
        assert_eq!(start["approvalsReviewer"], "user");
        let cwd = start["cwd"].as_str().unwrap();
        assert!(same_folder(
            Path::new(cwd),
            &lab.shared.paths.worktree_for(&pr(7))
        ));
        let prompt = &calls[0].requests("turn/start")[0]["params"];
        assert_eq!(prompt["input"][0]["text"], "Is the expiry checked?");
    }

    #[tokio::test]
    async fn the_turns_of_a_review_share_one_process_and_a_held_thread_is_not_resumed() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        let (_, first) = lab.run(new_thread()).await;
        let thread = first.session_ids[0].clone();
        let (end, second) = lab.run(SessionArg::Resume(thread.clone())).await;
        assert_eq!(end, TurnEnd::Done);
        assert_eq!(second.session_ids, std::slice::from_ref(&thread));
        let calls = lab.calls();
        assert_eq!(calls.len(), 1, "one process for the review");
        assert_eq!(
            calls[0].methods(),
            [
                "initialize",
                "initialized",
                "thread/start",
                "turn/start",
                "turn/start"
            ]
        );
        let turns = calls[0].requests("turn/start");
        assert_eq!(turns[1]["params"]["threadId"], thread);
    }

    #[tokio::test]
    async fn a_new_process_resumes_a_thread_the_old_one_held() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        let (_, first) = lab.run(new_thread()).await;
        let thread = first.session_ids[0].clone();
        close(&lab.shared, &pr(7)).await;
        let (end, _) = lab.run(SessionArg::Resume(thread.clone())).await;
        assert_eq!(end, TurnEnd::Done);
        let calls = lab.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1].methods(),
            ["initialize", "initialized", "thread/resume", "turn/start"]
        );
        assert_eq!(
            calls[1].requests("thread/resume")[0]["params"]["threadId"],
            thread
        );
        lab.shared.codex.shutdown().await;
    }

    #[tokio::test]
    async fn a_pinned_review_follows_a_program_change_of_the_same_agent_only() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        let pinned = lab.shared.config.read().await.harness.clone();
        lab.shared.sessions.pin(&pr(7), &pinned);
        lab.shared.config.write().await.harness.program = Some("/changed/codex".into());
        let now = turns::harness_for(&lab.shared, &pr(7)).await;
        assert_eq!(now.kind, HarnessKind::Codex);
        assert_eq!(now.program.as_deref(), Some("/changed/codex"));

        // Another agent in the config: the review keeps its own agent and program.
        lab.shared.config.write().await.harness.kind = HarnessKind::ClaudeCode;
        let now = turns::harness_for(&lab.shared, &pr(7)).await;
        assert_eq!(now.kind, HarnessKind::Codex);
        assert_eq!(now.program, pinned.program);
        assert_eq!(now.extra_args, pinned.extra_args);

        // A review that is not open follows the config.
        lab.shared.sessions.unpin(&pr(7));
        let now = turns::harness_for(&lab.shared, &pr(7)).await;
        assert_eq!(now.kind, HarnessKind::ClaudeCode);
    }

    #[tokio::test]
    async fn notifications_name_the_pinned_agent_not_the_config() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        let mut claude = lab.shared.config.read().await.harness.clone();
        claude.kind = HarnessKind::ClaudeCode;
        lab.shared.sessions.pin(&pr(7), &claude);
        let review = crate::reviews::load_stored(&lab.shared, &pr(7))
            .unwrap()
            .unwrap();
        // The config says Codex, the review was opened on Claude Code and has no session yet.
        assert_eq!(
            crate::notifications::harness_name(&lab.shared, Some(&review)).await,
            "Claude Code"
        );
        lab.shared.sessions.unpin(&pr(7));
        assert_eq!(
            crate::notifications::harness_name(&lab.shared, Some(&review)).await,
            "Codex"
        );
    }

    #[tokio::test]
    async fn a_codex_turn_is_recorded_as_unsandboxed_because_what_it_asks_leaves_the_sandbox() {
        let lab = lab(CodexScript::one(Scripted::hang()));
        let (task, stop) = lab.start_as(&pr(7), 5, TurnOrigin::Security, new_thread(), None);
        lab.wait_request(0, "turn/start").await;
        assert_eq!(lab.shared.sessions.turn_sandbox(&pr(7), 5), Some(false));
        stop.fire(Why::User);
        task.await.unwrap();
        lab.shared.codex.shutdown().await;
    }

    #[tokio::test]
    async fn another_review_gets_a_process_of_its_own() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        lab.add_review(&pr(8));
        lab.run(new_thread()).await;
        let (task, _) = lab.start_for(&pr(8), new_thread(), None);
        task.await.unwrap();
        let calls = lab.calls();
        assert_eq!(calls.len(), 2);
        assert!(!same_folder(&calls[0].cwd, &calls[1].cwd));
    }

    #[tokio::test]
    async fn a_fork_copies_the_thread_and_is_not_reported_as_the_session() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        let (_, chat) = lab.run(new_thread()).await;
        let parent = chat.session_ids[0].clone();
        let (end, fork) = lab
            .run(SessionArg::Fork {
                parent: parent.clone(),
                id: "ignored".into(),
            })
            .await;
        assert_eq!(end, TurnEnd::Done);
        assert!(fork.session_ids.is_empty(), "{:?}", fork.session_ids);
        let calls = lab.calls();
        let forks = calls[0].requests("thread/fork");
        assert_eq!(forks.len(), 1);
        assert_eq!(forks[0]["params"]["threadId"], parent);
        assert_eq!(forks[0]["params"]["sandbox"], "read-only");
        let turns = calls[0].requests("turn/start");
        assert_eq!(
            turns[1]["params"]["threadId"], "thr-2",
            "the turn runs on the copy"
        );
    }

    #[tokio::test]
    async fn a_failed_turn_fails_with_what_codex_said() {
        let lab = lab(CodexScript::one(Scripted::fail("model overloaded")));
        let (end, sink) = lab.run(new_thread()).await;
        assert_eq!(end, TurnEnd::Failed);
        assert!(sink.finals.is_empty());
        assert!(
            sink.errors
                .iter()
                .any(|(_, message)| message.contains("model overloaded")),
            "{:?}",
            sink.errors
        );
    }

    #[tokio::test]
    async fn a_crash_fails_the_turn_and_the_next_turn_starts_a_new_process() {
        let lab = lab(CodexScript::turns(vec![
            Scripted::crash(),
            Scripted::answer("again"),
        ]));
        let (end, sink) = lab.run(new_thread()).await;
        assert_eq!(end, TurnEnd::Failed);
        assert_eq!(
            sink.errors,
            [(
                AgentErrorKind::Crashed,
                "Codex stopped unexpectedly".to_string()
            )]
        );
        let first = lab.calls().remove(0);
        assert!(gone(first.pid).await, "the crashed process is reaped");

        let (end, sink) = lab.run(new_thread()).await;
        assert_eq!(end, TurnEnd::Done);
        assert_eq!(sink.text, "again");
        assert_eq!(lab.calls().len(), 2);
    }

    #[tokio::test]
    async fn a_stop_interrupts_the_turn_and_the_server_stays() {
        let lab = lab(CodexScript::one(Scripted::hang()));
        let (task, stop) = lab.start(new_thread(), None);
        lab.wait_request(0, "turn/start").await;
        stop.fire(Why::User);
        let (end, sink) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Stopped(Why::User));
        assert_eq!(
            sink.errors,
            [(AgentErrorKind::Interrupted, "Stopped by you".to_string())]
        );
        let call = lab.calls().remove(0);
        assert!(call.methods().contains(&"turn/interrupt".to_string()));
        assert!(FakeCodex::is_running(call.pid), "the next turn reuses it");
        lab.shared.codex.shutdown().await;
    }

    #[tokio::test]
    async fn a_hung_turn_is_killed_after_interrupt() {
        let lab = lab(CodexScript::one(Scripted::hang().ignore_interrupt()));
        lab.shared
            .codex
            .set_interrupt_grace(Duration::from_millis(300));
        let (task, stop) = lab.start(new_thread(), None);
        lab.wait_request(0, "turn/start").await;
        stop.fire(Why::User);
        let (end, _) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Stopped(Why::User));
        let call = lab.calls().remove(0);
        assert!(
            gone(call.pid).await,
            "the process that ignored the interrupt was killed"
        );
    }

    #[tokio::test]
    async fn a_turn_past_its_limit_is_interrupted() {
        let lab = lab(CodexScript::one(Scripted::hang()));
        let (task, _stop) = lab.start(new_thread(), Some(Duration::from_millis(300)));
        let (end, sink) = task.await.unwrap();
        assert_eq!(end, TurnEnd::TimedOut);
        assert!(
            sink.errors[0]
                .1
                .starts_with("The turn ran past its limit of 0s"),
            "{:?}",
            sink.errors
        );
        assert!(
            lab.calls()[0]
                .methods()
                .contains(&"turn/interrupt".to_string())
        );
        lab.shared.codex.shutdown().await;
    }

    #[tokio::test]
    async fn close_ends_the_codex_process() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        lab.run(new_thread()).await;
        let pid = lab.calls()[0].pid;
        assert!(FakeCodex::is_running(pid));
        close(&lab.shared, &pr(7)).await;
        assert!(gone(pid).await);
        let (end, _) = lab.run(new_thread()).await;
        assert_eq!(end, TurnEnd::Done);
        assert_eq!(lab.calls().len(), 2, "a later turn starts a new process");
        lab.shared.codex.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_ends_every_process_and_refuses_new_ones() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        lab.add_review(&pr(8));
        lab.run(new_thread()).await;
        let (task, _) = lab.start_for(&pr(8), new_thread(), None);
        task.await.unwrap();
        let pids: Vec<u32> = lab.calls().iter().map(|c| c.pid).collect();
        assert_eq!(pids.len(), 2);
        shutdown(&lab.shared).await;
        for pid in pids {
            assert!(gone(pid).await, "pid {pid} survived");
        }
        let (end, sink) = lab.run(new_thread()).await;
        assert_eq!(end, TurnEnd::Failed);
        assert!(sink.errors[0].1.contains("stopping"), "{:?}", sink.errors);
        assert_eq!(lab.calls().len(), 2);
    }

    #[tokio::test]
    async fn idle_server_is_stopped() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        lab.run(new_thread()).await;
        let pid = lab.calls()[0].pid;
        lab.shared.codex.sweep(Instant::now()).await;
        assert!(FakeCodex::is_running(pid), "just used, so not idle");
        lab.shared
            .codex
            .sweep(Instant::now() + IDLE + Duration::from_secs(1))
            .await;
        assert!(gone(pid).await, "idle for half an hour");
        let (end, _) = lab.run(new_thread()).await;
        assert_eq!(end, TurnEnd::Done);
        assert_eq!(lab.calls().len(), 2);
        lab.shared.codex.shutdown().await;
    }

    #[tokio::test]
    async fn a_server_with_a_turn_running_is_never_idle() {
        let lab = lab(CodexScript::one(Scripted::hang()));
        let (task, stop) = lab.start(new_thread(), None);
        lab.wait_request(0, "turn/start").await;
        lab.shared.codex.sweep(Instant::now() + IDLE * 2).await;
        let pid = lab.calls()[0].pid;
        assert!(FakeCodex::is_running(pid));
        stop.fire(Why::User);
        task.await.unwrap();
        lab.shared.codex.shutdown().await;
    }

    #[tokio::test]
    async fn the_extra_arguments_follow_app_server_without_the_reserved_ones() {
        let lab = lab_with(CodexScript::one(Scripted::answer("ok")), |config| {
            config.harness.extra_args = "--search --yolo".into();
        });
        lab.run(new_thread()).await;
        assert_eq!(lab.calls()[0].argv, ["app-server", "--search"]);
    }

    #[tokio::test]
    async fn a_missing_codex_is_not_installed() {
        let lab = lab_with(CodexScript::one(Scripted::answer("ok")), |config| {
            config.harness.program = Some("/nonexistent/codex".into());
        });
        let (end, sink) = lab.run(new_thread()).await;
        assert_eq!(end, TurnEnd::Failed);
        assert_eq!(sink.errors[0].0, AgentErrorKind::NotInstalled);
        assert!(sink.errors[0].1.contains("Codex"), "{:?}", sink.errors);
        assert!(lab.calls().is_empty());
    }

    #[tokio::test]
    async fn a_refused_send_leaves_no_pin() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        let prompt = crate::sessions::Prompt {
            text: "hi".into(),
            shown: true,
            summary_for: None,
        };
        let refused = crate::sessions::Sessions::submit(&lab.shared, &pr(99), prompt).await;
        assert!(matches!(
            refused,
            Err(crate::sessions::Refusal::NoReview(_))
        ));
        assert!(lab.shared.sessions.pinned(&pr(99)).is_none());
    }

    #[tokio::test]
    async fn a_review_closed_during_a_turn_is_released_when_the_last_turn_ends() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        let harness = lab.shared.config.read().await.harness.clone();
        lab.run(new_thread()).await;
        let pid = lab.calls()[0].pid;
        lab.shared.sessions.pin(&pr(7), &harness);
        lab.shared.sessions.pretend_running(&pr(7), 3);

        lab.shared.sessions.close_review(&lab.shared, &pr(7)).await;
        assert!(lab.shared.sessions.pinned(&pr(7)).is_some());
        assert!(FakeCodex::is_running(pid));

        lab.shared.sessions.pretend_stopped(&pr(7));
        lab.shared
            .sessions
            .release_closed(&lab.shared, &pr(7))
            .await;
        assert!(lab.shared.sessions.pinned(&pr(7)).is_none());
        assert!(gone(pid).await, "the process outlived the closed review");
    }

    #[tokio::test]
    async fn an_open_review_is_not_released_when_its_turn_ends() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        let harness = lab.shared.config.read().await.harness.clone();
        lab.run(new_thread()).await;
        let pid = lab.calls()[0].pid;
        lab.shared.sessions.pin(&pr(7), &harness);
        lab.shared
            .sessions
            .release_closed(&lab.shared, &pr(7))
            .await;
        assert!(lab.shared.sessions.pinned(&pr(7)).is_some());
        assert!(FakeCodex::is_running(pid));
        lab.shared.codex.shutdown().await;
    }

    #[tokio::test]
    async fn opening_again_replaces_the_pin_unless_a_turn_runs() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        let codex = lab.shared.config.read().await.harness.clone();
        let mut claude = codex.clone();
        claude.kind = HarnessKind::ClaudeCode;
        lab.shared.sessions.pin(&pr(7), &codex);

        lab.shared
            .sessions
            .repin(&lab.shared, &pr(7), &claude)
            .await;
        let pinned = lab.shared.sessions.pinned(&pr(7)).unwrap();
        assert_eq!(pinned.kind, HarnessKind::ClaudeCode);

        lab.shared.sessions.pretend_running(&pr(7), 3);
        lab.shared.sessions.repin(&lab.shared, &pr(7), &codex).await;
        let pinned = lab.shared.sessions.pinned(&pr(7)).unwrap();
        assert_eq!(
            pinned.kind,
            HarnessKind::ClaudeCode,
            "a running turn keeps it"
        );
        lab.shared.sessions.pretend_stopped(&pr(7));
    }

    #[tokio::test]
    async fn opening_on_another_kind_ends_the_codex_process() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        let codex = lab.shared.config.read().await.harness.clone();
        let mut claude = codex.clone();
        claude.kind = HarnessKind::ClaudeCode;
        lab.run(new_thread()).await;
        let pid = lab.calls()[0].pid;
        lab.shared.sessions.pin(&pr(7), &codex);

        lab.shared.sessions.repin(&lab.shared, &pr(7), &codex).await;
        assert!(FakeCodex::is_running(pid), "the same kind keeps it");

        lab.shared
            .sessions
            .repin(&lab.shared, &pr(7), &claude)
            .await;
        assert!(gone(pid).await, "the process outlived the switch");
    }

    #[tokio::test]
    async fn a_turn_that_waited_for_the_slot_during_shutdown_starts_nothing() {
        let lab = lab(CodexScript::one(Scripted::answer("ok")));
        let slot = lab.shared.codex.slot_of(&pr(7));
        let held = slot.state.lock().await;
        let (task, _stop) = lab.start(new_thread(), None);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let shared = lab.shared.clone();
        let closing = tokio::spawn(async move { shared.codex.shutdown().await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(held);
        closing.await.unwrap();
        let (end, sink) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Failed);
        assert!(sink.errors[0].1.contains("stopping"), "{:?}", sink.errors);
        assert!(lab.calls().is_empty(), "no process was started");
    }

    #[tokio::test]
    async fn a_request_codex_cannot_be_given_a_decision_for_gets_an_error() {
        let lab = lab(CodexScript::one(
            Scripted::answer("ok").ask_other("mcpServer/elicitation/request"),
        ));
        let (end, _) = lab.run(new_thread()).await;
        assert_eq!(end, TurnEnd::Done, "the turn goes on");
        let dir = lab.fake.path().to_path_buf();
        let decisions = tokio::task::spawn_blocking(move || {
            FakeCodex::wait_for_decisions(&dir, 1, Duration::from_secs(5))
        })
        .await
        .unwrap();
        assert_eq!(decisions, ["error"]);
    }

    type Events = tokio::sync::broadcast::Receiver<(String, Event)>;

    /// The next permission request the daemon announces.
    async fn next_request(events: &mut Events) -> (String, Event) {
        loop {
            let (_, event) = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("a request is announced")
                .expect("the channel stays open");
            if let Event::PermissionRequested { id, .. } = &event {
                return (id.clone(), event);
            }
        }
    }

    /// How the daemon says the request `id` ended.
    async fn outcome_of(events: &mut Events, id: &str) -> PermissionOutcome {
        loop {
            let (_, event) = tokio::time::timeout(Duration::from_secs(10), events.recv())
                .await
                .expect("the request ends")
                .expect("the channel stays open");
            if let Event::PermissionResolved {
                id: got, outcome, ..
            } = event
                && got == id
            {
                return outcome;
            }
        }
    }

    /// Every decision the fake received, once `count` of them arrived.
    async fn decisions(lab: &Lab, count: usize) -> Vec<String> {
        let dir = lab.fake.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            FakeCodex::wait_for_decisions(&dir, count, Duration::from_secs(5))
        })
        .await
        .unwrap()
    }

    fn asking(command: &str) -> CodexScript {
        CodexScript::one(Scripted::answer("done").ask_command(command))
    }

    /// Runs turn `turn` as a check, which the permission model knows while it runs.
    fn run_as_check(lab: &Lab, turn: u64) -> (JoinHandle<(TurnEnd, Recorder)>, Arc<Stop>) {
        lab.start_as(&pr(7), turn, TurnOrigin::Security, new_thread(), None)
    }

    async fn answer_request(lab: &Lab, id: &str, kind: PermissionAnswerKind) {
        permissions::answer(&lab.shared, id, kind).await;
    }

    #[tokio::test]
    async fn codex_command_asks_and_decline_on_timeout() {
        let lab = lab_with(asking("cargo test -p clusia-core"), |config| {
            config.harness.permission_timeout_secs = 1;
        });
        let mut events = lab.shared.events.subscribe();
        let (task, _stop) = run_as_check(&lab, 5);
        let (id, asked) = next_request(&mut events).await;
        match asked {
            Event::PermissionRequested {
                tool,
                summary,
                origin,
                sandbox,
                turn,
                ..
            } => {
                assert_eq!(tool, "Bash");
                assert_eq!(summary, "cargo test -p clusia-core");
                assert_eq!(origin, TurnOrigin::Security);
                assert!(
                    !sandbox,
                    "what Codex asks about leaves its sandbox, and the modal says so"
                );
                assert_eq!(turn, 5);
            }
            other => panic!("{other:?}"),
        }
        let (end, sink) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Done, "the turn goes on after a decline");
        assert_eq!(sink.finals, ["done"]);
        assert_eq!(
            outcome_of(&mut events, &id).await,
            PermissionOutcome::Expired
        );
        assert_eq!(decisions(&lab, 1).await, ["decline"]);
    }

    #[tokio::test]
    async fn allowing_once_accepts() {
        let lab = lab(asking("cargo test -p clusia-core"));
        let mut events = lab.shared.events.subscribe();
        let (task, _stop) = run_as_check(&lab, 5);
        let (id, _) = next_request(&mut events).await;
        answer_request(&lab, &id, PermissionAnswerKind::Once).await;
        let (end, _) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Done);
        assert_eq!(decisions(&lab, 1).await, ["accept"]);
    }

    #[tokio::test]
    async fn denying_declines_and_the_turn_goes_on() {
        let lab = lab(asking("rm -rf target"));
        let mut events = lab.shared.events.subscribe();
        let (task, _stop) = run_as_check(&lab, 5);
        let (id, _) = next_request(&mut events).await;
        answer_request(&lab, &id, PermissionAnswerKind::Deny).await;
        let (end, sink) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Done);
        assert_eq!(sink.finals, ["done"]);
        assert_eq!(decisions(&lab, 1).await, ["decline"]);
    }

    #[tokio::test]
    async fn allowing_for_the_review_covers_the_next_command_without_asking() {
        let lab = lab(asking("cargo test -p clusia-core"));
        let mut events = lab.shared.events.subscribe();
        let (task, _stop) = run_as_check(&lab, 5);
        let (id, _) = next_request(&mut events).await;
        answer_request(&lab, &id, PermissionAnswerKind::Review).await;
        task.await.unwrap();

        let (task, _stop) = run_as_check(&lab, 6);
        let (end, _) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Done);
        assert_eq!(decisions(&lab, 2).await, ["accept", "accept"]);
        while let Ok((_, event)) = events.try_recv() {
            assert!(
                !matches!(event, Event::PermissionRequested { turn: 6, .. }),
                "the rule of the review covered it"
            );
        }
    }

    /// A lab whose fake lists `files` (written under the worktree of review 7) in a file change.
    fn lab_changing(files: impl FnOnce(&Path) -> Vec<String>) -> Lab {
        let home = tempfile::tempdir().unwrap();
        let worktree = Paths::new(home.path()).worktree_for(&pr(7));
        let files = files(&worktree);
        let listed: Vec<&str> = files.iter().map(String::as_str).collect();
        lab_in(
            home,
            CodexScript::one(Scripted::answer("done").ask_files(&listed)),
            |_| {},
        )
    }

    #[tokio::test]
    async fn a_file_change_asks_as_an_edit_with_its_path_in_the_worktree() {
        let lab =
            lab_changing(|worktree| vec![worktree.join("src/auth/store.rs").display().to_string()]);
        let mut events = lab.shared.events.subscribe();
        let (task, _stop) = run_as_check(&lab, 5);
        let (id, asked) = next_request(&mut events).await;
        match asked {
            Event::PermissionRequested { tool, summary, .. } => {
                assert_eq!(tool, "Edit");
                assert_eq!(summary, "src/auth/store.rs");
            }
            other => panic!("{other:?}"),
        }
        answer_request(&lab, &id, PermissionAnswerKind::Once).await;
        let (end, _) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Done);
        assert_eq!(decisions(&lab, 1).await, ["accept"]);
    }

    #[tokio::test]
    async fn a_file_change_outside_the_worktree_is_declined_without_asking() {
        let lab = lab_changing(|_| vec!["/etc/hosts".to_string()]);
        let mut events = lab.shared.events.subscribe();
        let (task, _stop) = run_as_check(&lab, 5);
        let (end, _) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Done);
        assert_eq!(decisions(&lab, 1).await, ["decline"]);
        let mut resolved = Vec::new();
        while let Ok((_, event)) = events.try_recv() {
            match event {
                Event::PermissionRequested { .. } => panic!("nobody was asked"),
                Event::PermissionResolved { outcome, .. } => resolved.push(outcome),
                _ => {}
            }
        }
        assert_eq!(resolved, [PermissionOutcome::Denied]);
    }

    #[tokio::test]
    async fn one_path_outside_the_worktree_declines_the_whole_change() {
        let lab = lab_changing(|worktree| {
            vec![
                worktree.join("src/lib.rs").display().to_string(),
                "../../outside.txt".to_string(),
            ]
        });
        let mut events = lab.shared.events.subscribe();
        let (task, _stop) = run_as_check(&lab, 5);
        let (end, _) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Done);
        assert_eq!(decisions(&lab, 1).await, ["decline"]);
        while let Ok((_, event)) = events.try_recv() {
            assert!(
                !matches!(event, Event::PermissionRequested { .. }),
                "nobody was asked"
            );
        }
    }

    /// Runs a check whose fake sends the file change `lab` was built with, and expects it
    /// declined with no question and one denial told.
    async fn declined_without_asking(lab: &Lab) {
        let mut events = lab.shared.events.subscribe();
        let (task, _stop) = run_as_check(lab, 5);
        let (end, _) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Done);
        assert_eq!(decisions(lab, 1).await, ["decline"]);
        let mut resolved = Vec::new();
        while let Ok((_, event)) = events.try_recv() {
            match event {
                Event::PermissionRequested { .. } => panic!("nobody was asked"),
                Event::PermissionResolved { outcome, .. } => resolved.push(outcome),
                _ => {}
            }
        }
        assert_eq!(resolved, [PermissionOutcome::Denied]);
    }

    #[tokio::test]
    async fn a_second_path_outside_the_worktree_declines_the_change_without_asking() {
        let lab = lab_changing(|worktree| {
            vec![
                worktree.join("src/lib.rs").display().to_string(),
                "/etc/hosts".to_string(),
            ]
        });
        declined_without_asking(&lab).await;
    }

    #[tokio::test]
    async fn a_file_change_with_no_known_paths_is_declined_without_asking() {
        let lab = lab_changing(|_| Vec::new());
        declined_without_asking(&lab).await;
    }

    #[tokio::test]
    async fn a_file_through_a_link_that_leaves_the_worktree_is_declined_without_asking() {
        let lab = lab_changing(|worktree| {
            std::fs::create_dir_all(worktree).unwrap();
            std::os::unix::fs::symlink("/etc", worktree.join("link")).unwrap();
            vec![worktree.join("link/hosts").display().to_string()]
        });
        declined_without_asking(&lab).await;
    }

    /// A lab whose fake asks to run `command` in `cwd(worktree)`.
    fn lab_running_in(command: &str, cwd: impl FnOnce(&Path) -> String) -> Lab {
        let home = tempfile::tempdir().unwrap();
        let worktree = Paths::new(home.path()).worktree_for(&pr(7));
        let cwd = cwd(&worktree);
        lab_in(
            home,
            CodexScript::one(Scripted::answer("done").ask_command_in(command, &cwd)),
            |_| {},
        )
    }

    #[tokio::test]
    async fn a_command_outside_the_worktree_is_declined_without_asking() {
        let lab = lab_running_in("cargo test", |_| "/tmp".to_string());
        declined_without_asking(&lab).await;
    }

    #[tokio::test]
    async fn a_command_with_no_text_is_declined_without_asking() {
        let lab = lab(asking(""));
        declined_without_asking(&lab).await;
    }

    #[tokio::test]
    async fn a_command_in_a_worktree_folder_says_where_it_runs() {
        let lab = lab_running_in("cargo test", |worktree| {
            worktree.join("crates/app").display().to_string()
        });
        let mut events = lab.shared.events.subscribe();
        let (task, _stop) = run_as_check(&lab, 5);
        let (id, asked) = next_request(&mut events).await;
        match asked {
            Event::PermissionRequested {
                tool,
                summary,
                reason,
                ..
            } => {
                assert_eq!(tool, "Bash");
                assert_eq!(summary, "cargo test");
                assert!(
                    reason.is_some_and(|r| r.contains("crates/app")),
                    "the folder is shown"
                );
            }
            other => panic!("{other:?}"),
        }
        answer_request(&lab, &id, PermissionAnswerKind::Once).await;
        let (end, _) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Done);
        assert_eq!(decisions(&lab, 1).await, ["accept"]);
    }

    #[tokio::test]
    async fn a_file_change_that_asks_for_a_root_is_declined_without_asking() {
        let lab = lab(CodexScript::one(
            Scripted::answer("done").ask_files_with_root(&["src/lib.rs"], "/"),
        ));
        declined_without_asking(&lab).await;
    }

    #[tokio::test]
    async fn stopping_the_turn_while_it_asks_answers_cancel() {
        let lab = lab(asking("make"));
        let mut events = lab.shared.events.subscribe();
        let (task, stop) = run_as_check(&lab, 5);
        let (id, _) = next_request(&mut events).await;
        stop.fire(Why::User);
        let (end, _) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Stopped(Why::User));
        assert_eq!(
            outcome_of(&mut events, &id).await,
            PermissionOutcome::Cancelled
        );
        assert_eq!(decisions(&lab, 1).await, ["cancel"]);
        assert_eq!(lab.shared.permissions.waiting(), 0);
    }

    #[tokio::test]
    async fn a_turn_the_daemon_does_not_know_cannot_ask() {
        let lab = lab(asking("make"));
        let mut events = lab.shared.events.subscribe();
        let (end, _) = lab.run(new_thread()).await;
        assert_eq!(end, TurnEnd::Done);
        assert_eq!(decisions(&lab, 1).await, ["decline"]);
        while let Ok((_, event)) = events.try_recv() {
            assert!(!matches!(event, Event::PermissionRequested { .. }));
        }
    }

    #[tokio::test]
    async fn codex_never_sends_accept_for_session() {
        let lab = lab(asking("cargo test -p clusia-core"));
        let mut events = lab.shared.events.subscribe();
        // Denied, allowed once, then allowed for the review: every kind of answer.
        for (turn, kind) in [
            (1, PermissionAnswerKind::Deny),
            (2, PermissionAnswerKind::Once),
            (3, PermissionAnswerKind::Review),
        ] {
            let (task, _stop) = run_as_check(&lab, turn);
            let (id, _) = next_request(&mut events).await;
            answer_request(&lab, &id, kind).await;
            task.await.unwrap();
        }
        // A fourth is covered by the rule and accepted at once.
        let (task, _stop) = run_as_check(&lab, 4);
        task.await.unwrap();
        let sent = decisions(&lab, 4).await;
        assert_eq!(sent, ["decline", "accept", "accept", "accept"]);
        for call in lab.calls() {
            for line in &call.lines {
                assert!(!line.contains("acceptForSession"), "{line}");
                assert!(!line.contains("Amendment"), "{line}");
            }
        }
    }
}
