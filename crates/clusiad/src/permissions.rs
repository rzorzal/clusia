//! Permission requests: what the agent may not do on its own is decided here. A request that a
//! rule of the review covers is allowed at once; an edit outside the worktree is denied at
//! once; any other waits for the reviewer (the window, a notification or `clusia ask`) until
//! the deadline. Only an answer allows: no answer, a stop, the end of the turn or of the
//! review, and a shutdown all deny.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clusia_core::PrRef;
use clusia_core::notify::NotifyEvent;
use clusia_core::permissions::{
    covers, detail_for, inside_worktree, prefix_for, rule_for_bash, rule_for_tool, summary_for,
};
use clusia_protocol::{
    AgentLogEntry, ErrorCode, Event, Outcome, PermissionAnswerKind, PermissionOutcome,
    PermissionRequest, ProtocolError, Reply, topics,
};
use clusia_store::agent::load_agent_state;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::agent;
use crate::agent_log;
use crate::notifications;
use crate::reviews;
use crate::state::Shared;
use crate::sync::now_unix;

/// The tools that change a file; for them the request is about a path.
const FILE_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// What the agent is told when the reviewer denied the request.
const DENIED: &str = "The reviewer denied this request.";
/// What the agent is told when nobody answered before the deadline.
const EXPIRED: &str = "The reviewer did not answer in time, so the request was denied.";
/// What the agent is told when the turn or the review ended while it waited.
const CANCELLED: &str = "The request was cancelled because the turn ended.";
/// What the agent is told when it asks for a file outside the review's worktree.
const OUTSIDE: &str = "Clúsia does not let the agent change files outside the review's worktree.";

/// What the agent is told when it asks to run a command outside the sandbox while it is on.
const UNSANDBOXED: &str =
    "Clúsia keeps commands in the sandbox; turn the sandbox off in Config › Harness to allow this";

/// One tool request, as the bridge forwards it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PermissionAsk {
    pub pr: PrRef,
    pub turn: u64,
    pub tool: String,
    pub input: Value,
}

/// What the bridge tells the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionDecision {
    pub allow: bool,
    pub message: Option<String>,
}

impl PermissionDecision {
    fn allowed() -> Self {
        Self {
            allow: true,
            message: None,
        }
    }

    fn denied(message: &str) -> Self {
        Self {
            allow: false,
            message: Some(message.to_string()),
        }
    }

    fn of(outcome: PermissionOutcome) -> Self {
        match outcome {
            PermissionOutcome::Allowed | PermissionOutcome::AllowedForReview => Self::allowed(),
            PermissionOutcome::Denied => Self::denied(DENIED),
            PermissionOutcome::Expired => Self::denied(EXPIRED),
            PermissionOutcome::Cancelled => Self::denied(CANCELLED),
        }
    }
}

/// A request waiting for an answer.
struct Pending {
    /// What a window is told, now or when it asks later.
    request: PermissionRequest,
    /// The order requests arrived in.
    seq: u64,
    /// The rule "Allow for this review" would save; `None` when only Allow once is offered.
    rule: Option<String>,
    done: oneshot::Sender<PermissionOutcome>,
}

/// The requests waiting for an answer, and the denials already told.
#[derive(Default)]
pub(crate) struct Permissions {
    pending: Mutex<HashMap<String, Pending>>,
    next_seq: AtomicU64,
    /// The last turn of each review that ended: it asks no more, even while its program
    /// is still being stopped.
    closed: Mutex<HashMap<PrRef, u64>>,
    /// Denials already in the chat, by review, turn and tool: the program reports the same
    /// denial again in its result at the end of the turn, which must not be told twice.
    told: Mutex<HashMap<(PrRef, u64, String), usize>>,
}

impl Permissions {
    fn take(&self, id: &str) -> Option<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id)
    }

    fn ids_where(&self, keep: impl Fn(&PermissionRequest) -> bool) -> Vec<String> {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, pending)| keep(&pending.request))
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn note_denial(&self, pr: &PrRef, turn: u64, tool: &str) {
        *self
            .told
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry((pr.clone(), turn, tool.to_string()))
            .or_default() += 1;
    }

    /// Whether the program's report of a denied `tool` repeats one that was already told;
    /// each report uses up one.
    pub(crate) fn already_told(&self, pr: &PrRef, turn: u64, tool: &str) -> bool {
        let mut told = self.told.lock().unwrap_or_else(|p| p.into_inner());
        let key = (pr.clone(), turn, tool.to_string());
        match told.get_mut(&key) {
            Some(count) if *count > 1 => {
                *count -= 1;
                true
            }
            Some(_) => {
                told.remove(&key);
                true
            }
            None => false,
        }
    }

    fn close_turn(&self, pr: &PrRef, turn: u64) {
        let mut closed = self.closed.lock().unwrap_or_else(|p| p.into_inner());
        let last = closed.entry(pr.clone()).or_insert(turn);
        *last = (*last).max(turn);
    }

    fn is_closed(&self, pr: &PrRef, turn: u64) -> bool {
        self.closed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(pr)
            .is_some_and(|last| turn <= *last)
    }

    /// The review ended: its last turn is no longer kept. Only once no turn of it runs, since
    /// a stopped program may still ask until it is gone.
    pub(crate) fn forget_review(&self, shared: &Shared, pr: &PrRef) {
        if shared.sessions.running_turn(pr).is_none() {
            self.closed
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(pr);
        }
    }

    fn forget_turn(&self, pr: &PrRef, turn: u64) {
        self.told
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(p, t, _), _| !(p == pr && *t == turn));
    }

    #[cfg(test)]
    pub(crate) fn waiting(&self) -> usize {
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The path a file tool wants to change.
fn file_path(input: &Value) -> Option<PathBuf> {
    ["file_path", "notebook_path"]
        .into_iter()
        .find_map(|key| input.get(key)?.as_str())
        .map(PathBuf::from)
}

/// What the tool does, as the notification says it.
fn verb(tool: &str) -> &'static str {
    match tool {
        "Bash" => "run",
        "Edit" | "MultiEdit" | "NotebookEdit" => "edit",
        "Write" => "write",
        _ => "use",
    }
}

fn log(
    shared: &Shared,
    pr: &PrRef,
    turn: u64,
    tool: &str,
    summary: &str,
    outcome: PermissionOutcome,
) {
    let entry = AgentLogEntry::Permission {
        at: now_unix(),
        turn,
        tool: tool.to_string(),
        summary: summary.to_string(),
        outcome,
    };
    if let Err(e) = agent_log::append(&shared.paths.agent_log(pr), &entry) {
        tracing::warn!(error = %e, pr = %pr, "cannot append to the agent log");
    }
}

/// Whether `turn` of `pr` may still ask: it is the running turn, it has not ended, and the
/// daemon is not stopping.
fn may_ask(shared: &Shared, pr: &PrRef, turn: u64) -> Result<(), String> {
    if *shared.shutdown.borrow() {
        return Err("clusiad is stopping".to_string());
    }
    if shared.sessions.running_turn(pr) != Some(turn) || shared.permissions.is_closed(pr, turn) {
        return Err(format!("turn {turn} of {pr} is not running"));
    }
    Ok(())
}

/// A decision that needs no reviewer: logged and told like any other, under an id of its own.
fn decided_at_once(
    shared: &Shared,
    pr: &PrRef,
    turn: u64,
    tool: &str,
    summary: &str,
    outcome: PermissionOutcome,
) {
    log(shared, pr, turn, tool, summary, outcome);
    if outcome == PermissionOutcome::Denied {
        shared.permissions.note_denial(pr, turn, tool);
    }
    shared.publish(
        topics::AGENT,
        Event::PermissionResolved {
            id: new_id(),
            pr: pr.clone(),
            tool: tool.to_string(),
            summary: summary.to_string(),
            outcome,
        },
    );
}

fn new_id() -> String {
    format!("perm-{}", uuid::Uuid::new_v4().simple())
}

/// Puts `request` among the pending ones and announces it, unless its turn can no longer ask.
/// The check, the insert and the announcement happen under the lock that `settle` and
/// `cancel_turn` need to end a request, so a turn that ends meanwhile either finds the request
/// and cancels it, or is seen here and the request is never announced.
fn register(
    shared: &Shared,
    request: PermissionRequest,
    rule: Option<String>,
) -> Result<oneshot::Receiver<PermissionOutcome>, String> {
    let (done, outcome) = oneshot::channel();
    let permissions = &shared.permissions;
    let mut pending = permissions
        .pending
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    may_ask(shared, &request.pr, request.turn)?;
    pending.insert(
        request.id.clone(),
        Pending {
            request: request.clone(),
            seq: permissions.next_seq.fetch_add(1, Ordering::SeqCst),
            rule,
            done,
        },
    );
    let PermissionRequest {
        id,
        pr,
        turn,
        tool,
        summary,
        reason,
        prefix,
        sandbox,
        deadline,
        detail,
    } = request;
    shared.publish(
        topics::AGENT,
        Event::PermissionRequested {
            id,
            pr,
            turn,
            tool,
            summary,
            reason,
            prefix,
            sandbox,
            deadline,
            detail,
        },
    );
    Ok(outcome)
}

/// Decides one request. `Err` is a refusal (the turn is not running, the daemon is stopping);
/// the bridge reads it as a denial.
pub(crate) async fn ask(
    shared: &Arc<Shared>,
    request: PermissionAsk,
) -> Result<PermissionDecision, String> {
    let PermissionAsk {
        pr,
        turn,
        tool,
        input,
    } = request;
    may_ask(shared, &pr, turn)?;
    let worktree = shared.paths.worktree_for(&pr);
    let summary = summary_for(&tool, &input);
    let (timeout, configured_sandbox) = {
        let config = shared.config.read().await;
        (
            u64::from(config.harness.permission_timeout_secs),
            config.harness.sandbox,
        )
    };
    let sandbox = shared
        .sessions
        .turn_sandbox(&pr, turn)
        .unwrap_or(configured_sandbox);

    // A command that asks to run outside the sandbox would step over the boundary the
    // reviewer chose: no rule covers it and nobody is asked.
    if sandbox
        && tool == "Bash"
        && !matches!(
            input.get("dangerouslyDisableSandbox"),
            None | Some(Value::Null | Value::Bool(false))
        )
    {
        decided_at_once(
            shared,
            &pr,
            turn,
            &tool,
            &summary,
            PermissionOutcome::Denied,
        );
        return Ok(PermissionDecision::denied(UNSANDBOXED));
    }

    if FILE_TOOLS.contains(&tool.as_str())
        && !file_path(&input).is_some_and(|path| inside_worktree(&path, &worktree))
    {
        decided_at_once(
            shared,
            &pr,
            turn,
            &tool,
            &summary,
            PermissionOutcome::Denied,
        );
        return Ok(PermissionDecision::denied(OUTSIDE));
    }

    let state = load_agent_state(&shared.paths, &pr).unwrap_or_default();
    if covers(&state.rules, &tool, &input, &worktree) {
        decided_at_once(
            shared,
            &pr,
            turn,
            &tool,
            &summary,
            PermissionOutcome::AllowedForReview,
        );
        return Ok(PermissionDecision::allowed());
    }

    let prefix = prefix_for(&tool, &input, &worktree);
    let rule = prefix.as_deref().map(|prefix| match tool.as_str() {
        "Bash" => rule_for_bash(prefix),
        _ => rule_for_tool(&tool),
    });
    let id = new_id();
    // One instant for the deadline the windows count down to and the one that expires the
    // request, taken before anything that may take a while (the tray).
    let expires = tokio::time::Instant::now() + Duration::from_secs(timeout);
    let request = PermissionRequest {
        id: id.clone(),
        pr: pr.clone(),
        turn,
        tool: tool.clone(),
        summary: summary.clone(),
        reason: (tool == "Bash")
            .then(|| input.get("description").and_then(Value::as_str))
            .flatten()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(str::to_string),
        prefix,
        sandbox,
        deadline: now_ms() + (timeout * 1000) as i64,
        detail: detail_for(&tool, &input),
    };
    let outcome = register(shared, request, rule)?;
    // A bridge that goes away does not end this: the connection detaches its handler, so the
    // request waits for an answer, the deadline or the end of the turn. Only a dropped task
    // (the runtime going down) ends it early, and then it must not stay listed.
    let _abandoned = Abandoned { shared, id: &id };
    if !shared.holds.is_held(&pr) {
        notify(shared, &pr, &id, &tool, &summary).await;
    }
    tokio::pin!(outcome);
    let outcome = tokio::select! {
        answered = &mut outcome => answered,
        _ = tokio::time::sleep_until(expires) => {
            // An answer that landed first has already taken the request out: then it is the
            // one that decides, and the channel holds it.
            settle(shared, &id, PermissionOutcome::Expired).await;
            outcome.await
        }
    };
    Ok(PermissionDecision::of(
        outcome.unwrap_or(PermissionOutcome::Cancelled),
    ))
}

/// Cancels the request when `ask` is dropped before it was decided; once decided, the request
/// is no longer pending and this does nothing.
struct Abandoned<'a> {
    shared: &'a Shared,
    id: &'a str,
}

impl Drop for Abandoned<'_> {
    fn drop(&mut self) {
        if let Some(pending) = self.shared.permissions.take(self.id) {
            finish(self.shared, self.id, pending, PermissionOutcome::Cancelled);
        }
    }
}

/// Tells the tray, because no window holds the review to show the question.
async fn notify(shared: &Shared, pr: &PrRef, id: &str, tool: &str, summary: &str) {
    let title = match reviews::load_stored(shared, pr) {
        Ok(Some(review)) => review.title,
        _ => String::new(),
    };
    let event = NotifyEvent::agent_permission(
        pr,
        &title,
        verb(tool),
        summary,
        format!("agent_permission:{id}"),
        now_unix(),
    );
    notifications::deliver(shared, vec![event]).await;
}

/// Ends the request `id` with `outcome`: the first call wins, a later one finds nothing.
/// Saves the rule when the reviewer allowed the prefix, and only then lets the agent go on,
/// so the next request of the turn is already covered.
async fn settle(shared: &Shared, id: &str, outcome: PermissionOutcome) -> bool {
    let Some(pending) = shared.permissions.take(id) else {
        return false;
    };
    let outcome = match (&pending.rule, outcome) {
        (Some(rule), PermissionOutcome::AllowedForReview) => {
            if add_rule(shared, &pending.request.pr, rule).await {
                outcome
            } else {
                PermissionOutcome::Allowed
            }
        }
        // Allow for this review without a prefix to grant is Allow once.
        (None, PermissionOutcome::AllowedForReview) => PermissionOutcome::Allowed,
        _ => outcome,
    };
    finish(shared, id, pending, outcome);
    true
}

/// Logs and announces how the request `id`, already taken out, ended, and tells the agent.
fn finish(shared: &Shared, id: &str, pending: Pending, outcome: PermissionOutcome) {
    let request = &pending.request;
    log(
        shared,
        &request.pr,
        request.turn,
        &request.tool,
        &request.summary,
        outcome,
    );
    if !matches!(
        outcome,
        PermissionOutcome::Allowed | PermissionOutcome::AllowedForReview
    ) {
        shared
            .permissions
            .note_denial(&request.pr, request.turn, &request.tool);
    }
    shared.publish(
        topics::AGENT,
        Event::PermissionResolved {
            id: id.to_string(),
            pr: request.pr.clone(),
            tool: request.tool.clone(),
            summary: request.summary.clone(),
            outcome,
        },
    );
    let _ = pending.done.send(outcome);
}

/// The reviewer's answer. The first one wins; a later one (another window, the terminal, the
/// deadline) is refused with `NotFound`.
pub(crate) async fn answer(shared: &Shared, id: &str, kind: PermissionAnswerKind) -> Outcome {
    let outcome = match kind {
        PermissionAnswerKind::Once => PermissionOutcome::Allowed,
        PermissionAnswerKind::Review => PermissionOutcome::AllowedForReview,
        PermissionAnswerKind::Deny => PermissionOutcome::Denied,
    };
    if settle(shared, id, outcome).await {
        Outcome::Ok(Reply::Ack)
    } else {
        Outcome::Err(ProtocolError::new(
            ErrorCode::NotFound,
            "that request was already answered or has expired",
        ))
    }
}

/// The requests of `pr` that wait for an answer, oldest first: what a window that was not
/// listening when they were announced (just started, or reconnected) shows.
pub(crate) fn waiting_for(shared: &Shared, pr: &PrRef) -> Outcome {
    let pending = shared
        .permissions
        .pending
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let mut mine: Vec<(u64, PermissionRequest)> = pending
        .values()
        .filter(|pending| pending.request.pr == *pr)
        .map(|pending| (pending.seq, pending.request.clone()))
        .collect();
    mine.sort_by_key(|(seq, _)| *seq);
    Outcome::Ok(Reply::Permissions(
        mine.into_iter().map(|(_, request)| request).collect(),
    ))
}

/// The turn ended (or was stopped, or timed out): whatever it still waits for is denied, and
/// it asks no more.
pub(crate) async fn cancel_turn(shared: &Shared, pr: &PrRef, turn: u64) {
    shared.permissions.close_turn(pr, turn);
    for id in shared
        .permissions
        .ids_where(|request| request.pr == *pr && request.turn == turn)
    {
        settle(shared, &id, PermissionOutcome::Cancelled).await;
    }
    shared.permissions.forget_turn(pr, turn);
}

/// The daemon is stopping: nothing may keep waiting for an answer that cannot come.
pub(crate) async fn cancel_all(shared: &Shared) {
    for id in shared.permissions.ids_where(|_| true) {
        settle(shared, &id, PermissionOutcome::Cancelled).await;
    }
}

fn sorted(rules: &std::collections::BTreeSet<String>) -> Vec<String> {
    rules.iter().cloned().collect()
}

/// Saves `rule` for the review, unless the review is gone: an answer that comes while the
/// review ends must not bring back a rule its end just dropped. The check runs under the review
/// lock, which the review's end holds while it drops the rules.
async fn add_rule(shared: &Shared, pr: &PrRef, rule: &str) -> bool {
    let mut rules = None;
    agent::update_state(shared, pr, |state| {
        if matches!(reviews::load_stored(shared, pr), Ok(Some(_))) {
            state.rules.insert(rule.to_string());
            rules = Some(sorted(&state.rules));
        }
    })
    .await;
    let Some(rules) = rules else {
        return false;
    };
    shared.publish(
        topics::AGENT,
        Event::RulesChanged {
            pr: pr.clone(),
            rules,
        },
    );
    true
}

/// Stops allowing what `rule` allowed. Revoking a rule the review does not have is fine.
pub(crate) async fn revoke(shared: &Shared, pr: &PrRef, rule: &str) -> Outcome {
    let mut rules = Vec::new();
    agent::update_state(shared, pr, |state| {
        state.rules.remove(rule);
        rules = sorted(&state.rules);
    })
    .await;
    shared.publish(
        topics::AGENT,
        Event::RulesChanged {
            pr: pr.clone(),
            rules,
        },
    );
    Outcome::Ok(Reply::Ack)
}

/// The rules the reviewer allowed for this review, in name order.
pub(crate) fn rules(shared: &Shared, pr: &PrRef) -> Outcome {
    let state = load_agent_state(&shared.paths, pr).unwrap_or_default();
    Outcome::Ok(Reply::Rules(sorted(&state.rules)))
}

/// `ask` as a protocol outcome.
pub(crate) async fn handle_ask(shared: &Arc<Shared>, request: PermissionAsk) -> Outcome {
    match ask(shared, request).await {
        Ok(decision) => Outcome::Ok(Reply::PermissionDecision {
            allow: decision.allow,
            message: decision.message,
        }),
        Err(message) => Outcome::Err(ProtocolError::new(ErrorCode::InvalidState, message)),
    }
}

#[cfg(test)]
mod tests {
    use clusia_core::{Config, Paths, Review};
    use clusia_protocol::ErrorCode;
    use clusia_store::save_review;
    use serde_json::json;
    use tokio::sync::broadcast;
    use tokio::task::JoinHandle;

    use super::*;

    type Events = broadcast::Receiver<(String, Event)>;
    type Asked = JoinHandle<Result<PermissionDecision, String>>;

    struct Lab {
        _home: tempfile::TempDir,
        shared: Arc<Shared>,
        events: Events,
    }

    fn pr(n: u64) -> PrRef {
        format!("acme/widgets#{n}").parse().unwrap()
    }

    /// A daemon state with the review acme/widgets#7, whose turn 1 is running (no process).
    fn lab() -> Lab {
        let home = tempfile::tempdir().unwrap();
        let options = crate::options::DaemonOptions {
            claude_program: Some("/nonexistent/claude".into()),
            bridge_program: None,
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
        let shared = Arc::new(Shared::new(
            Paths::new(home.path()),
            Config::default(),
            options,
        ));
        let review = Review::new(pr(7), "Add feature".into(), "b".into(), "h".into(), 1);
        save_review(&shared.paths, &review).unwrap();
        std::fs::create_dir_all(shared.paths.worktree_for(&pr(7))).unwrap();
        shared.sessions.pretend_running(&pr(7), 1);
        let events = shared.events.subscribe();
        Lab {
            _home: home,
            shared,
            events,
        }
    }

    impl Lab {
        fn ask(&self, tool: &str, input: Value) -> Asked {
            self.ask_turn(1, tool, input)
        }

        fn ask_turn(&self, turn: u64, tool: &str, input: Value) -> Asked {
            let shared = self.shared.clone();
            let request = PermissionAsk {
                pr: pr(7),
                turn,
                tool: tool.to_string(),
                input,
            };
            tokio::spawn(async move { ask(&shared, request).await })
        }

        /// The next permission request the daemon announces: (id, event).
        async fn requested(&mut self) -> (String, Event) {
            loop {
                let (_, event) = self.events.recv().await.expect("the channel stays open");
                if let Event::PermissionRequested { id, .. } = &event {
                    return (id.clone(), event);
                }
            }
        }

        /// Everything announced up to and including the resolution of `id`.
        async fn until_resolved(&mut self, id: &str) -> Vec<(String, Event)> {
            let mut seen = Vec::new();
            loop {
                let item = self.events.recv().await.expect("the channel stays open");
                let done =
                    matches!(&item.1, Event::PermissionResolved { id: got, .. } if got == id);
                seen.push(item);
                if done {
                    return seen;
                }
            }
        }

        fn log(&self) -> Vec<AgentLogEntry> {
            agent_log::read(&self.shared.paths.agent_log(&pr(7)))
        }

        fn rules(&self) -> Vec<String> {
            load_agent_state(&self.shared.paths, &pr(7))
                .unwrap()
                .rules
                .into_iter()
                .collect()
        }

        fn worktree(&self) -> PathBuf {
            self.shared.paths.worktree_for(&pr(7))
        }
    }

    fn bash(command: &str) -> Value {
        json!({"command": command, "description": "run the tests"})
    }

    fn outcomes(log: &[AgentLogEntry]) -> Vec<PermissionOutcome> {
        log.iter()
            .filter_map(|entry| match entry {
                AgentLogEntry::Permission { outcome, .. } => Some(*outcome),
                _ => None,
            })
            .collect()
    }

    fn refused_as_not_found(outcome: Outcome) -> bool {
        matches!(outcome, Outcome::Err(e) if e.code == ErrorCode::NotFound)
    }

    #[tokio::test]
    async fn a_request_waits_for_the_reviewer_and_once_allows_only_it() {
        let mut lab = lab();
        let asked = lab.ask("Bash", bash("cargo test -p clusia-core"));
        let (id, requested) = lab.requested().await;
        let Event::PermissionRequested {
            pr: got_pr,
            turn,
            tool,
            summary,
            reason,
            prefix,
            sandbox,
            deadline,
            detail,
            ..
        } = requested
        else {
            unreachable!()
        };
        assert_eq!((got_pr, turn, tool.as_str()), (pr(7), 1, "Bash"));
        assert_eq!(summary, "cargo test -p clusia-core");
        assert_eq!(reason.as_deref(), Some("run the tests"));
        assert_eq!(prefix.as_deref(), Some("cargo test"));
        assert!(sandbox, "the sandbox is on by default");
        assert_eq!(detail, None, "a command is its own excerpt");
        let Outcome::Ok(Reply::Permissions(listed)) = waiting_for(&lab.shared, &pr(7)) else {
            panic!("a list of requests");
        };
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].id.as_str(), listed[0].turn), (id.as_str(), 1));
        assert_eq!(listed[0].summary, "cargo test -p clusia-core");
        let in_ms = deadline - now_ms();
        assert!(
            (110_000..=121_000).contains(&in_ms),
            "the default is 2 minutes: {in_ms}"
        );
        assert!(matches!(
            answer(&lab.shared, &id, PermissionAnswerKind::Once).await,
            Outcome::Ok(Reply::Ack)
        ));
        assert_eq!(asked.await.unwrap().unwrap(), PermissionDecision::allowed());
        assert_eq!(outcomes(&lab.log()), [PermissionOutcome::Allowed]);
        assert!(lab.rules().is_empty(), "once saves no rule");
        assert_eq!(lab.shared.permissions.waiting(), 0);
        // The same command asks again: once is once.
        let again = lab.ask("Bash", bash("cargo test -p clusia-core"));
        let (second, _) = lab.requested().await;
        assert_ne!(second, id);
        answer(&lab.shared, &second, PermissionAnswerKind::Deny).await;
        assert!(!again.await.unwrap().unwrap().allow);
    }

    #[tokio::test]
    async fn allowing_for_the_review_saves_the_prefix_and_covers_what_follows() {
        let mut lab = lab();
        let asked = lab.ask("Bash", bash("cargo test -p clusia-core"));
        let (id, _) = lab.requested().await;
        answer(&lab.shared, &id, PermissionAnswerKind::Review).await;
        assert!(asked.await.unwrap().unwrap().allow);
        assert_eq!(lab.rules(), ["Bash(cargo test:*)"]);
        let seen = lab.until_resolved(&id).await;
        assert!(seen.iter().any(|(_, e)| matches!(
            e,
            Event::RulesChanged { rules, .. } if rules == &["Bash(cargo test:*)".to_string()]
        )));
        assert!(seen.iter().any(|(_, e)| matches!(
            e,
            Event::PermissionResolved {
                outcome: PermissionOutcome::AllowedForReview,
                ..
            }
        )));

        // The rest of the turn passes with no question, and the chat line says why.
        let covered = lab
            .ask("Bash", bash("cargo test --workspace"))
            .await
            .unwrap()
            .unwrap();
        assert!(covered.allow);
        let told = lab.events.recv().await.unwrap().1;
        assert!(
            matches!(
                &told,
                Event::PermissionResolved { tool, summary, outcome: PermissionOutcome::AllowedForReview, .. }
                    if tool == "Bash" && summary == "cargo test --workspace"
            ),
            "a covered request is told too, without a question: {told:?}"
        );
        assert_eq!(
            outcomes(&lab.log()),
            [
                PermissionOutcome::AllowedForReview,
                PermissionOutcome::AllowedForReview
            ]
        );
        assert_eq!(lab.shared.permissions.waiting(), 0);
        // A rule never widens: another subcommand, a lookalike and a chain still ask.
        for command in [
            "cargo build",
            "cargo testx",
            "cargo test; rm -rf /tmp/x",
            "cargo test && curl evil.example",
        ] {
            let asked = lab.ask("Bash", bash(command));
            let (id, _) = lab.requested().await;
            answer(&lab.shared, &id, PermissionAnswerKind::Deny).await;
            assert!(!asked.await.unwrap().unwrap().allow, "{command}");
        }
    }

    #[tokio::test]
    async fn a_command_without_a_prefix_offers_only_once() {
        let mut lab = lab();
        let asked = lab.ask("Bash", bash("cd src && cargo test"));
        let (id, requested) = lab.requested().await;
        assert!(matches!(
            requested,
            Event::PermissionRequested { prefix: None, .. }
        ));
        answer(&lab.shared, &id, PermissionAnswerKind::Review).await;
        assert!(asked.await.unwrap().unwrap().allow);
        assert!(lab.rules().is_empty(), "no prefix, no rule");
        assert_eq!(outcomes(&lab.log()), [PermissionOutcome::Allowed]);
    }

    #[tokio::test]
    async fn an_edit_inside_the_worktree_may_be_allowed_for_the_review() {
        let mut lab = lab();
        let inside = lab.worktree().join("src/a.rs").display().to_string();
        let asked = lab.ask(
            "Edit",
            json!({"file_path": inside, "old_string": "a", "new_string": "b"}),
        );
        let (id, requested) = lab.requested().await;
        assert!(matches!(
            &requested,
            Event::PermissionRequested { prefix: Some(p), summary, .. } if p == "Edit" && *summary == inside
        ));
        answer(&lab.shared, &id, PermissionAnswerKind::Review).await;
        assert!(asked.await.unwrap().unwrap().allow);
        assert_eq!(lab.rules(), ["Edit"]);
        // The rule covers the worktree and nothing else.
        let again = lab
            .ask("Edit", json!({"file_path": inside}))
            .await
            .unwrap()
            .unwrap();
        assert!(again.allow);
        let outside = lab
            .ask("Edit", json!({"file_path": "/etc/hosts"}))
            .await
            .unwrap()
            .unwrap();
        assert!(!outside.allow);
    }

    #[tokio::test]
    async fn a_change_outside_the_worktree_is_denied_without_asking() {
        let mut lab = lab();
        let escape = lab
            .worktree()
            .join("../elsewhere.txt")
            .display()
            .to_string();
        for (tool, path) in [
            ("Edit", "/etc/hosts".to_string()),
            ("Write", escape),
            ("MultiEdit", "../../outside.rs".to_string()),
        ] {
            let decision = lab
                .ask(tool, json!({"file_path": path}))
                .await
                .unwrap()
                .unwrap();
            assert!(!decision.allow, "{tool} {path}");
            assert_eq!(decision.message.as_deref(), Some(OUTSIDE));
        }
        let missing = lab
            .ask("Write", json!({"content": "x"}))
            .await
            .unwrap()
            .unwrap();
        assert!(
            !missing.allow,
            "a write that names no path is not allowed either"
        );
        assert_eq!(lab.shared.permissions.waiting(), 0);
        assert_eq!(outcomes(&lab.log()), [PermissionOutcome::Denied; 4]);
        let mut told = Vec::new();
        while let Ok((_, event)) = lab.events.try_recv() {
            match event {
                Event::PermissionRequested { .. } => panic!("no prompt"),
                Event::PermissionResolved {
                    tool,
                    summary,
                    outcome,
                    ..
                } => told.push((tool, summary, outcome)),
                _ => {}
            }
        }
        assert_eq!(told.len(), 4, "each denial is told: {told:?}");
        assert_eq!(
            told[0],
            (
                "Edit".into(),
                "/etc/hosts".into(),
                PermissionOutcome::Denied
            )
        );
        // Told once: the program's own report of those denials is not repeated.
        assert!(lab.shared.permissions.already_told(&pr(7), 1, "Edit"));
        assert!(!lab.shared.permissions.already_told(&pr(7), 1, "Edit"));
    }

    #[tokio::test(start_paused = true)]
    async fn silence_and_errors_deny() {
        let mut lab = lab();
        // A turn that is not the running one, and a review with no turn, are refused.
        let refused = lab.ask_turn(9, "Bash", bash("ls")).await.unwrap();
        assert!(refused.unwrap_err().contains("not running"));
        let shared = lab.shared.clone();
        let other = ask(
            &shared,
            PermissionAsk {
                pr: pr(8),
                turn: 1,
                tool: "Bash".into(),
                input: bash("ls"),
            },
        )
        .await;
        assert!(other.is_err());

        // Nobody answers: the deadline denies, and says so.
        let asked = lab.ask("Bash", bash("make"));
        let (id, _) = lab.requested().await;
        let decision = asked.await.unwrap().unwrap();
        assert_eq!(decision, PermissionDecision::denied(EXPIRED));
        let seen = lab.until_resolved(&id).await;
        assert!(matches!(
            &seen.last().unwrap().1,
            Event::PermissionResolved {
                outcome: PermissionOutcome::Expired,
                ..
            }
        ));
        assert_eq!(outcomes(&lab.log()), [PermissionOutcome::Expired]);
        assert_eq!(lab.shared.permissions.waiting(), 0);
        // An answer after the deadline is too late, and allows nothing.
        let late = answer(&lab.shared, &id, PermissionAnswerKind::Once).await;
        assert!(refused_as_not_found(late));

        // A daemon on its way out takes no new question.
        lab.shared.trigger_shutdown();
        let stopping = lab.ask("Bash", bash("ls")).await.unwrap();
        assert!(stopping.unwrap_err().contains("stopping"));
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_follows_the_setting() {
        let mut lab = lab();
        lab.shared
            .config
            .write()
            .await
            .harness
            .permission_timeout_secs = 30;
        let started = tokio::time::Instant::now();
        let asked = lab.ask("Bash", bash("make"));
        let (_, requested) = lab.requested().await;
        assert!(matches!(
            requested,
            Event::PermissionRequested { deadline, .. } if deadline - now_ms() <= 30_000
        ));
        assert!(!asked.await.unwrap().unwrap().allow);
        assert_eq!(started.elapsed(), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn the_first_answer_wins() {
        let mut lab = lab();
        let asked = lab.ask("Bash", bash("make"));
        let (id, _) = lab.requested().await;
        let (first, second) = tokio::join!(
            answer(&lab.shared, &id, PermissionAnswerKind::Deny),
            answer(&lab.shared, &id, PermissionAnswerKind::Once),
        );
        // Exactly one of them decided; the other found nothing.
        let (won, lost) = match first {
            Outcome::Ok(_) => (PermissionOutcome::Denied, second),
            lost => (PermissionOutcome::Allowed, lost),
        };
        assert!(refused_as_not_found(lost));
        assert_eq!(
            asked.await.unwrap().unwrap().allow,
            won == PermissionOutcome::Allowed
        );
        assert_eq!(outcomes(&lab.log()), [won]);
    }

    #[tokio::test]
    async fn turn_end_clears_pending() {
        let mut lab = lab();
        let one = lab.ask("Bash", bash("make"));
        let two = lab.ask("Bash", bash("make test"));
        let (a, _) = lab.requested().await;
        let (b, _) = lab.requested().await;
        assert_eq!(lab.shared.permissions.waiting(), 2);
        // Another turn's end leaves them alone.
        cancel_turn(&lab.shared, &pr(7), 2).await;
        assert_eq!(lab.shared.permissions.waiting(), 2);
        cancel_turn(&lab.shared, &pr(7), 1).await;
        assert_eq!(lab.shared.permissions.waiting(), 0);
        assert_eq!(
            one.await.unwrap().unwrap(),
            PermissionDecision::denied(CANCELLED)
        );
        assert_eq!(
            two.await.unwrap().unwrap(),
            PermissionDecision::denied(CANCELLED)
        );
        assert_eq!(outcomes(&lab.log()), [PermissionOutcome::Cancelled; 2]);
        assert!(refused_as_not_found(
            answer(&lab.shared, &a, PermissionAnswerKind::Once).await
        ));
        assert!(refused_as_not_found(
            answer(&lab.shared, &b, PermissionAnswerKind::Once).await
        ));
    }

    #[tokio::test]
    async fn a_stopping_daemon_denies_whatever_waits() {
        let mut lab = lab();
        let asked = lab.ask("Bash", bash("make"));
        lab.requested().await;
        cancel_all(&lab.shared).await;
        assert!(!asked.await.unwrap().unwrap().allow);
        assert_eq!(lab.shared.permissions.waiting(), 0);
    }

    #[tokio::test]
    async fn an_answer_for_the_review_after_it_ended_saves_no_rule() {
        let mut lab = lab();
        let asked = lab.ask("Bash", bash("cargo test"));
        let (id, _) = lab.requested().await;
        // The review closes empty under its lock while the answer comes in.
        let guard = reviews::lock(&lab.shared, &pr(7)).await;
        let answering = {
            let shared = lab.shared.clone();
            let id = id.clone();
            tokio::spawn(async move { answer(&shared, &id, PermissionAnswerKind::Review).await })
        };
        while lab.shared.permissions.waiting() > 0 {
            tokio::task::yield_now().await;
        }
        clusia_store::delete_review(&lab.shared.paths, &pr(7)).unwrap();
        agent::forget(&lab.shared, &pr(7));
        drop(guard);
        assert_eq!(answering.await.unwrap(), Outcome::Ok(Reply::Ack));
        assert!(
            asked.await.unwrap().unwrap().allow,
            "the answer still allows"
        );
        assert!(lab.rules().is_empty(), "no rule outlives the review");
        assert_eq!(outcomes(&lab.log()), [PermissionOutcome::Allowed]);
        let seen = lab.until_resolved(&id).await;
        assert!(
            !seen
                .iter()
                .any(|(_, e)| matches!(e, Event::RulesChanged { rules, .. } if !rules.is_empty())),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn a_request_whose_asker_went_away_is_cancelled() {
        let mut lab = lab();
        let asked = lab.ask("Bash", bash("make"));
        let (id, _) = lab.requested().await;
        asked.abort();
        assert!(asked.await.unwrap_err().is_cancelled());
        assert_eq!(lab.shared.permissions.waiting(), 0);
        let seen = lab.until_resolved(&id).await;
        assert!(matches!(
            &seen.last().unwrap().1,
            Event::PermissionResolved {
                outcome: PermissionOutcome::Cancelled,
                ..
            }
        ));
        assert_eq!(outcomes(&lab.log()), [PermissionOutcome::Cancelled]);
        assert!(refused_as_not_found(
            answer(&lab.shared, &id, PermissionAnswerKind::Once).await
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn the_request_expires_when_the_announced_deadline_passes() {
        let mut lab = lab();
        lab.shared
            .config
            .write()
            .await
            .harness
            .permission_timeout_secs = 30;
        let started = tokio::time::Instant::now();
        // The tray is slow to take the notification.
        let shared = lab.shared.clone();
        let inbox = shared.engine.lock().await;
        let asked = lab.ask("Bash", bash("make"));
        lab.requested().await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop(inbox);
        assert_eq!(
            asked.await.unwrap().unwrap(),
            PermissionDecision::denied(EXPIRED)
        );
        assert_eq!(started.elapsed(), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn only_a_command_gives_a_reason() {
        let mut lab = lab();
        let path = lab.worktree().join("src/a.rs").display().to_string();
        let asked = lab.ask("Edit", json!({"file_path": path, "description": "tidy up"}));
        let (id, requested) = lab.requested().await;
        assert!(matches!(
            requested,
            Event::PermissionRequested { reason: None, .. }
        ));
        answer(&lab.shared, &id, PermissionAnswerKind::Deny).await;
        asked.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn rules_can_be_listed_and_revoked_and_die_with_the_review() {
        let mut lab = lab();
        for command in ["cargo test", "git status"] {
            let asked = lab.ask("Bash", bash(command));
            let (id, _) = lab.requested().await;
            answer(&lab.shared, &id, PermissionAnswerKind::Review).await;
            asked.await.unwrap().unwrap();
        }
        assert_eq!(
            rules(&lab.shared, &pr(7)),
            Outcome::Ok(Reply::Rules(vec![
                "Bash(cargo test:*)".into(),
                "Bash(git status:*)".into()
            ]))
        );
        while lab.events.try_recv().is_ok() {}
        assert_eq!(
            revoke(&lab.shared, &pr(7), "Bash(cargo test:*)").await,
            Outcome::Ok(Reply::Ack)
        );
        assert!(matches!(
            lab.events.recv().await.unwrap().1,
            Event::RulesChanged { rules, .. } if rules == ["Bash(git status:*)".to_string()]
        ));
        // What the revoked rule covered asks again.
        let asked = lab.ask("Bash", bash("cargo test"));
        let (id, _) = lab.requested().await;
        answer(&lab.shared, &id, PermissionAnswerKind::Deny).await;
        assert!(!asked.await.unwrap().unwrap().allow);
        // Revoking a rule that is not there is harmless.
        assert_eq!(
            revoke(&lab.shared, &pr(7), "Bash(make:*)").await,
            Outcome::Ok(Reply::Ack)
        );
        // The review ends: its rules go with it.
        agent::forget(&lab.shared, &pr(7));
        assert!(lab.rules().is_empty());
    }

    #[tokio::test]
    async fn the_tray_hears_of_a_request_only_when_no_window_holds_the_review() {
        let mut lab = lab();
        let asked = lab.ask("Bash", bash("make"));
        let (id, _) = lab.requested().await;
        answer(&lab.shared, &id, PermissionAnswerKind::Once).await;
        asked.await.unwrap().unwrap();
        let seen = lab.until_resolved(&id).await;
        let banner = seen.iter().find_map(|(topic, event)| match event {
            Event::Notify {
                title,
                body,
                sound,
                time_sensitive,
                open,
                ..
            } if topic == topics::TRAY => Some((title, body, sound, *time_sensitive, open)),
            _ => None,
        });
        let (title, body, sound, time_sensitive, open) = banner.expect("a banner for the tray");
        assert_eq!(title, "Claude Code needs your permission on #7");
        assert_eq!(body, "It wants to run: make · Add feature");
        assert!(sound.is_some() && time_sensitive);
        assert_eq!(
            open,
            &clusia_core::notify::OpenTarget::Review {
                pr: pr(7),
                thread: None
            }
        );

        // A window that holds the review shows the question itself.
        lab.shared.holds.set(&pr(7), 1, true);
        let held = lab.ask("Bash", bash("make"));
        let (id, _) = lab.requested().await;
        answer(&lab.shared, &id, PermissionAnswerKind::Deny).await;
        held.await.unwrap().unwrap();
        let seen = lab.until_resolved(&id).await;
        assert!(!seen.iter().any(|(_, e)| matches!(e, Event::Notify { .. })));
    }

    #[tokio::test]
    async fn the_request_carries_an_excerpt_of_the_input() {
        let mut lab = lab();
        let path = lab.worktree().join("src/a.rs").display().to_string();
        let asked = lab.ask(
            "Edit",
            json!({"file_path": path, "old_string": "let a = 1;", "new_string": "let a = 2;"}),
        );
        let (id, requested) = lab.requested().await;
        assert!(matches!(
            requested,
            Event::PermissionRequested { detail: Some(d), .. } if d == "let a = 1;\n→\nlet a = 2;"
        ));
        answer(&lab.shared, &id, PermissionAnswerKind::Deny).await;
        asked.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_window_that_was_not_listening_asks_what_waits() {
        let mut lab = lab();
        let one = lab.ask("Bash", bash("make"));
        let (first, _) = lab.requested().await;
        let two = lab.ask("Bash", bash("make test"));
        let (second, _) = lab.requested().await;
        // A window that connects now never saw the announcements.
        let Outcome::Ok(Reply::Permissions(listed)) = waiting_for(&lab.shared, &pr(7)) else {
            panic!("a list of requests");
        };
        let ids: Vec<&str> = listed.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, [first.as_str(), second.as_str()], "oldest first");
        assert_eq!(listed[1].summary, "make test");
        // `make` is a rule-able program on its own, whatever target it is given.
        assert_eq!(listed[1].prefix.as_deref(), Some("make"));
        assert!(listed[1].deadline > now_ms());
        // Another review has none, and an answered request leaves the list.
        assert_eq!(
            waiting_for(&lab.shared, &pr(8)),
            Outcome::Ok(Reply::Permissions(Vec::new()))
        );
        answer(&lab.shared, &first, PermissionAnswerKind::Deny).await;
        one.await.unwrap().unwrap();
        let Outcome::Ok(Reply::Permissions(rest)) = waiting_for(&lab.shared, &pr(7)) else {
            panic!("a list of requests");
        };
        assert_eq!(rest.len(), 1);
        answer(&lab.shared, &second, PermissionAnswerKind::Deny).await;
        two.await.unwrap().unwrap();
    }

    fn request_of(turn: u64) -> PermissionRequest {
        PermissionRequest {
            id: "perm-late".into(),
            pr: pr(7),
            turn,
            tool: "Bash".into(),
            summary: "make".into(),
            reason: None,
            prefix: None,
            sandbox: true,
            deadline: now_ms() + 120_000,
            detail: None,
        }
    }

    #[tokio::test]
    async fn a_request_racing_the_turns_end_is_never_left_waiting() {
        let mut lab = lab();
        // The turn ends between the check that let the ask in and the moment it registers:
        // `register` checks again, under the lock that ending a turn needs.
        cancel_turn(&lab.shared, &pr(7), 1).await;
        let refused = register(&lab.shared, request_of(1), None);
        assert!(refused.is_err());
        assert_eq!(lab.shared.permissions.waiting(), 0);
        while let Ok((_, event)) = lab.events.try_recv() {
            assert!(
                !matches!(event, Event::PermissionRequested { .. }),
                "a dead turn's request is never announced"
            );
        }
        // Asking again says the same, and the next turn of the review is unaffected.
        let again = lab.ask("Bash", bash("make")).await.unwrap();
        assert!(again.unwrap_err().contains("not running"));
        lab.shared.sessions.pretend_running(&pr(7), 2);
        let next = lab.ask_turn(2, "Bash", bash("make"));
        let (id, _) = lab.requested().await;
        answer(&lab.shared, &id, PermissionAnswerKind::Once).await;
        assert!(next.await.unwrap().unwrap().allow);

        // A daemon on its way out takes no new request either.
        lab.shared.trigger_shutdown();
        assert!(register(&lab.shared, request_of(2), None).is_err());
        assert_eq!(lab.shared.permissions.waiting(), 0);
    }

    #[test]
    fn a_denial_already_told_is_not_told_again() {
        let permissions = Permissions::default();
        permissions.note_denial(&pr(7), 1, "Bash");
        permissions.note_denial(&pr(7), 1, "Bash");
        assert!(!permissions.already_told(&pr(7), 2, "Bash"), "another turn");
        assert!(!permissions.already_told(&pr(7), 1, "Edit"), "another tool");
        assert!(permissions.already_told(&pr(7), 1, "Bash"));
        assert!(permissions.already_told(&pr(7), 1, "Bash"));
        assert!(
            !permissions.already_told(&pr(7), 1, "Bash"),
            "each report uses one up"
        );
        permissions.note_denial(&pr(7), 1, "Bash");
        permissions.forget_turn(&pr(7), 1);
        assert!(!permissions.already_told(&pr(7), 1, "Bash"));
    }

    fn unsandboxed(command: &str) -> Value {
        json!({"command": command, "dangerouslyDisableSandbox": true})
    }

    #[tokio::test]
    async fn any_flag_value_but_false_or_null_leaves_the_sandbox() {
        for flag in [json!("true"), json!(1), json!("yes"), json!("false")] {
            let lab = lab();
            lab.shared.sessions.set_turn_sandbox(&pr(7), 1, true);
            let decision = lab
                .ask(
                    "Bash",
                    json!({"command": "cargo test", "dangerouslyDisableSandbox": flag}),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(decision, PermissionDecision::denied(UNSANDBOXED), "{flag}");
        }
    }

    #[tokio::test]
    async fn a_command_that_leaves_the_sandbox_is_denied_while_the_sandbox_is_on() {
        let mut lab = lab();
        lab.shared.sessions.set_turn_sandbox(&pr(7), 1, true);
        assert!(add_rule(&lab.shared, &pr(7), "Bash(cargo test:*)").await);
        lab.events.recv().await.unwrap();
        let decision = lab
            .ask("Bash", unsandboxed("cargo test"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decision, PermissionDecision::denied(UNSANDBOXED));
        assert_eq!(
            UNSANDBOXED,
            "Clúsia keeps commands in the sandbox; turn the sandbox off in Config › Harness to allow this"
        );
        let told = lab.events.recv().await.unwrap().1;
        assert!(
            matches!(
                &told,
                Event::PermissionResolved { tool, summary, outcome: PermissionOutcome::Denied, .. }
                    if tool == "Bash" && summary == "cargo test"
            ),
            "no rule covers it and nobody is asked: {told:?}"
        );
        assert_eq!(outcomes(&lab.log()), [PermissionOutcome::Denied]);
        assert_eq!(lab.shared.permissions.waiting(), 0);
    }

    #[tokio::test]
    async fn without_the_sandbox_leaving_it_is_asked_as_usual() {
        let mut lab = lab();
        lab.shared.sessions.set_turn_sandbox(&pr(7), 1, false);
        let asked = lab.ask("Bash", unsandboxed("curl https://example.com"));
        let (id, requested) = lab.requested().await;
        assert!(
            matches!(requested, Event::PermissionRequested { sandbox: false, .. }),
            "the modal says the network is allowed"
        );
        answer(&lab.shared, &id, PermissionAnswerKind::Once).await;
        assert!(asked.await.unwrap().unwrap().allow);
    }

    #[tokio::test]
    async fn a_request_tells_the_sandbox_its_turn_started_with() {
        let mut lab = lab();
        lab.shared.sessions.set_turn_sandbox(&pr(7), 1, true);
        lab.shared.config.write().await.harness.sandbox = false;
        let asked = lab.ask("Bash", bash("cargo test"));
        let (id, requested) = lab.requested().await;
        assert!(matches!(
            requested,
            Event::PermissionRequested { sandbox: true, .. }
        ));
        answer(&lab.shared, &id, PermissionAnswerKind::Deny).await;
        asked.await.unwrap().unwrap();
        // The switch changed after the turn started: leaving the sandbox is still refused.
        let decision = lab
            .ask("Bash", unsandboxed("cargo test"))
            .await
            .unwrap()
            .unwrap();
        assert!(!decision.allow);
    }

    #[tokio::test]
    async fn the_end_of_a_review_forgets_its_last_turn_once_nothing_runs() {
        let lab = lab();
        cancel_turn(&lab.shared, &pr(7), 1).await;
        // The review ends while its stopped program is still running: it may not ask.
        agent::forget(&lab.shared, &pr(7));
        assert!(lab.shared.permissions.is_closed(&pr(7), 1));
        lab.shared.sessions.pretend_stopped(&pr(7));
        agent::stop(&lab.shared, &pr(7)).await;
        assert!(!lab.shared.permissions.is_closed(&pr(7), 1));
        assert!(
            lab.shared.permissions.closed.lock().unwrap().is_empty(),
            "nothing is kept for a review that ended"
        );
    }
}
