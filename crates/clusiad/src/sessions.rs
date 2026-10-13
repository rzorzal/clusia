//! Agent turns: one `claude -p` process per message, run one at a time for each review.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clusia_core::PrRef;
use clusia_core::config::{Harness, HarnessKind};
use clusia_harness::{Agent, AgentEvent, SessionArg, extract_suggestions, version_at_least};
use clusia_protocol::{
    AgentErrorKind, AgentLogEntry, ErrorCode, Event, Outcome, ProbeResult, ProtocolError, Reply,
    SessionStateKind, Suggestion, TurnOrigin, topics,
};
use tokio::process::Command;
use tokio::sync::{Semaphore, watch};

use crate::agent_log;
use crate::agent_stream::StreamFilter;
use crate::reviews;
use crate::state::Shared;
use crate::sync::now_unix;
use crate::turns::{
    self, Boxed, KILL_GRACE, SessionSource, Stop, TurnEnd, TurnRun, TurnSink, Why, program_path,
};

/// Turns running at the same time across all reviews; the rest wait.
pub(crate) const MAX_CONCURRENT_TURNS: usize = 3;
const PROBE_LIMIT: Duration = Duration::from_secs(10);

/// What a turn asks the agent. `shown` is false for the prompts the daemon writes itself,
/// which the chat never displays as something the user said.
#[derive(Clone)]
pub(crate) struct Prompt {
    pub text: String,
    pub shown: bool,
    /// The head a summary turn describes. When the turn ends well the daemon records it, so the
    /// same head is not summarized twice.
    pub summary_for: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A turn is running and another one already waits.
    Busy,
    NoReview(String),
    Stopping,
    /// A summary of the same head is already running or waiting.
    Summarizing,
}

impl Refusal {
    /// `harness` is the display name of the agent the refusal talks about.
    pub(crate) fn into_outcome(self, harness: &str) -> Outcome {
        let (code, message) = match self {
            Refusal::Busy => (
                ErrorCode::Busy,
                AgentErrorKind::Busy.message_for(harness, ""),
            ),
            Refusal::NoReview(message) => (ErrorCode::InvalidState, message),
            Refusal::Stopping => (ErrorCode::InvalidState, "clusiad is stopping".to_string()),
            Refusal::Summarizing => (
                ErrorCode::Busy,
                format!("{harness} is already reading this version"),
            ),
        };
        Outcome::Err(ProtocolError::new(code, message))
    }
}

struct Active {
    turn: u64,
    stop: Arc<Stop>,
    /// Whether the program of this turn runs in the sandbox; set once its command is built.
    sandbox: Option<bool>,
}

/// The agent, the program and the extra arguments a review's turns run with from the moment it
/// is opened, whatever Config › Harness says meanwhile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pinned {
    pub kind: HarnessKind,
    pub program: Option<String>,
    pub extra_args: String,
    pub custom_name: String,
}

impl Pinned {
    fn of(harness: &Harness) -> Self {
        Self {
            kind: harness.kind,
            program: harness.program.clone(),
            extra_args: harness.extra_args.clone(),
            custom_name: harness.custom_name.clone(),
        }
    }

    /// What the pages call the pinned harness.
    pub(crate) fn display_name(&self) -> String {
        Harness {
            kind: self.kind,
            program: self.program.clone(),
            custom_name: self.custom_name.clone(),
            ..Harness::default()
        }
        .display_name()
    }
}

/// A security check or audit whose program runs.
struct RunningCheck {
    turn: u64,
    origin: TurnOrigin,
    /// Whether its program runs in the sandbox; set once its command is built.
    sandbox: Option<bool>,
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
    /// The checks running now, which are not the chat's turn but may ask for permission.
    checks: Vec<RunningCheck>,
    /// The summary turn running or waiting, and the head it describes. The head is recorded
    /// only when that turn ends well, so until then this is what keeps a second open of the
    /// same head from asking again.
    summarizing: Option<(u64, String)>,
}

impl Slot {
    /// The next turn id of the review, above the highest one handed out or already logged.
    fn take_turn(&mut self, floor: u64) -> u64 {
        self.last_turn = self.last_turn.max(floor) + 1;
        self.last_turn
    }

    /// Turn `turn` ended or was dropped: if it was the summary, the head may be asked again.
    fn summary_over(&mut self, turn: u64) {
        if self.summarizing.as_ref().is_some_and(|(t, _)| *t == turn) {
            self.summarizing = None;
        }
    }
}

/// The running agent turns of every review.
pub(crate) struct Sessions {
    table: Mutex<HashMap<PrRef, Slot>>,
    permits: Arc<Semaphore>,
    /// How many review drivers are running; shutdown waits for zero.
    live: watch::Sender<usize>,
    closing: AtomicBool,
    /// The last state announced for each review, told again whenever its log is read. A lock
    /// of its own: states are announced while `table` is held.
    states: Mutex<HashMap<PrRef, SessionStateKind>>,
    /// The harness each open review's turns run on; set when it opens or at its first message.
    pinned: Mutex<HashMap<PrRef, Pinned>>,
    /// Saved reviews closed while a turn still ran: the last turn to end releases their pin and
    /// their Codex process.
    closed: Mutex<HashSet<PrRef>>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_TURNS)),
            live: watch::channel(0).0,
            closing: AtomicBool::new(false),
            states: Mutex::new(HashMap::new()),
            pinned: Mutex::new(HashMap::new()),
            closed: Mutex::new(HashSet::new()),
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
    {
        let mut states = shared
            .sessions
            .states
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if state == SessionStateKind::None {
            states.remove(pr);
        } else {
            states.insert(pr.clone(), state);
        }
    }
    shared.publish(
        topics::AGENT,
        Event::SessionState {
            pr: pr.clone(),
            state,
        },
    );
}

impl Sessions {
    /// Pins the harness `harness` names for the turns of `pr`, unless the review has one.
    pub(crate) fn pin(&self, pr: &PrRef, harness: &Harness) {
        self.pinned
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(pr.clone())
            .or_insert_with(|| Pinned::of(harness));
    }

    /// The review opens: it runs on the config's harness, unless a turn of an earlier open still
    /// runs, which keeps the one it started with. A close waiting for that turn is cancelled. A
    /// Codex server of an earlier open ends when the review moves to another kind.
    pub(crate) async fn repin(&self, shared: &Shared, pr: &PrRef, harness: &Harness) {
        self.closed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(pr);
        if self.any_running(pr) {
            self.pin(pr, harness);
            return;
        }
        let replaced = self
            .pinned
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(pr.clone(), Pinned::of(harness));
        if replaced.is_some_and(|old| old.kind == HarnessKind::Codex && harness.kind != old.kind) {
            crate::codex::close(shared, pr).await;
        }
    }

    /// The harness `pr`'s turns are pinned to; `None` for a review that is not open.
    pub(crate) fn pinned(&self, pr: &PrRef) -> Option<Pinned> {
        self.pinned
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(pr)
            .cloned()
    }

    /// What the pages call the harness `pr` is pinned to; `None` for a review that is not open.
    pub(crate) fn pinned_name(&self, pr: &PrRef) -> Option<String> {
        self.pinned(pr).map(|pinned| pinned.display_name())
    }

    /// The review was saved and closed: its next turns follow the config again.
    pub(crate) fn unpin(&self, pr: &PrRef) {
        self.pinned
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(pr);
    }

    /// The review was saved and closed: its pin and its Codex process go now, or, while a turn
    /// still runs, when the last one ends.
    pub(crate) async fn close_review(&self, shared: &Shared, pr: &PrRef) {
        self.closed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(pr.clone());
        self.release_closed(shared, pr).await;
    }

    /// Releases a closed review once no turn of it runs. A no-op for a review that is open.
    pub(crate) async fn release_closed(&self, shared: &Shared, pr: &PrRef) {
        if self.any_running(pr) {
            return;
        }
        let was_closed = self
            .closed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(pr);
        if was_closed {
            self.unpin(pr);
            crate::codex::close(shared, pr).await;
        }
    }

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
        // A message to a saved review brings it back: no close waits for its turns any more.
        sessions
            .closed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(pr);
        let harness = shared.config.read().await.harness.clone();
        sessions.pin(pr, &harness);
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
            if let Some(head) = &prompt.summary_for
                && slot.summarizing.as_ref().is_some_and(|(_, h)| h == head)
            {
                return Err(Refusal::Summarizing);
            }
            if slot.running.is_some() && slot.queued.is_some() {
                return Err(Refusal::Busy);
            }
            let turn = slot.take_turn(floor);
            if let Some(head) = &prompt.summary_for {
                slot.summarizing = Some((turn, head.clone()));
            }
            if slot.running.is_some() {
                slot.queued = Some(Queued {
                    turn,
                    prompt: prompt.clone(),
                });
                (turn, None)
            } else {
                let stop = Stop::new();
                slot.running = Some(Active {
                    turn,
                    stop: stop.clone(),
                    sandbox: None,
                });
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

    /// Stops the running turn of `pr` and drops the one waiting behind it. When the dropped
    /// turn was the summary, the checks that waited for it go ahead, as they do when a summary
    /// ends: it will never run.
    pub(crate) fn cancel(&self, shared: &Arc<Shared>, pr: &PrRef) {
        if self.stop_with(shared, pr, Why::User) {
            let (shared, pr) = (shared.clone(), pr.clone());
            tokio::spawn(async move { crate::checks::on_summary_over(&shared, &pr).await });
        }
    }

    /// Like [`Sessions::cancel`], for a review that ended: the chat says so, and a turn that
    /// still finishes records and tells nothing. Does not wait, so the review lock may be held.
    pub(crate) fn cancel_ended(&self, shared: &Shared, pr: &PrRef) {
        self.stop_with(shared, pr, Why::Ended);
    }

    /// Stops the running turn and drops the queued one; whether the dropped turn was a summary.
    fn stop_with(&self, shared: &Shared, pr: &PrRef, why: Why) -> bool {
        let (stop, dropped, dropped_summary) = {
            let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
            let Some(slot) = table.get_mut(pr) else {
                return false;
            };
            let queued = slot.queued.take();
            let was_summary = queued.as_ref().is_some_and(|queued| {
                slot.summarizing
                    .as_ref()
                    .is_some_and(|(turn, _)| *turn == queued.turn)
            });
            if let Some(queued) = &queued {
                slot.summary_over(queued.turn);
            }
            (
                slot.running.as_ref().map(|active| active.stop.clone()),
                queued,
                was_summary,
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
        dropped_summary
    }

    /// Ends the session of `pr`: its turn is stopped, its queue dropped and its slot forgotten.
    /// The log stays.
    pub(crate) async fn end(&self, shared: &Shared, pr: &PrRef) {
        self.stop_with(shared, pr, Why::Ended);
        for _ in 0..40 {
            let running = {
                let table = self.table.lock().unwrap_or_else(|p| p.into_inner());
                table.get(pr).is_some_and(|slot| slot.running.is_some())
            };
            if !running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let removed = self
            .table
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(pr)
            .is_some();
        self.pinned
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(pr);
        self.closed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(pr);
        if removed {
            announce(shared, pr, SessionStateKind::None);
        }
    }

    /// A turn id for something that is not a chat message (a check), from the counter the chat
    /// uses, so no two turns of a review share an id.
    pub(crate) async fn next_turn(&self, shared: &Shared, pr: &PrRef) -> u64 {
        let log = shared.paths.agent_log(pr);
        let floor = tokio::task::spawn_blocking(move || agent_log::last_turn(&log))
            .await
            .unwrap_or(0);
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        table.entry(pr.clone()).or_default().take_turn(floor)
    }

    /// Records that `turn`, a check, runs, so the permission bridge it started is heard.
    pub(crate) fn register_check(&self, pr: &PrRef, turn: u64, origin: TurnOrigin) {
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        table
            .entry(pr.clone())
            .or_default()
            .checks
            .push(RunningCheck {
                turn,
                origin,
                sandbox: None,
            });
    }

    /// The check `turn` no longer runs.
    pub(crate) fn unregister_check(&self, pr: &PrRef, turn: u64) {
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(slot) = table.get_mut(pr) {
            slot.checks.retain(|check| check.turn != turn);
        }
    }

    /// Who runs `turn` of `pr` if it runs now: the chat, or the check that registered it.
    pub(crate) fn origin_of(&self, pr: &PrRef, turn: u64) -> Option<TurnOrigin> {
        let table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        let slot = table.get(pr)?;
        if slot
            .running
            .as_ref()
            .is_some_and(|active| active.turn == turn)
        {
            return Some(TurnOrigin::Chat);
        }
        slot.checks
            .iter()
            .find(|check| check.turn == turn)
            .map(|check| check.origin)
    }

    /// The turn `turn` of `pr` ended: if it was the summary, the head may be asked again.
    fn summary_ended(&self, pr: &PrRef, turn: u64) {
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(slot) = table.get_mut(pr) {
            slot.summary_over(turn);
        }
    }

    /// Whether a summary turn of `pr` runs or waits.
    pub(crate) fn summarizing(&self, pr: &PrRef) -> bool {
        let table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        table.get(pr).is_some_and(|slot| slot.summarizing.is_some())
    }

    /// Whether any turn of `pr`, the chat's or a check's, runs now.
    pub(crate) fn any_running(&self, pr: &PrRef) -> bool {
        let table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        table
            .get(pr)
            .is_some_and(|slot| slot.running.is_some() || !slot.checks.is_empty())
    }

    /// Makes `turn` the running turn of `pr` without starting a process.
    #[cfg(test)]
    pub(crate) fn pretend_running(&self, pr: &PrRef, turn: u64) {
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        table.entry(pr.clone()).or_default().running = Some(Active {
            turn,
            stop: Stop::new(),
            sandbox: None,
        });
    }

    /// Forgets the running turn of `pr`, as when its program ended.
    #[cfg(test)]
    pub(crate) fn pretend_stopped(&self, pr: &PrRef) {
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(slot) = table.get_mut(pr) {
            slot.running = None;
        }
    }

    /// Records whether the program of `turn` runs in the sandbox, if `turn` still runs.
    pub(crate) fn set_turn_sandbox(&self, pr: &PrRef, turn: u64, sandbox: bool) {
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        let Some(slot) = table.get_mut(pr) else {
            return;
        };
        if let Some(active) = slot.running.as_mut().filter(|active| active.turn == turn) {
            active.sandbox = Some(sandbox);
        } else if let Some(check) = slot.checks.iter_mut().find(|check| check.turn == turn) {
            check.sandbox = Some(sandbox);
        }
    }

    /// Whether the program of `turn` runs in the sandbox; `None` before its command is built
    /// or once it no longer runs.
    pub(crate) fn turn_sandbox(&self, pr: &PrRef, turn: u64) -> Option<bool> {
        let table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        let slot = table.get(pr)?;
        match slot.running.as_ref().filter(|active| active.turn == turn) {
            Some(active) => active.sandbox,
            None => slot.checks.iter().find(|check| check.turn == turn)?.sandbox,
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

    /// Called when the turn `finished` ended: returns the queued turn, which takes over, or
    /// `None` when the session is ready.
    fn next_after(
        &self,
        shared: &Shared,
        pr: &PrRef,
        finished: u64,
    ) -> Option<(u64, Prompt, Arc<Stop>)> {
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        let slot = table.get_mut(pr)?;
        slot.summary_over(finished);
        match slot.queued.take() {
            Some(queued) => {
                let stop = Stop::new();
                slot.running = Some(Active {
                    turn: queued.turn,
                    stop: stop.clone(),
                    sandbox: None,
                });
                Some((queued.turn, queued.prompt, stop))
            }
            None => {
                slot.running = None;
                announce(shared, pr, SessionStateKind::Ready);
                None
            }
        }
    }

    /// `--resume` for a session `kind` made and the CLI knows, `--session-id` with a fresh uuid
    /// otherwise: a session of another harness is not resumed, and a custom command resumes only
    /// the sessions of the same `program`.
    pub(crate) fn session_for(
        &self,
        shared: &Shared,
        pr: &PrRef,
        kind: HarnessKind,
        program: Option<&str>,
    ) -> SessionArg {
        if let Ok(Some(review)) = reviews::load_stored(shared, pr)
            && let Some(id) = review.session_of(kind, program)
        {
            return SessionArg::Resume(id.to_string());
        }
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        let slot = table.entry(pr.clone()).or_default();
        let id = slot
            .fresh_session
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
        SessionArg::New(id.clone())
    }

    /// A session a check runs in: a fork of the review's session, which keeps the chat's
    /// history out of reach of the check and the check's out of the chat's, or a new session of
    /// its own when the review has none yet, or only one of another harness. Neither is ever
    /// recorded as the review's session.
    pub(crate) fn fork_session(
        &self,
        shared: &Shared,
        pr: &PrRef,
        kind: HarnessKind,
        program: Option<&str>,
    ) -> SessionArg {
        let id = uuid::Uuid::new_v4().to_string();
        match reviews::load_stored(shared, pr) {
            Ok(Some(review)) => match review.session_of(kind, program) {
                Some(parent) => SessionArg::Fork {
                    parent: parent.to_string(),
                    id,
                },
                None => SessionArg::New(id),
            },
            _ => SessionArg::New(id),
        }
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
                format!("{} stopped unexpectedly", current_name(&shared).await),
            );
        }
        // Whatever the turn was still asking, it asks no more.
        crate::permissions::cancel_turn(&shared, &pr, turn).await;
        next = shared.sessions.next_after(&shared, &pr, turn);
    }
    shared.sessions.release_closed(&shared, &pr).await;
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

/// What the chat does with the events of one of its turns: tells the window, writes the log
/// and records the session and the summary.
struct ChatSink<'a> {
    shared: &'a Arc<Shared>,
    out: Out<'a>,
    filter: StreamFilter,
    saw_final: bool,
    failed: bool,
    /// Some answer text already went out as chunks.
    streamed: bool,
    summary_for: Option<String>,
    /// Why the turn was asked to stop: a turn whose review ended records and tells nothing.
    stop: Arc<Stop>,
    /// The agent this turn runs on.
    agent: Agent,
    /// What the pages call it.
    name: String,
    /// The configured program of the turn's harness; a custom command's sessions are its own.
    program: Option<String>,
}

impl ChatSink<'_> {
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
            AgentEvent::Denied { tool, detail } => {
                // A request the reviewer (or the clock) denied is already in the chat.
                if !self
                    .shared
                    .permissions
                    .already_told(self.out.pr, self.out.turn, &tool)
                {
                    self.out.denied(tool, detail);
                }
            }
            AgentEvent::SessionId(id) => {
                remember_session(
                    self.shared,
                    self.out.pr,
                    &id,
                    self.agent.kind(),
                    self.program.as_deref(),
                    &self.name,
                )
                .await
            }
            AgentEvent::Error { kind, message } => self.error(kind, message),
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
                        "" => format!("{} stopped unexpectedly", self.name),
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
                    // Read before the end is told: a terminal that watched the answer leaves
                    // as soon as it hears the end, and was still watching.
                    let watched = self.shared.holds.is_held(self.out.pr);
                    self.out.done(duration_ms);
                    if let Some(head) = self.summary_for.take() {
                        // Checked under the review lock: the end of a review stops the turn
                        // before it takes that lock to forget the summary, so a head written
                        // here is either refused or forgotten after.
                        let stop = self.stop.clone();
                        crate::agent::update_state(self.shared, self.out.pr, move |state| {
                            if stop.why() != Why::Ended {
                                state.last_summary_head = Some(head);
                            }
                        })
                        .await;
                    }
                    crate::agent::refresh(self.shared, self.out.pr);
                    if self.stop.why() != Why::Ended && !watched {
                        crate::agent::notify_finished(self.shared, self.out.pr, self.out.turn)
                            .await;
                    }
                }
            }
        }
    }
}

impl TurnSink for ChatSink<'_> {
    fn waiting(&mut self) {
        announce(self.shared, self.out.pr, SessionStateKind::Queued);
    }

    fn started(&mut self) {
        announce(self.shared, self.out.pr, SessionStateKind::Running);
    }

    fn event<'a>(&'a mut self, event: AgentEvent) -> Boxed<'a> {
        Box::pin(self.handle(event))
    }

    fn error(&mut self, kind: AgentErrorKind, message: String) {
        self.failed = true;
        self.out.error(kind, message);
    }

    /// Lets out the text the suggestion filter still holds.
    fn release(&mut self) {
        let rest = self.filter.finish();
        if !rest.is_empty() {
            self.streamed = true;
            self.out.chunk(rest);
        }
    }

    fn flush(&mut self) {
        self.out.flush();
    }

    fn saw_final(&self) -> bool {
        self.saw_final
    }

    fn failed(&self) -> bool {
        self.failed
    }
}

/// The CLI confirmed the session: from now on turns of `kind` resume it. Stored in the review
/// file together with the harness that made it: its kind, its configured `program` when it is a
/// custom command, and the name the pages used.
async fn remember_session(
    shared: &Shared,
    pr: &PrRef,
    id: &str,
    kind: HarnessKind,
    program: Option<&str>,
    name: &str,
) {
    let _guard = reviews::lock(shared, pr).await;
    if let Ok(Some(mut review)) = reviews::load_stored(shared, pr)
        && review.session_of(kind, program) != Some(id)
    {
        review.remember_session(id, kind, program, name);
        if reviews::save(shared, &review).is_err() {
            tracing::warn!(pr = %pr, "cannot store the agent session in the review");
        }
    }
    shared.sessions.session_confirmed(pr);
}

/// The program no longer knows the chat's session: the review forgets it, so the next turn starts
/// a new one.
pub(crate) async fn forget_session(shared: &Shared, pr: &PrRef) {
    let _guard = reviews::lock(shared, pr).await;
    if let Ok(Some(mut review)) = reviews::load_stored(shared, pr) {
        review.forget_session();
        if reviews::save(shared, &review).is_err() {
            tracing::warn!(pr = %pr, "cannot clear the agent session of the review");
        }
    }
}

/// Runs one chat turn: the process part is `turns::run`, the chat's part is the sink.
async fn run_turn(shared: Arc<Shared>, pr: PrRef, turn: u64, prompt: Prompt, stop: Arc<Stop>) {
    let harness = turns::harness_for(&shared, &pr).await;
    let mut sink = ChatSink {
        shared: &shared,
        out: Out::new(&shared, &pr, turn),
        filter: StreamFilter::default(),
        saw_final: false,
        failed: false,
        streamed: false,
        summary_for: prompt.summary_for.clone(),
        stop: stop.clone(),
        agent: Agent::from(harness.kind),
        name: harness.display_name(),
        program: harness.program.clone(),
    };
    let is_summary = prompt.summary_for.is_some();
    let run = TurnRun {
        pr: pr.clone(),
        turn,
        prompt: prompt.text,
        fresh_prompt: None,
        session: SessionSource::Chat,
        origin: TurnOrigin::Chat,
        places: shared.sessions.permits.clone(),
        limit: None,
        stop,
    };
    let end = turns::run(&shared, run, &mut sink).await;
    // Before the checks start: a reopen from then on must not wait for this summary, since
    // nobody would release what waits for it.
    shared.sessions.summary_ended(&pr, turn);
    if is_summary && !matches!(end, TurnEnd::Stopped(Why::Ended | Why::Shutdown)) {
        crate::checks::on_summary_over(&shared, &pr).await;
    }
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
        let entries = agent_log::read(&path);
        for turn in agent_log::unfinished_turns(&entries) {
            // A check's turn ends in its own kind of line: the chat never sees it.
            let check = entries.iter().find_map(|entry| match entry {
                AgentLogEntry::Check { turn: t, kind, .. } if *t == turn => Some(*kind),
                _ => None,
            });
            let revalidation = entries.iter().any(
                |entry| matches!(entry, AgentLogEntry::Revalidation { turn: t, .. } if *t == turn),
            );
            let entry = match check {
                Some(kind) => AgentLogEntry::Check {
                    at: now_unix(),
                    turn,
                    kind,
                    state: "stopped".into(),
                },
                None if revalidation => AgentLogEntry::Revalidation {
                    at: now_unix(),
                    turn,
                    state: "stopped".into(),
                },
                None => AgentLogEntry::Error {
                    at: now_unix(),
                    turn,
                    kind: AgentErrorKind::Interrupted,
                    message: Why::Shutdown.message().to_string(),
                },
            };
            if let Err(e) = agent_log::append(&path, &entry) {
                tracing::warn!(error = %e, file = %path.display(), "cannot close an unfinished turn");
            }
        }
    }
}

/// What the pages call the harness the config names now.
pub(crate) async fn current_name(shared: &Shared) -> String {
    shared.config.read().await.harness.display_name()
}

/// What the pages call the harness `pr`'s next turn runs on: the one `harness_for` picks, the
/// pinned one or the config's.
pub(crate) async fn name_for(shared: &Shared, pr: &PrRef) -> String {
    turns::harness_for(shared, pr).await.display_name()
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
        summary_for: None,
    };
    match Sessions::submit(shared, pr, prompt).await {
        Ok(turn) => Outcome::Ok(Reply::AgentTurn { turn }),
        Err(refusal) => refusal.into_outcome(&name_for(shared, pr).await),
    }
}

/// The chat to replay: every entry, except the suggestions already accepted or dismissed, so a
/// window that opens later shows only the ones still waiting.
pub(crate) async fn log(shared: &Shared, pr: &PrRef) -> Outcome {
    // A window that reads the log learns from it what was said, and from this where the
    // session stands: whether a turn is running or waiting is not in the log.
    let state = shared
        .sessions
        .states
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(pr)
        .copied()
        .unwrap_or(SessionStateKind::None);
    announce(shared, pr, state);
    let path = shared.paths.agent_log(pr);
    let handled = clusia_store::agent::load_agent_state(&shared.paths, pr).unwrap_or_default();
    match tokio::task::spawn_blocking(move || agent_log::read(&path)).await {
        Ok(entries) => {
            // The checks keep their own results: nothing a check's turn did is part of the chat.
            let checks = agent_log::check_turns(&entries);
            Outcome::Ok(Reply::AgentLog(
                entries
                    .into_iter()
                    .filter(|entry| !checks.contains(&agent_log::turn_of(entry)))
                    .filter(|entry| match entry {
                        AgentLogEntry::Suggestion { suggestion, .. } => {
                            !handled.accepted.contains(&suggestion.id)
                                && !handled.dismissed.contains(&suggestion.id)
                        }
                        _ => true,
                    })
                    .collect(),
            ))
        }
        Err(e) => Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("cannot read the agent log: {e}"),
        )),
    }
}

/// What a working agent told the probe.
struct Found {
    version: String,
    /// Whether the login check passed; `None` for an agent that has none.
    signed_in: Option<bool>,
    account: Option<String>,
}

/// Why the probe found the agent unusable, with what it did learn.
struct Refused {
    version: Option<String>,
    signed_in: Option<bool>,
    error: String,
}

impl Refused {
    fn unknown(error: String) -> Self {
        Self {
            version: None,
            signed_in: None,
            error,
        }
    }
}

/// `line` with an API key hidden: from `sk-` on, nothing of it is kept.
fn mask_keys(line: &str) -> String {
    let at = line.match_indices("sk-").find_map(|(at, _)| {
        let word_start = line[..at]
            .chars()
            .next_back()
            .is_none_or(|before| !before.is_alphanumeric());
        word_start.then_some(at)
    });
    match at {
        Some(at) => format!("{}sk-…", &line[..at]),
        None => line.to_string(),
    }
}

/// `<program> --version` with the environment rules of `agent`'s own turns, so the OpenAI keys
/// and the GitHub tokens never reach it either.
fn version_command(
    agent: Agent,
    program: &Path,
    env: &[(OsString, OsString)],
) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command.arg("--version");
    agent.set_env(&mut command, env);
    command
}

/// Runs a probe command with the probe's time limit.
async fn probe_output(
    command: std::process::Command,
    agent: Agent,
) -> Result<std::process::Output, String> {
    let mut command = Command::from(command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(PROBE_LIMIT, command.output()).await {
        Err(_) => Err(format!(
            "{} did not answer within {} seconds",
            agent.name(),
            PROBE_LIMIT.as_secs()
        )),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(agent.message_for(AgentErrorKind::NotInstalled))
        }
        Ok(Err(e)) => Err(e.to_string()),
        Ok(Ok(output)) => Ok(output),
    }
}

/// Asks `agent` for its version, checks it against the minimum Clúsia needs and, for an agent
/// that has one, asks whether it is signed in.
async fn check_agent(
    agent: Agent,
    program: &Path,
    env: &[(OsString, OsString)],
) -> Result<Found, Refused> {
    let output = probe_output(version_command(agent, program, env), agent)
        .await
        .map_err(Refused::unknown)?;
    let version = if output.status.success() {
        agent.parse_version(&String::from_utf8_lossy(&output.stdout))
    } else {
        let said = String::from_utf8_lossy(&output.stderr);
        Err(match said.trim() {
            "" => format!("{} exited with {}", agent.name(), output.status),
            said => said.to_string(),
        })
    }
    .map_err(Refused::unknown)?;
    if let Some(min) = agent.min_version()
        && !version_at_least(&version, min)
    {
        return Err(Refused {
            error: format!(
                "{} {version} is older than {min}, which Clúsia needs",
                agent.name()
            ),
            version: Some(version),
            signed_in: None,
        });
    }
    let Some(login) = agent.signed_in_command(program, env) else {
        return Ok(Found {
            version,
            signed_in: None,
            account: None,
        });
    };
    let output = match probe_output(login, agent).await {
        Ok(output) => output,
        Err(error) => {
            return Err(Refused {
                version: Some(version),
                signed_in: None,
                error,
            });
        }
    };
    if !output.status.success() {
        return Err(Refused {
            error: format!(
                "{} is not signed in. Run `{} login` in a terminal",
                agent.name(),
                agent.program()
            ),
            version: Some(version),
            signed_in: Some(false),
        });
    }
    // `login status` writes to standard error; the first line says how the user signed in.
    let (stderr, stdout) = (
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    let account = [stderr, stdout]
        .iter()
        .flat_map(|text| text.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(mask_keys);
    Ok(Found {
        version,
        signed_in: Some(true),
        account,
    })
}

/// Asks the program for its version, and its login when it has one: the Config › Harness probe
/// card.
pub(crate) async fn probe(shared: &Shared) -> Outcome {
    let harness = shared.config.read().await.harness.clone();
    let agent = Agent::from(harness.kind);
    if agent == Agent::Custom {
        return Outcome::Ok(Reply::Probe(ProbeResult {
            ok: false,
            version: None,
            program: harness.program.clone().unwrap_or_default(),
            elapsed_ms: 0,
            error: Some("A custom command's probe arrives with its runner".into()),
            harness: harness.display_name(),
            signed_in: None,
            account: None,
        }));
    }
    let program = program_path(shared, &harness);
    let started = Instant::now();
    let env: Vec<_> = std::env::vars_os().collect();
    let checked = check_agent(agent, &program, &env).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let program = program.display().to_string();
    let name = agent.name().to_string();
    Outcome::Ok(Reply::Probe(match checked {
        Ok(found) => ProbeResult {
            ok: true,
            version: Some(found.version),
            program,
            elapsed_ms,
            error: None,
            harness: name,
            signed_in: found.signed_in,
            account: found.account,
        },
        Err(refused) => ProbeResult {
            ok: false,
            version: refused.version,
            program,
            elapsed_ms,
            error: Some(refused.error),
            harness: name,
            signed_in: refused.signed_in,
            account: None,
        },
    }))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clusia_core::config::Harness;
    use clusia_core::{Config, Paths, Review};
    use clusia_harness::testkit::{FakeClaude, Script, Turn};
    use clusia_protocol::AgentLogEntry;
    use clusia_store::save_review;
    use tokio::sync::broadcast;

    use super::*;
    use crate::turns::{TurnEnd, classify_stderr, has_exited};

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
                summary_for: None,
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
            Refusal::Busy.into_outcome("Claude Code"),
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

    /// Writes `body` as an executable shell script in the lab's fake folder and returns its path.
    fn script(lab: &Lab, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = lab.fake.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    /// Waits until `pid` is gone; `false` if it is still there after `limit`.
    async fn gone_within(pid: u32, limit: Duration) -> bool {
        let give_up = Instant::now() + limit;
        while FakeClaude::is_running(pid) {
            if Instant::now() > give_up {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }

    #[tokio::test]
    async fn a_stop_after_the_answer_leaves_the_turn_done() {
        let mut lab = lab(Script::one(Turn::answer("All good.").then_hang()));
        let pr = pr(7);
        lab.send(&pr, "think").await.unwrap();
        let call = wait_calls(&lab, 1).await.remove(0);
        let mut events = until(&mut lab.events, |e| matches!(e, Event::AgentDone { .. })).await;
        lab.shared.sessions.cancel(&lab.shared, &pr);
        events.extend(until(&mut lab.events, ready(&pr)).await);
        assert_eq!(
            error_kind(&events),
            None,
            "a finished turn is not interrupted"
        );
        assert!(
            !FakeClaude::is_running(call.pid),
            "the lingering process is gone"
        );
        let log = lab.log(&pr);
        assert!(matches!(
            log.last(),
            Some(AgentLogEntry::Done { turn: 1, .. })
        ));
        assert!(!log.iter().any(|e| matches!(e, AgentLogEntry::Error { .. })));
    }

    #[tokio::test]
    async fn the_timeout_after_the_answer_leaves_the_turn_done() {
        let mut lab = lab_with(Script::one(Turn::answer("All good.").then_hang()), |c| {
            c.harness.turn_timeout_secs = 1;
        });
        let pr = pr(7);
        lab.send(&pr, "think").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        assert!(events.iter().any(|e| matches!(e, Event::AgentDone { .. })));
        assert_eq!(
            error_kind(&events),
            None,
            "a finished turn is not interrupted"
        );
    }

    #[tokio::test]
    async fn a_stop_kills_what_the_program_started_too() {
        let mut lab = lab(Script::one(Turn::answer("unused")));
        let pid_file = lab.fake.path().join("grandchild.pid");
        let program = script(
            &lab,
            "spawner",
            &format!(
                "sh -c 'trap \"\" TERM; echo $$ > {pid}; exec sleep 3600' </dev/null >/dev/null 2>&1 &\n\
                 echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\"}}'\n\
                 exec sleep 3600\n",
                pid = pid_file.display()
            ),
        );
        lab.shared.config.write().await.harness.program = Some(program);
        let pr = pr(7);
        lab.send(&pr, "think").await.unwrap();
        until(&mut lab.events, state_is(&pr, SessionStateKind::Running)).await;
        let grandchild: u32 = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        lab.shared.sessions.cancel(&lab.shared, &pr);
        until(&mut lab.events, ready(&pr)).await;
        let gone = gone_within(grandchild, Duration::from_secs(2)).await;
        if !gone {
            // SAFETY: only signals the stray test process this test started.
            unsafe { libc::kill(grandchild as i32, libc::SIGKILL) };
        }
        assert!(gone, "the process the program started was left behind");
    }

    #[tokio::test]
    async fn output_that_is_not_utf8_degrades_to_text() {
        let mut lab = lab(Script::one(Turn::answer("unused")));
        let program = script(
            &lab,
            "latin1",
            concat!(
                "printf '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\"}\\n'\n",
                "printf '\\377\\376 noise\\n'\n",
                "printf '{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"caf\\351 ok\"}},\"session_id\":\"s\"}\\n'\n",
                "printf '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":5,\"num_turns\":1,\"result\":\"ok\",\"session_id\":\"s\",\"permission_denials\":[]}\\n'\n",
            ),
        );
        lab.shared.config.write().await.harness.program = Some(program);
        let pr = pr(7);
        lab.send(&pr, "hi").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        assert_eq!(error_kind(&events), None);
        assert!(events.iter().any(|e| matches!(e, Event::AgentDone { .. })));
        assert_eq!(
            texts(&events),
            "\u{fffd}\u{fffd} noise\ncaf\u{fffd} ok",
            "a line that is not JSON shows as text, and the stream goes on"
        );
    }

    #[tokio::test]
    async fn a_turn_that_waited_for_a_place_runs_with_the_config_of_then() {
        let mut lab = lab(Script::one(Turn::hanging()));
        let all = [pr(7), pr(8), pr(9), pr(10)];
        for pr in &all[1..] {
            lab.add_review(pr);
        }
        for pr in &all[..3] {
            lab.send(pr, "go").await.unwrap();
        }
        wait_calls(&lab, 3).await;
        lab.send(&all[3], "go").await.unwrap();
        until(&mut lab.events, state_is(&all[3], SessionStateKind::Queued)).await;
        let later = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(later.path(), Script::one(Turn::answer("later")));
        lab.shared.config.write().await.harness.program = Some(program.display().to_string());
        lab.shared.sessions.cancel(&lab.shared, &all[0]);
        let events = until(&mut lab.events, ready(&all[3])).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::AgentDone { pr, .. } if *pr == all[3])),
            "the waiting turn ran the program set while it waited"
        );
        assert_eq!(FakeClaude::calls(later.path()).len(), 1);
        lab.shared.sessions.shutdown(&lab.shared).await;
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
    async fn a_revalidation_a_stopped_daemon_left_unfinished_is_closed_as_stopped() {
        let home = tempfile::tempdir().unwrap();
        let shared = shared_for(Paths::new(home.path()), Config::default());
        let pr = pr(7);
        let path = shared.paths.agent_log(&pr);
        let run = |state: &str| AgentLogEntry::Revalidation {
            at: 1,
            turn: 3,
            state: state.into(),
        };
        agent_log::append(&path, &run("waiting")).unwrap();
        agent_log::append(&path, &run("running")).unwrap();

        close_unfinished(&shared);

        assert!(matches!(
            agent_log::read(&path).last(),
            Some(AgentLogEntry::Revalidation { turn: 3, state, .. }) if state == "stopped"
        ));
        let count = agent_log::read(&path).len();
        close_unfinished(&shared);
        assert_eq!(agent_log::read(&path).len(), count, "closed only once");
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
    fn only_a_child_that_is_gone_counts_as_exited() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(!has_exited(pid), "a running child has not exited");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(has_exited(pid), "a reaped child is gone");
        assert!(has_exited(std::process::id()), "not a child at all");
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

    #[test]
    fn an_api_key_is_never_kept_in_the_account_line() {
        assert_eq!(
            mask_keys("Logged in using an API key - sk-proj-abcdef0123"),
            "Logged in using an API key - sk-…"
        );
        assert_eq!(mask_keys("sk-live-1234"), "sk-…");
        assert_eq!(
            mask_keys("Logged in using ChatGPT"),
            "Logged in using ChatGPT"
        );
        assert_eq!(
            mask_keys("Logged in as task-bot (ChatGPT)"),
            "Logged in as task-bot (ChatGPT)",
            "a word that only ends in sk- is not a key"
        );
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
        assert_eq!(
            (found.harness.as_str(), found.signed_in, found.account),
            ("Claude Code", None, None)
        );
    }

    /// A program that prints what `codex --version` prints.
    fn version_script(dir: &Path, said: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let program = dir.join("codex");
        std::fs::write(&program, format!("#!/bin/sh\necho '{said}'\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    #[tokio::test]
    async fn the_probe_of_codex_reads_its_version_and_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let program = version_script(dir.path(), "codex-cli 0.162.1");
        let lab = lab_with(Script::one(Turn::answer("x")), |c| {
            c.harness.kind = HarnessKind::Codex;
            c.harness.program = Some(program.display().to_string());
        });
        let Outcome::Ok(Reply::Probe(found)) = probe(&lab.shared).await else {
            panic!("a probe reply");
        };
        assert!(found.ok, "{found:?}");
        assert_eq!(found.version.as_deref(), Some("0.162.1"));
        assert_eq!(found.harness, "Codex");

        let missing = lab_with(Script::one(Turn::answer("x")), |c| {
            c.harness.kind = HarnessKind::Codex;
            c.harness.program = Some("/nonexistent/codex".into());
        });
        let Outcome::Ok(Reply::Probe(found)) = probe(&missing.shared).await else {
            panic!("a probe reply");
        };
        assert_eq!(
            found.error.as_deref(),
            Some("Install Codex or set its path")
        );
        assert_eq!(found.harness, "Codex");
    }

    #[test]
    fn a_version_run_gets_no_github_token_for_either_agent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("agent");
        std::fs::write(&program, "#!/bin/sh\nexec /usr/bin/env\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env: Vec<(OsString, OsString)> = [
            ("GH_TOKEN", "t1"),
            ("GITHUB_TOKEN", "t2"),
            ("CLUSIA_HOME", "/x"),
            ("KEEP", "x"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        for agent in [Agent::ClaudeCode, Agent::Codex] {
            let out = version_command(agent, &program, &env).output().unwrap();
            let seen = String::from_utf8_lossy(&out.stdout);
            assert!(seen.contains("KEEP=x"), "{agent:?}: {seen}");
            for dropped in ["GH_TOKEN=", "GITHUB_TOKEN=", "CLUSIA_"] {
                assert!(!seen.contains(dropped), "{agent:?}: {seen}");
            }
        }
    }

    #[tokio::test]
    async fn the_program_follows_the_harness_kind() {
        let lab = lab(Script::one(Turn::answer("x")));
        let mut harness = Harness::default();
        assert_eq!(
            crate::turns::program_path(&lab.shared, &harness),
            Path::new("/nonexistent/claude")
        );
        harness.kind = HarnessKind::Codex;
        assert_eq!(
            crate::turns::program_path(&lab.shared, &harness),
            Path::new("/nonexistent/codex")
        );
        harness.program = Some(" /opt/tools/codex ".into());
        assert_eq!(
            crate::turns::program_path(&lab.shared, &harness),
            Path::new("/opt/tools/codex"),
            "the configured program wins for either kind"
        );
    }

    #[tokio::test]
    async fn a_session_of_another_kind_is_not_resumed() {
        let lab = lab(Script::one(Turn::answer("x")));
        let pr = pr(7);
        let sessions = &lab.shared.sessions;
        let store = |kind: Option<HarnessKind>| {
            let mut review = reviews::load_stored(&lab.shared, &pr).unwrap().unwrap();
            review.harness_session = Some("thr-1".into());
            review.harness_session_kind = kind;
            save_review(&lab.shared.paths, &review).unwrap();
        };
        store(Some(HarnessKind::Codex));
        let shared = &lab.shared;
        assert_eq!(
            sessions.session_for(shared, &pr, HarnessKind::Codex, None),
            SessionArg::Resume("thr-1".into())
        );
        assert!(matches!(
            sessions.session_for(shared, &pr, HarnessKind::ClaudeCode, None),
            SessionArg::New(_)
        ));
        assert!(matches!(
            sessions.fork_session(shared, &pr, HarnessKind::Codex, None),
            SessionArg::Fork { parent, .. } if parent == "thr-1"
        ));
        assert!(matches!(
            sessions.fork_session(shared, &pr, HarnessKind::ClaudeCode, None),
            SessionArg::New(_)
        ));
        // A session stored before the kind existed is Claude Code's.
        store(None);
        assert_eq!(
            sessions.session_for(shared, &pr, HarnessKind::ClaudeCode, None),
            SessionArg::Resume("thr-1".into())
        );
        assert!(matches!(
            sessions.session_for(shared, &pr, HarnessKind::Codex, None),
            SessionArg::New(_)
        ));
        assert!(matches!(
            sessions.fork_session(shared, &pr, HarnessKind::Codex, None),
            SessionArg::New(_)
        ));
    }

    #[tokio::test]
    async fn the_session_is_stored_with_the_kind_that_made_it() {
        let lab = lab(Script::one(Turn::answer("x")));
        let pr = pr(7);
        remember_session(&lab.shared, &pr, "thr-9", HarnessKind::Codex, None, "Codex").await;
        let stored = reviews::load_stored(&lab.shared, &pr).unwrap().unwrap();
        assert_eq!(stored.harness_session.as_deref(), Some("thr-9"));
        assert_eq!(stored.harness_session_kind, Some(HarnessKind::Codex));
        assert_eq!(
            lab.shared
                .sessions
                .session_for(&lab.shared, &pr, HarnessKind::Codex, None),
            SessionArg::Resume("thr-9".into())
        );
        remember_session(
            &lab.shared,
            &pr,
            "uuid-1",
            HarnessKind::ClaudeCode,
            None,
            "Claude Code",
        )
        .await;
        let stored = reviews::load_stored(&lab.shared, &pr).unwrap().unwrap();
        assert_eq!(
            (
                stored.harness_session.as_deref(),
                stored.harness_session_kind
            ),
            (Some("uuid-1"), Some(HarnessKind::ClaudeCode))
        );
    }

    #[tokio::test]
    async fn a_session_of_another_program_is_not_resumed() {
        let lab = lab(Script::one(Turn::answer("x")));
        let pr = pr(7);
        let shared = &lab.shared;
        remember_session(
            shared,
            &pr,
            "abc",
            HarnessKind::Custom,
            Some("/bin/a"),
            "Echo",
        )
        .await;
        let sessions = &shared.sessions;
        assert_eq!(
            sessions.session_for(shared, &pr, HarnessKind::Custom, Some("/bin/a")),
            SessionArg::Resume("abc".into())
        );
        assert!(matches!(
            sessions.session_for(shared, &pr, HarnessKind::Custom, Some("/bin/b")),
            SessionArg::New(_)
        ));
        assert!(matches!(
            sessions.fork_session(shared, &pr, HarnessKind::Custom, Some("/bin/b")),
            SessionArg::New(_)
        ));
        let stored = reviews::load_stored(shared, &pr).unwrap().unwrap();
        assert_eq!(stored.harness_session_program.as_deref(), Some("/bin/a"));
        assert_eq!(stored.harness_session_name.as_deref(), Some("Echo"));
    }

    #[tokio::test]
    async fn a_custom_pin_holds_when_the_program_or_the_name_changes() {
        let lab = lab_with(Script::one(Turn::answer("x")), |config| {
            config.harness.kind = HarnessKind::Custom;
            config.harness.program = Some("/bin/echo-a".into());
            config.harness.custom_name = "Echo".into();
        });
        let (shared, pr) = (&lab.shared, pr(7));
        let opened = shared.config.read().await.harness.clone();
        shared.sessions.repin(shared, &pr, &opened).await;
        assert_eq!(turns::harness_for(shared, &pr).await, opened);
        {
            let mut config = shared.config.write().await;
            config.harness.program = Some("/bin/echo-b".into());
            config.harness.custom_name = "Other".into();
            config.harness.extra_args = "--fast".into();
        }
        assert_eq!(
            turns::harness_for(shared, &pr).await,
            opened,
            "the pin wins"
        );
        assert_eq!(shared.sessions.pinned_name(&pr).as_deref(), Some("Echo"));
        assert_eq!(name_for(shared, &pr).await, "Echo");
        assert_eq!(current_name(shared).await, "Other");
        shared.sessions.unpin(&pr);
        assert_eq!(name_for(shared, &pr).await, "Other");
    }

    #[tokio::test]
    async fn a_custom_pin_without_a_program_gives_way_to_the_program_set_later() {
        let lab = lab_with(Script::one(Turn::answer("x")), |config| {
            config.harness.kind = HarnessKind::Custom;
            config.harness.program = None;
        });
        let (shared, pr) = (&lab.shared, pr(7));
        let opened = shared.config.read().await.harness.clone();
        assert_eq!(opened.program, None);
        shared.sessions.repin(shared, &pr, &opened).await;
        shared.config.write().await.harness.program = Some("/bin/echo-a".into());
        let now = turns::harness_for(shared, &pr).await;
        assert_eq!(now.program.as_deref(), Some("/bin/echo-a"));
        assert_eq!(name_for(shared, &pr).await, "echo-a");
        // A pin that names another kind still wins.
        shared.config.write().await.harness.kind = HarnessKind::ClaudeCode;
        assert_eq!(
            turns::harness_for(shared, &pr).await.kind,
            HarnessKind::Custom
        );
    }

    #[tokio::test]
    async fn an_open_review_is_named_after_the_program_it_will_run() {
        let lab = lab_with(Script::one(Turn::answer("x")), |config| {
            config.harness.kind = HarnessKind::Custom;
            config.harness.program = Some("/bin/echo-b".into());
            config.harness.custom_name = "Second".into();
        });
        let (shared, pr) = (&lab.shared, pr(7));
        remember_session(
            shared,
            &pr,
            "abc",
            HarnessKind::Custom,
            Some("/bin/echo-a"),
            "First",
        )
        .await;
        let review = reviews::load_stored(shared, &pr).unwrap().unwrap();
        async fn name(shared: &Shared, review: &Review) -> String {
            crate::notifications::harness_name(shared, Some(review)).await
        }
        assert_eq!(
            name(shared, &review).await,
            "First",
            "not open: the session's name"
        );
        let opened = shared.config.read().await.harness.clone();
        shared.sessions.repin(shared, &pr, &opened).await;
        assert_eq!(
            name(shared, &review).await,
            "Second",
            "pinned to a program that did not make it"
        );
        remember_session(
            shared,
            &pr,
            "def",
            HarnessKind::Custom,
            Some("/bin/echo-b"),
            "Second",
        )
        .await;
        let review = reviews::load_stored(shared, &pr).unwrap().unwrap();
        assert_eq!(name(shared, &review).await, "Second");
    }

    #[tokio::test]
    async fn a_codex_pin_over_a_claude_session_keeps_naming_the_session() {
        let lab = lab_with(Script::one(Turn::answer("x")), |config| {
            config.harness.kind = HarnessKind::Codex;
        });
        let (shared, pr) = (&lab.shared, pr(7));
        remember_session(
            shared,
            &pr,
            "uuid-1",
            HarnessKind::ClaudeCode,
            None,
            "Claude Code",
        )
        .await;
        let opened = shared.config.read().await.harness.clone();
        shared.sessions.repin(shared, &pr, &opened).await;
        let review = reviews::load_stored(shared, &pr).unwrap().unwrap();
        assert_eq!(
            crate::notifications::harness_name(shared, Some(&review)).await,
            "Claude Code",
            "the wording of the built-in agents is unchanged"
        );
    }

    #[tokio::test]
    async fn a_built_in_pin_still_lets_program_edits_through() {
        let lab = lab(Script::one(Turn::answer("x")));
        let (shared, pr) = (&lab.shared, pr(7));
        let opened = shared.config.read().await.harness.clone();
        shared.sessions.repin(shared, &pr, &opened).await;
        shared.config.write().await.harness.program = Some("/bin/other-claude".into());
        let now = turns::harness_for(shared, &pr).await;
        assert_eq!(now.program.as_deref(), Some("/bin/other-claude"));
        assert_eq!(
            shared.sessions.pinned_name(&pr).as_deref(),
            Some("Claude Code")
        );
    }

    #[tokio::test]
    async fn a_saved_custom_review_names_the_program_that_answered() {
        let lab = lab(Script::one(Turn::answer("x")));
        let (shared, pr) = (&lab.shared, pr(7));
        remember_session(
            shared,
            &pr,
            "abc",
            HarnessKind::Custom,
            Some("/bin/a"),
            "Echo",
        )
        .await;
        let review = reviews::load_stored(shared, &pr).unwrap().unwrap();
        assert_eq!(
            crate::notifications::harness_name(shared, Some(&review)).await,
            "Echo"
        );
    }

    #[tokio::test]
    async fn a_custom_turn_with_a_missing_program_never_runs_claude_code() {
        let mut lab = lab_with(Script::one(Turn::answer("x")), |config| {
            config.harness.kind = HarnessKind::Custom;
            config.harness.program = Some("/bin/echo-a".into());
        });
        let pr = pr(7);
        lab.send(&pr, "hi").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        assert_eq!(error_kind(&events), Some(AgentErrorKind::NotInstalled));
        assert!(lab.calls().is_empty(), "no Claude Code command line ran");
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
    async fn the_log_command_leaves_out_the_checks() {
        let mut lab = lab(Script::one(Turn::answer("hello")));
        let pr = pr(7);
        lab.send(&pr, "hi").await.unwrap();
        until(&mut lab.events, ready(&pr)).await;
        agent_log::append(
            &lab.shared.paths.agent_log(&pr),
            &AgentLogEntry::Check {
                at: 5,
                turn: 2,
                kind: clusia_core::CheckKind::Security,
                state: "running".into(),
            },
        )
        .unwrap();
        // What the check's turn did is not part of the chat either: its tools, its permission
        // lines and its end.
        let path = lab.shared.paths.agent_log(&pr);
        agent_log::append(
            &path,
            &AgentLogEntry::Permission {
                at: 6,
                turn: 2,
                tool: "Bash".into(),
                summary: "cargo test".into(),
                outcome: clusia_protocol::PermissionOutcome::Cancelled,
            },
        )
        .unwrap();
        agent_log::append(
            &path,
            &AgentLogEntry::ToolUse {
                at: 7,
                turn: 2,
                summary: "Read src/a.rs".into(),
            },
        )
        .unwrap();
        let Outcome::Ok(Reply::AgentLog(entries)) = log(&lab.shared, &pr).await else {
            panic!("a log reply");
        };
        assert_eq!(entries.len(), 3, "the chat's own turn is all that is left");
        assert!(entries.iter().all(|e| agent_log::turn_of(e) == 1));
    }

    #[tokio::test]
    async fn a_check_turn_a_stopped_daemon_left_unfinished_ends_as_a_check() {
        let home = tempfile::tempdir().unwrap();
        let shared = shared_for(Paths::new(home.path()), Config::default());
        let pr = pr(7);
        let path = shared.paths.agent_log(&pr);
        agent_log::append(
            &path,
            &AgentLogEntry::Check {
                at: 1,
                turn: 2,
                kind: clusia_core::CheckKind::Audit,
                state: "running".into(),
            },
        )
        .unwrap();
        close_unfinished(&shared);
        let log = agent_log::read(&path);
        assert!(
            matches!(
                log.last(),
                Some(AgentLogEntry::Check { turn: 2, kind: clusia_core::CheckKind::Audit, state, .. })
                    if state == "stopped"
            ),
            "{log:?}"
        );
        assert!(
            !log.iter().any(|e| matches!(e, AgentLogEntry::Error { .. })),
            "the chat never gets an error line for a check"
        );
        let count = log.len();
        close_unfinished(&shared);
        assert_eq!(agent_log::read(&path).len(), count, "closed only once");
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
            Path::new("/nonexistent/claude"),
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

    #[tokio::test]
    async fn a_summary_is_over_before_the_checks_that_waited_for_it_start() {
        let mut lab = lab_with(
            Script::one(Turn::answer("The summary.").delay_ms(300)),
            |c| {
                c.harness.check_security = false;
                c.harness.audit = false;
            },
        );
        let pr = pr(7);
        let prompt = Prompt {
            text: "Summarize".into(),
            shown: false,
            summary_for: Some("abc123".into()),
        };
        Sessions::submit(&lab.shared, &pr, prompt).await.unwrap();
        // The session is remembered before the text streams. After the end the turn records
        // the summary's head under the review lock and the checks then wait for that lock; a
        // second hold queued behind the first (the lock is fair) keeps the checks waiting,
        // which is the gap a reopen can fall into.
        until(&mut lab.events, |e| matches!(e, Event::AgentChunk { .. })).await;
        let first = reviews::lock(&lab.shared, &pr).await;
        until(&mut lab.events, |e| matches!(e, Event::AgentDone { .. })).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let second = {
            let (shared, pr) = (lab.shared.clone(), pr.clone());
            tokio::spawn(async move { reviews::lock(&shared, &pr).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(first);
        let guard = second.await.unwrap();
        let over = tokio::time::timeout(Duration::from_secs(3), async {
            while lab.shared.sessions.summarizing(&pr) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        drop(guard);
        assert!(over.is_ok(), "a reopen now must not wait for the summary");
        until(&mut lab.events, ready(&pr)).await;
    }

    #[tokio::test]
    async fn a_summary_turn_records_the_head_it_described() {
        let mut lab = lab(Script::one(Turn::answer("The summary.")));
        let pr = pr(7);
        let prompt = Prompt {
            text: "Summarize".into(),
            shown: false,
            summary_for: Some("abc123".into()),
        };
        Sessions::submit(&lab.shared, &pr, prompt).await.unwrap();
        until(&mut lab.events, ready(&pr)).await;
        let state = clusia_store::agent::load_agent_state(&lab.shared.paths, &pr).unwrap();
        assert_eq!(state.last_summary_head.as_deref(), Some("abc123"));
        let log = lab.log(&pr);
        assert!(
            !log.iter().any(|e| matches!(e, AgentLogEntry::User { .. })),
            "a prompt the daemon wrote is not shown as the user's"
        );
        assert_eq!(
            agent_log::last_summary(&log).as_deref(),
            Some("The summary.")
        );
    }

    #[tokio::test]
    async fn ending_a_session_stops_its_turn_and_dismisses_what_waited() {
        let answer =
            "```clusia-suggestion\n{\"file\":\"src/a.rs\",\"line\":3,\"body\":\"Why?\"}\n```\n";
        let mut lab = lab(Script::turns(vec![Turn::answer(answer), Turn::hanging()]));
        let pr = pr(7);
        lab.send(&pr, "review").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        let id = events
            .iter()
            .find_map(|e| match e {
                Event::AgentSuggestion { suggestion, .. } => Some(suggestion.id.clone()),
                _ => None,
            })
            .unwrap();
        lab.send(&pr, "and now").await.unwrap();
        let call = wait_calls(&lab, 2).await.remove(1);

        crate::agent::stop(&lab.shared, &pr).await;
        crate::agent::forget(&lab.shared, &pr);

        assert!(!FakeClaude::is_running(call.pid));
        assert_eq!(lab.shared.sessions.state(&pr), SessionStateKind::None);
        let state = clusia_store::agent::load_agent_state(&lab.shared.paths, &pr).unwrap();
        assert!(state.dismissed.contains(&id));
        assert_eq!(state.last_summary_head, None);
        assert!(!lab.log(&pr).is_empty(), "the log stays");
    }

    #[tokio::test]
    async fn two_accepts_at_once_add_one_item() {
        let answer =
            "```clusia-suggestion\n{\"file\":\"src/a.rs\",\"line\":2,\"body\":\"Why?\"}\n```\n";
        let mut lab = lab(Script::one(Turn::answer(answer)));
        let pr = pr(7);
        lab.send(&pr, "review").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        let id = events
            .iter()
            .find_map(|e| match e {
                Event::AgentSuggestion { suggestion, .. } => Some(suggestion.id.clone()),
                _ => None,
            })
            .unwrap();
        let files = vec![clusia_core::FileDiff {
            path: "src/a.rs".into(),
            previous_path: None,
            status: "added".into(),
            additions: 3,
            deletions: 0,
            patch: Some("@@ -0,0 +1,3 @@\n+a\n+b\n+c".into()),
        }];
        lab.shared
            .files_cache
            .lock()
            .await
            .insert(pr.clone(), ("h".into(), Arc::new(files)));

        // Both accepts see the suggestion waiting before either can take the review.
        let guard = reviews::lock(&lab.shared, &pr).await;
        let accept = |shared: Arc<Shared>, pr: PrRef, id: String| async move {
            crate::agent::accept(&shared, "test", &pr, &id, None).await
        };
        let first = tokio::spawn(accept(lab.shared.clone(), pr.clone(), id.clone()));
        let second = tokio::spawn(accept(lab.shared.clone(), pr.clone(), id.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(guard);
        let outcomes = [first.await.unwrap(), second.await.unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| matches!(o, Outcome::Ok(Reply::DraftItem(_))))
                .count(),
            1,
            "{outcomes:?}"
        );
        let review = reviews::load_stored(&lab.shared, &pr).unwrap().unwrap();
        assert_eq!(review.draft.items.len(), 1);
    }

    #[tokio::test]
    async fn a_dismiss_waiting_behind_an_accept_finds_the_suggestion_gone() {
        let answer =
            "```clusia-suggestion\n{\"file\":\"src/a.rs\",\"line\":2,\"body\":\"Why?\"}\n```\n";
        let mut lab = lab(Script::one(Turn::answer(answer)));
        let pr = pr(7);
        lab.send(&pr, "review").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        let id = events
            .iter()
            .find_map(|e| match e {
                Event::AgentSuggestion { suggestion, .. } => Some(suggestion.id.clone()),
                _ => None,
            })
            .unwrap();
        let files = vec![clusia_core::FileDiff {
            path: "src/a.rs".into(),
            previous_path: None,
            status: "added".into(),
            additions: 3,
            deletions: 0,
            patch: Some("@@ -0,0 +1,3 @@\n+a\n+b\n+c".into()),
        }];
        lab.shared
            .files_cache
            .lock()
            .await
            .insert(pr.clone(), ("h".into(), Arc::new(files)));

        // The accept waits for the review first; the dismiss comes while it waits.
        let guard = reviews::lock(&lab.shared, &pr).await;
        let accept = tokio::spawn({
            let (shared, pr, id) = (lab.shared.clone(), pr.clone(), id.clone());
            async move { crate::agent::accept(&shared, "test", &pr, &id, None).await }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let dismiss = tokio::spawn({
            let (shared, pr, id) = (lab.shared.clone(), pr.clone(), id.clone());
            async move { crate::agent::dismiss(&shared, &pr, &id).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(guard);
        assert!(matches!(
            accept.await.unwrap(),
            Outcome::Ok(Reply::DraftItem(_))
        ));
        assert!(
            matches!(dismiss.await.unwrap(), Outcome::Err(ref e) if e.code == ErrorCode::NotFound)
        );
        let state = clusia_store::agent::load_agent_state(&lab.shared.paths, &pr).unwrap();
        assert!(state.accepted.contains(&id));
        assert!(
            !state.dismissed.contains(&id),
            "accepted, never also dismissed"
        );
    }

    #[tokio::test]
    async fn a_refused_discard_keeps_the_agent() {
        let lab = lab(Script::one(Turn::hanging()));
        let pr = pr(7);
        lab.send(&pr, "review").await.unwrap();
        let call = wait_calls(&lab, 1).await.remove(0);
        let mut review = reviews::load_stored(&lab.shared, &pr).unwrap().unwrap();
        review.state = clusia_core::ReviewState::Publishing;
        save_review(&lab.shared.paths, &review).unwrap();

        let outcome = reviews::discard(&lab.shared, "test", &pr).await;
        assert!(
            matches!(&outcome, Outcome::Err(e) if e.code == ErrorCode::InvalidState),
            "{outcome:?}"
        );
        assert_eq!(lab.shared.sessions.state(&pr), SessionStateKind::Running);
        assert!(FakeClaude::is_running(call.pid));
        lab.shared.sessions.end(&lab.shared, &pr).await;
    }

    #[tokio::test]
    async fn a_turn_that_ends_after_its_review_ended_records_and_tells_nothing() {
        let mut lab = lab(Script::one(Turn::answer("x")));
        let pr = pr(7);
        let finish = |stop: Arc<Stop>| {
            let shared = lab.shared.clone();
            let pr = pr.clone();
            async move {
                let mut run = ChatSink {
                    shared: &shared,
                    out: Out::new(&shared, &pr, 1),
                    filter: StreamFilter::default(),
                    saw_final: false,
                    failed: false,
                    streamed: false,
                    summary_for: Some("abc123".into()),
                    stop,
                    agent: Agent::ClaudeCode,
                    name: "Claude Code".into(),
                    program: None,
                };
                run.handle(AgentEvent::Final {
                    text: "The summary.".into(),
                    duration_ms: 5,
                    is_error: false,
                })
                .await;
            }
        };
        let told = |events: &mut Events| {
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|(_, e)| matches!(e, Event::InboxChanged { .. }))
        };

        let ended = Stop::new();
        ended.fire(Why::Ended);
        finish(ended).await;
        let state =
            clusia_store::agent::load_agent_state(&lab.shared.paths, &pr).unwrap_or_default();
        assert_eq!(state.last_summary_head, None);
        assert!(
            !told(&mut lab.events),
            "no notification for an ended review"
        );

        finish(Stop::new()).await;
        let state = clusia_store::agent::load_agent_state(&lab.shared.paths, &pr).unwrap();
        assert_eq!(state.last_summary_head.as_deref(), Some("abc123"));
        assert!(told(&mut lab.events), "a live review is notified");
    }

    #[tokio::test]
    async fn closing_an_empty_review_says_the_review_ended() {
        let lab = lab(Script::one(Turn::hanging()));
        let pr = pr(7);
        lab.send(&pr, "review").await.unwrap();
        wait_calls(&lab, 1).await;
        assert!(matches!(
            reviews::close(&lab.shared, "test", &pr).await,
            Outcome::Ok(Reply::Ack)
        ));
        let mut message = None;
        for _ in 0..50 {
            message = lab.log(&pr).into_iter().find_map(|e| match e {
                AgentLogEntry::Error { message, .. } => Some(message),
                _ => None,
            });
            if message.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(message.as_deref(), Some("Stopped because the review ended"));
    }

    /// Remembers what a run that is not the chat was told.
    #[derive(Default)]
    struct Recorder {
        waited: bool,
        started: bool,
        errors: Vec<(AgentErrorKind, String)>,
        session_ids: Vec<String>,
        saw_final: bool,
        failed: bool,
    }

    impl TurnSink for Recorder {
        fn waiting(&mut self) {
            self.waited = true;
        }

        fn started(&mut self) {
            self.started = true;
        }

        fn event<'a>(&'a mut self, event: AgentEvent) -> Boxed<'a> {
            Box::pin(async move {
                match event {
                    AgentEvent::SessionId(id) => self.session_ids.push(id),
                    AgentEvent::Final { is_error, .. } => {
                        self.saw_final = true;
                        self.failed |= is_error;
                    }
                    AgentEvent::Error { .. } => self.failed = true,
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

    fn check_run(
        turn: u64,
        session: SessionSource,
        places: Arc<Semaphore>,
        limit: Option<Duration>,
    ) -> (TurnRun, Arc<Stop>) {
        let stop = Stop::new();
        let run = TurnRun {
            pr: pr(7),
            turn,
            prompt: "Check this pull request.".into(),
            fresh_prompt: Some("Read .clusia/review.md first. Check this pull request.".into()),
            session,
            origin: TurnOrigin::Security,
            places,
            limit,
            stop: stop.clone(),
        };
        (run, stop)
    }

    /// Runs `run` in a task of its own and gives back how it ended and what its sink heard.
    fn start(lab: &Lab, run: TurnRun) -> tokio::task::JoinHandle<(TurnEnd, Recorder)> {
        let shared = lab.shared.clone();
        tokio::spawn(async move {
            let mut sink = Recorder::default();
            let end = turns::run(&shared, run, &mut sink).await;
            (end, sink)
        })
    }

    fn store_session(lab: &Lab, pr: &PrRef, id: &str) {
        let mut review = reviews::load_stored(&lab.shared, pr).unwrap().unwrap();
        review.harness_session = Some(id.into());
        save_review(&lab.shared.paths, &review).unwrap();
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let at = args.iter().position(|a| a == flag).expect(flag);
        &args[at + 1]
    }

    #[tokio::test]
    async fn a_fork_run_forks_the_review_session() {
        let lab = lab(Script::one(Turn::answer("Found no security problems.")));
        let pr = pr(7);
        store_session(&lab, &pr, "parent-session");
        let (run, _stop) = check_run(5, SessionSource::Fork, Arc::new(Semaphore::new(2)), None);
        let (end, sink) = start(&lab, run).await.unwrap();
        assert_eq!(end, TurnEnd::Done);
        assert!(sink.started && !sink.waited);
        let args = wait_calls(&lab, 1).await.remove(0).argv;
        let at = args.iter().position(|a| a == "--resume").unwrap();
        assert_eq!(
            args[at..at + 3],
            ["--resume", "parent-session", "--fork-session"]
        );
        let fork = value_after(&args, "--session-id");
        assert_ne!(fork, "parent-session");
        assert_eq!(fork.len(), 36, "a new uuid");
        assert_eq!(value_after(&args, "-p"), "Check this pull request.");
        assert_eq!(sink.session_ids, [fork.to_string()]);
        let review = reviews::load_stored(&lab.shared, &pr).unwrap().unwrap();
        assert_eq!(
            review.harness_session.as_deref(),
            Some("parent-session"),
            "the chat's session is never replaced by a fork's"
        );
        assert_eq!(
            FakeClaude::history(lab.fake.path(), fork),
            ["Check this pull request."]
        );
    }

    #[tokio::test]
    async fn a_fork_without_a_review_session_starts_its_own() {
        let lab = lab(Script::one(Turn::answer("ok")));
        let pr = pr(7);
        let (run, _stop) = check_run(5, SessionSource::Fork, Arc::new(Semaphore::new(2)), None);
        let (end, _) = start(&lab, run).await.unwrap();
        assert_eq!(end, TurnEnd::Done);
        let args = wait_calls(&lab, 1).await.remove(0).argv;
        assert!(!args.contains(&"--resume".to_string()));
        assert!(!args.contains(&"--fork-session".to_string()));
        assert_eq!(value_after(&args, "--session-id").len(), 36);
        assert_eq!(
            value_after(&args, "-p"),
            "Read .clusia/review.md first. Check this pull request.",
            "a session of its own has not read the review yet"
        );
        let review = reviews::load_stored(&lab.shared, &pr).unwrap().unwrap();
        assert_eq!(review.harness_session, None, "it never becomes the chat's");
    }

    #[tokio::test]
    async fn a_given_session_is_used_as_it_is() {
        let lab = lab(Script::one(Turn::answer("ok")));
        let (run, _stop) = check_run(
            5,
            SessionSource::Given(SessionArg::Resume("elsewhere".into())),
            Arc::new(Semaphore::new(2)),
            None,
        );
        assert_eq!(start(&lab, run).await.unwrap().0, TurnEnd::Done);
        let args = wait_calls(&lab, 1).await.remove(0).argv;
        assert_eq!(value_after(&args, "--resume"), "elsewhere");
    }

    #[tokio::test]
    async fn a_check_never_widens_what_the_chat_may_do() {
        let lab = lab_with(Script::one(Turn::answer("ok")), |config| {
            config.harness.sandbox = true;
            config.harness.use_cli_permissions = false;
            config.harness.extra_args = "--model opus".into();
        });
        let pr = pr(7);
        store_session(&lab, &pr, "parent-session");
        lab.send(&pr, "hello").await.unwrap();
        wait_calls(&lab, 1).await;
        let (run, _stop) = check_run(9, SessionSource::Fork, Arc::new(Semaphore::new(2)), None);
        assert_eq!(start(&lab, run).await.unwrap().0, TurnEnd::Done);
        let calls = wait_calls(&lab, 2).await;
        let flags = |args: &[String]| -> Vec<String> {
            args.iter()
                .filter(|a| a.starts_with("--"))
                .filter(|a| !["--resume", "--fork-session", "--session-id"].contains(&a.as_str()))
                .cloned()
                .collect()
        };
        let (chat, check) = (&calls[0].argv, &calls[1].argv);
        assert_eq!(
            flags(chat),
            flags(check),
            "the same flags, in the same order"
        );
        for flag in [
            "--permission-mode",
            "--permission-prompt-tool",
            "--settings",
            "--setting-sources",
            "--append-system-prompt",
            "--model",
        ] {
            assert_eq!(value_after(chat, flag), value_after(check, flag), "{flag}");
        }
        let allowed = |args: &[String]| {
            args[args.iter().position(|a| a == "--allowedTools").unwrap() + 1..][..4].to_vec()
        };
        assert_eq!(allowed(chat), allowed(check));
        assert_eq!(allowed(check), ["Read", "Grep", "Glob", "LS"]);
        let bridge = |args: &[String]| -> serde_json::Value {
            serde_json::from_str(value_after(args, "--mcp-config")).unwrap()
        };
        let (a, b) = (bridge(chat), bridge(check));
        assert_eq!(
            a["mcpServers"]["clusia"]["args"].as_array().unwrap()[..6],
            b["mcpServers"]["clusia"]["args"].as_array().unwrap()[..6],
            "the same bridge, for the same socket and review"
        );
        assert_eq!(
            b["mcpServers"]["clusia"]["args"][6], "9",
            "with the turn of the check"
        );
        assert_eq!(calls[0].cwd, calls[1].cwd);
        assert_eq!(calls[0].env, calls[1].env, "the same environment");
    }

    #[tokio::test]
    async fn a_check_is_known_to_the_daemon_only_while_it_runs() {
        let lab = lab(Script::one(Turn::hanging()));
        let pr = pr(7);
        assert_eq!(lab.shared.sessions.origin_of(&pr, 5), None);
        let (run, stop) = check_run(5, SessionSource::Fork, Arc::new(Semaphore::new(2)), None);
        let task = start(&lab, run);
        let call = wait_calls(&lab, 1).await.remove(0);
        assert_eq!(
            lab.shared.sessions.origin_of(&pr, 5),
            Some(TurnOrigin::Security)
        );
        assert!(lab.shared.sessions.any_running(&pr));
        assert_eq!(
            lab.shared.sessions.origin_of(&pr, 6),
            None,
            "only the turn that runs may ask"
        );
        stop.fire(Why::User);
        let (end, sink) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Stopped(Why::User));
        assert_eq!(
            sink.errors,
            [(AgentErrorKind::Interrupted, "Stopped by you".to_string())]
        );
        assert_eq!(lab.shared.sessions.origin_of(&pr, 5), None);
        assert!(!lab.shared.sessions.any_running(&pr));
        assert!(gone_within(call.pid, Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn a_run_stopped_before_it_starts_starts_no_process() {
        let lab = lab(Script::one(Turn::answer("ok")));
        let (run, stop) = check_run(5, SessionSource::Fork, Arc::new(Semaphore::new(2)), None);
        stop.fire(Why::Ended);
        let (end, sink) = start(&lab, run).await.unwrap();
        assert_eq!(end, TurnEnd::Stopped(Why::Ended));
        assert!(!sink.started);
        assert_eq!(
            sink.errors,
            [(
                AgentErrorKind::Interrupted,
                "Stopped because the review ended".to_string()
            )]
        );
        assert!(
            lab.calls().is_empty(),
            "no process, not even one killed at once"
        );
        assert_eq!(lab.shared.sessions.origin_of(&pr(7), 5), None);
    }

    #[tokio::test]
    async fn a_run_waits_for_a_place_of_its_own() {
        let lab = lab(Script::one(Turn::answer("ok")));
        let places = Arc::new(Semaphore::new(1));
        let held = places.clone().acquire_owned().await.unwrap();
        let (run, _stop) = check_run(5, SessionSource::Fork, places, None);
        let task = start(&lab, run);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(lab.calls().is_empty(), "no process before a place is free");
        assert_eq!(lab.shared.sessions.origin_of(&pr(7), 5), None);
        drop(held);
        let (end, sink) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Done);
        assert!(sink.waited && sink.started);
    }

    #[tokio::test]
    async fn a_full_place_for_checks_does_not_hold_the_chat() {
        let mut lab = lab(Script::one(Turn::answer("hi")));
        let pr = pr(7);
        let places = Arc::new(Semaphore::new(0));
        let (run, stop) = check_run(5, SessionSource::Fork, places, None);
        let task = start(&lab, run);
        lab.send(&pr, "hello").await.unwrap();
        until(&mut lab.events, ready(&pr)).await;
        assert_eq!(lab.calls().len(), 1, "only the chat's process");
        stop.fire(Why::User);
        assert_eq!(task.await.unwrap().0, TurnEnd::Stopped(Why::User));
    }

    #[tokio::test]
    async fn stopping_a_run_that_waits_starts_no_process() {
        let lab = lab(Script::one(Turn::answer("ok")));
        let places = Arc::new(Semaphore::new(1));
        let _held = places.clone().acquire_owned().await.unwrap();
        let (run, stop) = check_run(5, SessionSource::Fork, places, None);
        let task = start(&lab, run);
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.fire(Why::Ended);
        let (end, sink) = task.await.unwrap();
        assert_eq!(end, TurnEnd::Stopped(Why::Ended));
        assert!(sink.waited && !sink.started);
        assert_eq!(
            sink.errors,
            [(
                AgentErrorKind::Interrupted,
                "Stopped because the review ended".to_string()
            )]
        );
        assert!(lab.calls().is_empty());
    }

    #[tokio::test]
    async fn a_run_past_its_limit_is_stopped() {
        let lab = lab(Script::one(Turn::hanging()));
        let (run, _stop) = check_run(
            5,
            SessionSource::Fork,
            Arc::new(Semaphore::new(2)),
            Some(Duration::from_secs(1)),
        );
        let task = start(&lab, run);
        let call = wait_calls(&lab, 1).await.remove(0);
        let (end, sink) = task.await.unwrap();
        assert_eq!(end, TurnEnd::TimedOut);
        assert_eq!(
            sink.errors,
            [(
                AgentErrorKind::Interrupted,
                "The turn ran past its limit of 1s and was stopped".to_string()
            )]
        );
        assert!(gone_within(call.pid, Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn a_run_that_fails_to_start_says_so() {
        let lab = lab_with(Script::one(Turn::answer("x")), |config| {
            config.harness.program = Some("/nonexistent/claude".into());
        });
        let (run, _stop) = check_run(5, SessionSource::Fork, Arc::new(Semaphore::new(2)), None);
        let (end, sink) = start(&lab, run).await.unwrap();
        assert_eq!(end, TurnEnd::Failed);
        assert_eq!(sink.errors.len(), 1);
        assert_eq!(sink.errors[0].0, AgentErrorKind::NotInstalled);
        assert_eq!(lab.shared.sessions.origin_of(&pr(7), 5), None);
    }

    #[tokio::test]
    async fn a_program_that_ends_without_an_answer_fails_the_run() {
        let lab = lab(Script::one(
            Turn::lines(&[]).exit(1).stderr("Error: Not logged in\n"),
        ));
        let (run, _stop) = check_run(5, SessionSource::Fork, Arc::new(Semaphore::new(2)), None);
        let (end, sink) = start(&lab, run).await.unwrap();
        assert_eq!(end, TurnEnd::Failed);
        assert_eq!(sink.errors[0].0, AgentErrorKind::NotSignedIn);
    }

    #[tokio::test]
    async fn the_chat_and_the_checks_share_one_turn_counter() {
        let mut lab = lab(Script::one(Turn::answer("hi")));
        let pr = pr(7);
        assert_eq!(lab.shared.sessions.next_turn(&lab.shared, &pr).await, 1);
        let chat = lab.send(&pr, "hello").await.unwrap();
        assert_eq!(chat, 2);
        until(&mut lab.events, ready(&pr)).await;
        assert_eq!(lab.shared.sessions.next_turn(&lab.shared, &pr).await, 3);
        assert_eq!(lab.send(&pr, "again").await.unwrap(), 4);
        until(&mut lab.events, ready(&pr)).await;
        let other = self::pr(8);
        lab.add_review(&other);
        assert_eq!(
            lab.shared.sessions.next_turn(&lab.shared, &other).await,
            1,
            "each review counts for itself"
        );
    }

    #[tokio::test]
    async fn the_counter_continues_from_the_log() {
        let lab = lab(Script::one(Turn::answer("hi")));
        let pr = pr(7);
        agent_log::append(
            &lab.shared.paths.agent_log(&pr),
            &AgentLogEntry::Check {
                at: 1,
                turn: 10,
                kind: clusia_core::CheckKind::Audit,
                state: "done".into(),
            },
        )
        .unwrap();
        assert_eq!(lab.shared.sessions.next_turn(&lab.shared, &pr).await, 11);
        assert_eq!(lab.send(&pr, "hello").await.unwrap(), 12);
    }

    #[tokio::test]
    async fn the_chat_answers_while_a_check_runs() {
        let mut lab = lab(Script::turns(vec![Turn::hanging(), Turn::answer("hi")]));
        let pr = pr(7);
        let (run, stop) = check_run(5, SessionSource::Fork, Arc::new(Semaphore::new(2)), None);
        let task = start(&lab, run);
        wait_calls(&lab, 1).await;
        lab.send(&pr, "hello").await.unwrap();
        let events = until(&mut lab.events, ready(&pr)).await;
        assert!(texts(&events).contains("hi"));
        assert_eq!(lab.shared.sessions.state(&pr), SessionStateKind::Ready);
        assert_eq!(
            lab.shared.sessions.origin_of(&pr, 5),
            Some(TurnOrigin::Security),
            "the check still runs"
        );
        stop.fire(Why::User);
        task.await.unwrap();
    }
}
