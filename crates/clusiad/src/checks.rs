//! Security and audit checks: each one is a Claude Code turn forked from the review's session,
//! run beside the chat. A review has at most one run of each kind, and at most `CHECK_PLACES`
//! runs go at once across all reviews.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clusia_core::checks::{
    AuditArea, CheckKind, CheckResult, Finding, PromptContext, anchor_in_diff, audit_prompt,
    security_prompt,
};
use clusia_core::{AgentState, DraftKind, FileDiff, Origin, PrRef, Side};
use clusia_harness::{AgentEvent, extract_check_blocks};
use clusia_protocol::{
    AgentErrorKind, AgentLogEntry, AnchorInput, CheckState, CheckStatus, ErrorCode, Event, Outcome,
    ProtocolError, Reply, topics,
};
use clusia_store::agent::load_agent_state;
use clusia_store::{delete_checks, load_checks, load_review_cache, save_checks};
use tokio::sync::{Semaphore, watch};

use crate::agent_log;
use crate::reviews;
use crate::state::Shared;
use crate::sync::now_unix;
use crate::turns::{self, Boxed, SessionSource, Stop, TurnEnd, TurnRun, TurnSink, Why};

/// Checks running at the same time across all reviews; the rest wait.
pub(crate) const CHECK_PLACES: usize = 2;
/// How long `stop_all` waits for the runs of a review to end.
const STOP_WAIT: Duration = Duration::from_secs(4);
/// How long a shutdown waits for the runs to end.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
const KINDS: [CheckKind; 2] = [CheckKind::Security, CheckKind::Audit];

/// A run that waits for a place or is running.
struct Run {
    turn: u64,
    stop: Arc<Stop>,
    /// `Waiting` or `Running`.
    state: CheckState,
}

#[derive(Default)]
struct Slot {
    run: Option<Run>,
    /// Why the last run of this kind failed, until the next run starts.
    failed: Option<String>,
}

#[derive(Default)]
struct Entry {
    security: Slot,
    audit: Slot,
    /// The kinds to start when the summary turn ends well.
    awaiting: Vec<CheckKind>,
}

impl Entry {
    fn slot(&mut self, kind: CheckKind) -> &mut Slot {
        match kind {
            CheckKind::Security => &mut self.security,
            CheckKind::Audit => &mut self.audit,
        }
    }

    fn run_of(&self, kind: CheckKind) -> Option<&Run> {
        match kind {
            CheckKind::Security => self.security.run.as_ref(),
            CheckKind::Audit => self.audit.run.as_ref(),
        }
    }

    fn idle(&self) -> bool {
        self.security.run.is_none() && self.audit.run.is_none()
    }

    fn empty(&self) -> bool {
        self.idle()
            && self.security.failed.is_none()
            && self.audit.failed.is_none()
            && self.awaiting.is_empty()
    }
}

/// The running checks of every review.
pub(crate) struct Checks {
    table: Mutex<HashMap<PrRef, Entry>>,
    places: Arc<Semaphore>,
    /// How many runs live; shutdown waits for zero.
    live: watch::Sender<usize>,
    closing: AtomicBool,
}

impl Default for Checks {
    fn default() -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
            places: Arc::new(Semaphore::new(CHECK_PLACES)),
            live: watch::channel(0).0,
            closing: AtomicBool::new(false),
        }
    }
}

impl Checks {
    /// Reads or changes the entry of `pr`; an entry left with nothing in it is dropped.
    fn with<R>(&self, pr: &PrRef, change: impl FnOnce(&mut Entry) -> R) -> R {
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        let entry = table.entry(pr.clone()).or_default();
        let out = change(entry);
        if entry.empty() {
            table.remove(pr);
        }
        out
    }

    /// What a run of `kind` is doing now (waiting for the summary counts), or why the last one
    /// failed; `None` when idle.
    fn live_state(&self, pr: &PrRef, kind: CheckKind) -> Option<CheckState> {
        self.with(pr, |entry| {
            let awaiting = entry.awaiting.contains(&kind);
            let slot = entry.slot(kind);
            match &slot.run {
                Some(run) => Some(run.state.clone()),
                // Waiting for the summary to end counts as waiting.
                None if awaiting => Some(CheckState::Waiting),
                None => slot
                    .failed
                    .clone()
                    .map(|message| CheckState::Failed { message }),
            }
        })
    }

    fn set_state(&self, pr: &PrRef, kind: CheckKind, turn: u64, state: CheckState) {
        self.with(pr, |entry| {
            if let Some(run) = entry.slot(kind).run.as_mut().filter(|r| r.turn == turn) {
                run.state = state;
            }
        });
    }

    /// Ends the run `turn` of `kind`, leaving `failed` as the reason; `false` when that run is
    /// not the registered one any more.
    fn end_run(&self, pr: &PrRef, kind: CheckKind, turn: u64, failed: Option<String>) -> bool {
        self.with(pr, |entry| {
            let slot = entry.slot(kind);
            if slot.run.as_ref().is_some_and(|r| r.turn == turn) {
                slot.run = None;
                slot.failed = failed;
                true
            } else {
                false
            }
        })
    }
}

/// Counts a run in `Checks::live` while it lives.
struct Live(Arc<Shared>);

impl Live {
    fn new(shared: &Arc<Shared>) -> Self {
        shared.checks.live.send_modify(|n| *n += 1);
        Self(shared.clone())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.checks.live.send_modify(|n| *n -= 1);
    }
}

/// What a run needs once it has been accepted.
struct Job {
    turn: u64,
    prompt: String,
    /// The prompt for a review with no session to fork.
    fresh_prompt: String,
    /// The area ids the answer may name: `security`, or the enabled audit areas.
    areas: Vec<String>,
    files: Vec<FileDiff>,
    /// The head the run reads.
    head: String,
    limit: Duration,
    stop: Arc<Stop>,
}

/// What `on_open` decided to run, for the Agent row of the loading screen.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    pub security: bool,
    /// How many areas the audit reads, when it runs.
    pub audit_areas: Option<usize>,
}

/// The Agent row's note: what the agent does as the review opens, only the parts that run.
pub(crate) fn note(summarizing: bool, plan: &Plan) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if summarizing {
        parts.push("Summarizing".into());
    }
    if plan.security {
        parts.push("Checking security".into());
    }
    if let Some(n) = plan.audit_areas {
        let unit = if n == 1 { "area" } else { "areas" };
        parts.push(format!("Auditing ({n} {unit})"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn tell_state(shared: &Shared, pr: &PrRef, kind: CheckKind, state: CheckState) {
    shared.publish(
        topics::AGENT,
        Event::CheckState {
            pr: pr.clone(),
            kind,
            state,
        },
    );
}

fn note_log(shared: &Shared, pr: &PrRef, turn: u64, kind: CheckKind, state: &str) {
    let entry = AgentLogEntry::Check {
        at: now_unix(),
        turn,
        kind,
        state: state.to_string(),
    };
    if let Err(e) = agent_log::append(&shared.paths.agent_log(pr), &entry) {
        tracing::warn!(error = %e, pr = %pr, "cannot append to the agent log");
    }
}

fn stored(shared: &Shared, pr: &PrRef) -> Vec<CheckResult> {
    load_checks(&shared.paths, pr).unwrap_or_else(|e| {
        tracing::warn!(error = %e, pr = %pr, "cannot read the stored checks");
        Vec::new()
    })
}

fn current_head(shared: &Shared, pr: &PrRef) -> Option<String> {
    reviews::load_stored(shared, pr)
        .ok()
        .flatten()
        .map(|review| review.head_sha)
}

/// Where `kind` stands when nothing runs: its stored result is `Done` for the head the review
/// has now and `Stale` for another one.
fn stored_state(
    shared: &Shared,
    pr: &PrRef,
    kind: CheckKind,
    results: &[CheckResult],
) -> CheckState {
    let Some(result) = results.iter().find(|r| r.kind == kind) else {
        return CheckState::NotRun;
    };
    match current_head(shared, pr) {
        Some(head) if head != result.head => CheckState::Stale,
        _ => CheckState::Done,
    }
}

fn state_of(shared: &Shared, pr: &PrRef, kind: CheckKind) -> CheckState {
    match shared.checks.live_state(pr, kind) {
        Some(state) => state,
        None => stored_state(shared, pr, kind, &stored(shared, pr)),
    }
}

enum Refused {
    Busy,
    Closing,
}

/// Accepts a run of `kind` for `pr` and starts it; waits for a place in the background. A run
/// that is already waiting or running is left alone.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
async fn start(shared: &Arc<Shared>, pr: &PrRef, kind: CheckKind) -> Result<(), Outcome> {
    if shared.checks.closing.load(Ordering::SeqCst) {
        return Err(reviews::invalid_state("clusiad is stopping"));
    }
    let review = match reviews::load_stored(shared, pr) {
        Ok(Some(review)) if shared.paths.worktree_for(pr).is_dir() => review,
        Ok(_) => {
            return Err(reviews::invalid_state(format!(
                "no review for {pr}; open it first"
            )));
        }
        Err(out) => return Err(out),
    };
    let harness = shared.config.read().await.harness.clone();
    let areas: Vec<AuditArea> = harness
        .audit_areas
        .iter()
        .filter(|area| area.enabled)
        .cloned()
        .collect();
    if kind == CheckKind::Audit && areas.is_empty() {
        return Err(reviews::invalid_state(
            "no audit area is on; switch one on in Config › Harness",
        ));
    }
    let (title, files) = match load_review_cache(&shared.paths, pr) {
        Ok(Some(cache)) => (cache.pr.summary.title, cache.files),
        _ => (review.title.clone(), Vec::new()),
    };
    let names: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
    let prompt_for = |forked: bool| {
        let context = PromptContext {
            pr_title: title.clone(),
            files: names.clone(),
            forked,
        };
        match kind {
            CheckKind::Security => security_prompt(&context),
            CheckKind::Audit => audit_prompt(&context, &areas),
        }
    };
    let turn = shared.sessions.next_turn(shared, pr).await;
    let stop = Stop::new();
    let placed = shared.checks.with(pr, |entry| {
        if shared.checks.closing.load(Ordering::SeqCst) {
            return Err(Refused::Closing);
        }
        let slot = entry.slot(kind);
        if slot.run.is_some() {
            return Err(Refused::Busy);
        }
        slot.failed = None;
        slot.run = Some(Run {
            turn,
            stop: stop.clone(),
            state: CheckState::Waiting,
        });
        entry.awaiting.retain(|k| *k != kind);
        Ok(Live::new(shared))
    });
    let live = match placed {
        Ok(live) => live,
        Err(Refused::Busy) => return Ok(()),
        Err(Refused::Closing) => return Err(reviews::invalid_state("clusiad is stopping")),
    };
    tell_state(shared, pr, kind, CheckState::Waiting);
    note_log(shared, pr, turn, kind, "waiting");
    let job = Job {
        turn,
        prompt: prompt_for(true),
        fresh_prompt: prompt_for(false),
        areas: match kind {
            CheckKind::Security => vec!["security".to_string()],
            CheckKind::Audit => areas.iter().map(|area| area.id.clone()).collect(),
        },
        files,
        head: review.head_sha.clone(),
        limit: Duration::from_secs(u64::from(harness.check_timeout_secs)),
        stop,
    };
    tokio::spawn(drive(live, shared.clone(), pr.clone(), kind, job));
    Ok(())
}

/// Runs one check to its end, and settles it whatever happens inside.
async fn drive(_live: Live, shared: Arc<Shared>, pr: PrRef, kind: CheckKind, job: Job) {
    let turn = job.turn;
    let task = tokio::spawn(execute(shared.clone(), pr.clone(), kind, job));
    if task.await.is_err() {
        tracing::error!(pr = %pr, turn, "a check failed inside the daemon");
        fail(
            &shared,
            &pr,
            kind,
            turn,
            "The check stopped unexpectedly".to_string(),
        );
    }
}

async fn execute(shared: Arc<Shared>, pr: PrRef, kind: CheckKind, job: Job) {
    let mut sink = CheckSink::new(shared.clone(), pr.clone(), kind, job.turn);
    let run = TurnRun {
        pr: pr.clone(),
        turn: job.turn,
        prompt: job.prompt.clone(),
        fresh_prompt: Some(job.fresh_prompt.clone()),
        session: SessionSource::Fork,
        origin: kind.into(),
        places: shared.checks.places.clone(),
        limit: Some(job.limit),
        stop: job.stop.clone(),
    };
    let end = turns::run(&shared, run, &mut sink).await;
    match end {
        TurnEnd::Done => complete(&shared, &pr, kind, &job, sink).await,
        TurnEnd::Failed => {
            let message = sink
                .message
                .unwrap_or_else(|| "Claude Code could not finish the check".to_string());
            fail(&shared, &pr, kind, job.turn, message);
        }
        TurnEnd::TimedOut => fail(
            &shared,
            &pr,
            kind,
            job.turn,
            format!(
                "The check ran past its limit of {}s and was stopped",
                job.limit.as_secs()
            ),
        ),
        TurnEnd::Stopped(why) => stopped(&shared, &pr, kind, job.turn, why),
    }
}

/// Reads what a run's turn says. The session id is ignored on purpose: a check's session never
/// becomes the chat's.
struct CheckSink {
    shared: Arc<Shared>,
    pr: PrRef,
    kind: CheckKind,
    turn: u64,
    /// The text streamed so far, used when no result line carries the answer.
    streamed: String,
    answer: Option<String>,
    saw_final: bool,
    failed: bool,
    message: Option<String>,
}

impl CheckSink {
    fn new(shared: Arc<Shared>, pr: PrRef, kind: CheckKind, turn: u64) -> Self {
        Self {
            shared,
            pr,
            kind,
            turn,
            streamed: String::new(),
            answer: None,
            saw_final: false,
            failed: false,
            message: None,
        }
    }

    fn running(&self, activity: Option<String>) {
        let state = CheckState::Running { activity };
        self.shared
            .checks
            .set_state(&self.pr, self.kind, self.turn, state.clone());
        tell_state(&self.shared, &self.pr, self.kind, state);
    }

    fn fail_with(&mut self, message: String) {
        self.failed = true;
        self.message.get_or_insert(message);
    }

    fn handle(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(text) => self.streamed.push_str(&text),
            AgentEvent::ToolUse(summary) => self.running(Some(summary)),
            AgentEvent::Denied { .. } | AgentEvent::SessionId(_) => {}
            AgentEvent::Error { message, .. } => self.fail_with(message),
            AgentEvent::Final { text, is_error, .. } => {
                self.saw_final = true;
                if !is_error {
                    self.answer = Some(text);
                } else if text.trim().is_empty() {
                    self.fail_with("Claude Code could not finish the check".to_string());
                } else {
                    self.fail_with(text.trim().to_string());
                }
            }
        }
    }
}

impl TurnSink for CheckSink {
    fn waiting(&mut self) {}

    fn started(&mut self) {
        self.running(None);
        note_log(&self.shared, &self.pr, self.turn, self.kind, "running");
    }

    fn event(&mut self, event: AgentEvent) -> Boxed<'_> {
        Box::pin(async move { self.handle(event) })
    }

    fn error(&mut self, _kind: AgentErrorKind, message: String) {
        self.fail_with(message);
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

/// Where a finding points, as the range it covers: its line, or its first and last lines.
fn lines_of(finding: &Finding) -> Option<(u32, u32)> {
    Some((finding.first_line()?, finding.last_line()?))
}

/// `file:line`, `file:start-end` for a range, `file` for none.
fn place_of(finding: &Finding) -> String {
    match (finding.line, finding.start_line, finding.end_line) {
        (Some(line), _, _) => format!("{}:{line}", finding.file),
        (None, Some(start), Some(end)) => format!("{}:{start}-{end}", finding.file),
        _ => finding.file.clone(),
    }
}

/// Turns the answer of a run that ended well into a result: the blocks it holds, minus the
/// findings dismissed before, anchored where the diff has the lines. Stored, then told.
async fn complete(shared: &Arc<Shared>, pr: &PrRef, kind: CheckKind, job: &Job, sink: CheckSink) {
    let text = sink.answer.unwrap_or(sink.streamed);
    let (_prose, mut findings, passes, unreadable) = extract_check_blocks(&text, kind, &job.areas);
    let mut seen = HashSet::new();
    findings.retain(|f| seen.insert(f.id.clone()));
    for finding in &mut findings {
        finding.anchored = lines_of(finding)
            .is_some_and(|(start, end)| anchor_in_diff(&job.files, &finding.file, start, end));
    }
    let mut result = CheckResult {
        kind,
        head: job.head.clone(),
        files: u32::try_from(job.files.len()).unwrap_or(u32::MAX),
        findings,
        passes,
        unreadable,
        areas: match kind {
            CheckKind::Security => Vec::new(),
            CheckKind::Audit => job.areas.clone(),
        },
        at: now_unix(),
    };
    // Under the review lock: a review that ended while the run finished has no result to keep,
    // and a finding dismissed while the run finished must not come back.
    let kept = {
        let _guard = reviews::lock(shared, pr).await;
        let known = load_agent_state(&shared.paths, pr).unwrap_or_default();
        result
            .findings
            .retain(|f| !known.dismissed_findings.contains(&f.id));
        let alive =
            job.stop.why() != Why::Ended && matches!(reviews::load_stored(shared, pr), Ok(Some(_)));
        if alive {
            let mut results = stored(shared, pr);
            results.retain(|r| r.kind != kind);
            results.push(result.clone());
            if let Err(e) = save_checks(&shared.paths, pr, &results) {
                tracing::warn!(error = %e, pr = %pr, "cannot store the check result");
            }
        }
        alive
    };
    shared.checks.end_run(pr, kind, job.turn, None);
    if !kept {
        // The review ended while the run finished: the turn still gets its ending.
        note_log(shared, pr, job.turn, kind, "stopped");
        tell_state(shared, pr, kind, state_of(shared, pr, kind));
        return;
    }
    for finding in &result.findings {
        shared.publish(
            topics::AGENT,
            Event::CheckFinding {
                pr: pr.clone(),
                kind,
                finding: finding.clone(),
            },
        );
    }
    for pass in &result.passes {
        shared.publish(
            topics::AGENT,
            Event::CheckPass {
                pr: pr.clone(),
                kind,
                pass: pass.clone(),
            },
        );
    }
    shared.publish(
        topics::AGENT,
        Event::CheckDone {
            pr: pr.clone(),
            kind,
            result,
        },
    );
    tell_state(shared, pr, kind, state_of(shared, pr, kind));
    note_log(shared, pr, job.turn, kind, "done");
    // The review moved to a new head while this run read the old one: when the kind is on, it
    // runs again on the new head.
    let on = {
        let config = shared.config.read().await;
        match kind {
            CheckKind::Security => config.harness.check_security,
            CheckKind::Audit => config.harness.audit,
        }
    };
    if on && current_head(shared, pr).is_some_and(|head| head != job.head) {
        let _ = restart(shared.clone(), pr.clone(), kind).await;
    }
}

/// `start` for a run that ends by starting another. The boxed future has a named type, which
/// cuts the cycle `start` → `drive` → `complete` → `start` that the compiler cannot resolve for
/// an opaque `async fn` type.
fn restart(
    shared: Arc<Shared>,
    pr: PrRef,
    kind: CheckKind,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), Outcome>> + Send>> {
    Box::pin(async move { start(&shared, &pr, kind).await })
}

fn fail(shared: &Shared, pr: &PrRef, kind: CheckKind, turn: u64, message: String) {
    if !shared.checks.end_run(pr, kind, turn, Some(message.clone())) {
        return;
    }
    note_log(shared, pr, turn, kind, "failed");
    tell_state(shared, pr, kind, CheckState::Failed { message });
}

/// A run that was stopped always gets its ending in the log. After the user's Stop the previous
/// result, if there is one, is sent again, so a window that cleared it when the run began shows
/// it back. A review that ended (or a publish that failed after stopping the runs) is told
/// where the kind stands now. A shutdown tells nothing.
fn stopped(shared: &Shared, pr: &PrRef, kind: CheckKind, turn: u64, why: Why) {
    shared.checks.end_run(pr, kind, turn, None);
    note_log(shared, pr, turn, kind, "stopped");
    if why == Why::Shutdown {
        return;
    }
    if why == Why::User
        && let Some(result) = stored(shared, pr).into_iter().find(|r| r.kind == kind)
    {
        shared.publish(
            topics::AGENT,
            Event::CheckDone {
                pr: pr.clone(),
                kind,
                result,
            },
        );
    }
    tell_state(shared, pr, kind, state_of(shared, pr, kind));
}

/// `RunCheck`: starts the check, or leaves the one that already waits or runs.
pub(crate) async fn run(shared: &Arc<Shared>, pr: &PrRef, kind: CheckKind) -> Outcome {
    match start(shared, pr, kind).await {
        Ok(()) => Outcome::Ok(Reply::Ack),
        Err(out) => out,
    }
}

/// `StopCheck`: stops the run of `kind`, if one waits or runs, and forgets that it was to start
/// when the summary ends.
pub(crate) fn stop(shared: &Shared, pr: &PrRef, kind: CheckKind) -> Outcome {
    let was_awaiting = shared.checks.with(pr, |entry| {
        let was = entry.awaiting.contains(&kind);
        entry.awaiting.retain(|k| *k != kind);
        if let Some(run) = entry.run_of(kind) {
            run.stop.fire(Why::User);
        }
        was && entry.run_of(kind).is_none()
    });
    if was_awaiting {
        tell_state(shared, pr, kind, state_of(shared, pr, kind));
    }
    Outcome::Ok(Reply::Ack)
}

/// The review ended or was left (closed, published, discarded): its runs are stopped, for `why`,
/// and nothing more starts. The stored results stay (`forget` deletes them). Waits until the runs
/// are gone, so the caller may take the review lock after.
pub(crate) async fn stop_all(shared: &Shared, pr: &PrRef, why: Why) {
    shared.checks.with(pr, |entry| {
        entry.awaiting.clear();
        for kind in KINDS {
            if let Some(run) = entry.run_of(kind) {
                run.stop.fire(why);
            }
        }
    });
    let give_up = Instant::now() + STOP_WAIT;
    while !shared.checks.with(pr, |entry| entry.idle()) && Instant::now() < give_up {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Daemon shutdown: no new run, every run stopped, and a wait until none is left.
pub(crate) async fn shutdown(shared: &Shared) {
    let checks = &shared.checks;
    checks.closing.store(true, Ordering::SeqCst);
    {
        let mut table = checks.table.lock().unwrap_or_else(|p| p.into_inner());
        for entry in table.values_mut() {
            entry.awaiting.clear();
            for kind in KINDS {
                if let Some(run) = entry.run_of(kind) {
                    run.stop.fire(Why::Shutdown);
                }
            }
        }
    }
    let mut live = checks.live.subscribe();
    let _ = tokio::time::timeout(SHUTDOWN_WAIT, live.wait_for(|n| *n == 0)).await;
}

/// The review was published or discarded: what ran is stopped, the results are deleted, and the
/// windows hear that nothing was run. Closing an empty review does not come here: its results
/// stay until retention. The caller may hold the review lock.
pub(crate) fn forget(shared: &Shared, pr: &PrRef) {
    let removed = shared
        .checks
        .table
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(pr);
    if let Some(entry) = &removed {
        for kind in KINDS {
            if let Some(run) = entry.run_of(kind) {
                run.stop.fire(Why::Ended);
            }
        }
    }
    drop_results(shared, pr);
    for kind in KINDS {
        tell_state(shared, pr, kind, CheckState::NotRun);
    }
}

/// Deletes the stored results of `pr`.
pub(crate) fn drop_results(shared: &Shared, pr: &PrRef) {
    if let Err(e) = delete_checks(&shared.paths, pr) {
        tracing::warn!(error = %e, pr = %pr, "cannot delete the stored checks");
    }
}

/// What the checks do as the review opens. A kind that is on in the settings runs when it has
/// no result for this head: at once, or, when a summary turn is running, right after it ends
/// (`on_summary_over`). A kind that does not run but holds a result of another head is told
/// `Stale`, so a window that shows the old findings stops offering them. `Plan` says what runs.
///
/// Called by `reviews::open` while it holds the review lock; `on_summary_over` takes the same
/// lock before it reads `awaiting`, so a summary that ends between the check that one runs and
/// the `awaiting` list being set is not missed.
pub(crate) async fn on_open(
    shared: &Arc<Shared>,
    pr: &PrRef,
    head: &str,
    summarized: bool,
) -> Plan {
    let harness = shared.config.read().await.harness.clone();
    let results = stored(shared, pr);
    let due = |kind: CheckKind| {
        let running = matches!(
            shared.checks.live_state(pr, kind),
            Some(CheckState::Waiting | CheckState::Running { .. })
        );
        running
            || results
                .iter()
                .find(|r| r.kind == kind)
                .is_none_or(|r| r.head != head)
    };
    let areas = harness.audit_areas.iter().filter(|a| a.enabled).count();
    let mut plan = Plan::default();
    let mut kinds = Vec::new();
    if harness.check_security && due(CheckKind::Security) {
        plan.security = true;
        kinds.push(CheckKind::Security);
    }
    if harness.audit && areas > 0 && due(CheckKind::Audit) {
        plan.audit_areas = Some(areas);
        kinds.push(CheckKind::Audit);
    }
    for kind in KINDS {
        let stale = results.iter().any(|r| r.kind == kind && r.head != head);
        if stale && !kinds.contains(&kind) && shared.checks.live_state(pr, kind).is_none() {
            tell_state(shared, pr, kind, CheckState::Stale);
        }
    }
    if summarized {
        // A kind with a run that waits or runs keeps its own state; it does not wait for the
        // summary. One already waiting for it keeps waiting.
        kinds.retain(|kind| {
            shared
                .checks
                .with(pr, |entry| entry.run_of(*kind).is_none())
        });
        shared.checks.with(pr, |entry| {
            for kind in &kinds {
                if !entry.awaiting.contains(kind) {
                    entry.awaiting.push(*kind);
                }
            }
        });
        for kind in kinds {
            tell_state(shared, pr, kind, CheckState::Waiting);
        }
    } else {
        for kind in kinds {
            if start(shared, pr, kind).await.is_err() {
                tracing::warn!(pr = %pr, "could not start a check as the review opened");
            }
        }
    }
    plan
}

/// The summary turn is over, whatever way it ended except with the review or the daemon (the chat
/// calls this for those only): the checks that waited for it start now. They fork the review's
/// session when it has one, and start their own otherwise. `awaiting` is read under the review
/// lock, which `reviews::open` holds while `on_open` sets it, so a summary that ends right away
/// waits for the open to finish and cannot be missed.
pub(crate) async fn on_summary_over(shared: &Arc<Shared>, pr: &PrRef) {
    let kinds = {
        let _guard = reviews::lock(shared, pr).await;
        shared
            .checks
            .with(pr, |entry| std::mem::take(&mut entry.awaiting))
    };
    for kind in kinds {
        if start(shared, pr, kind).await.is_err() {
            tracing::warn!(pr = %pr, "could not start a check after the summary");
        }
    }
}

/// `GetChecks`: every stored result as it is, where each kind stands, and the ids the human
/// accepted and dismissed.
pub(crate) fn get(shared: &Shared, pr: &PrRef) -> Outcome {
    let results = stored(shared, pr);
    let states = KINDS
        .iter()
        .map(|&kind| CheckStatus {
            kind,
            state: shared
                .checks
                .live_state(pr, kind)
                .unwrap_or_else(|| stored_state(shared, pr, kind, &results)),
        })
        .collect();
    let known = load_agent_state(&shared.paths, pr).unwrap_or_default();
    Outcome::Ok(Reply::Checks {
        results,
        states,
        accepted: known.accepted_findings.iter().cloned().collect(),
        dismissed: known.dismissed_findings.iter().cloned().collect(),
    })
}

fn not_open(id: &str) -> Outcome {
    Outcome::Err(ProtocolError::new(
        ErrorCode::NotFound,
        format!("there is no open finding {id}"),
    ))
}

/// The finding `id` if it is still open: in a stored result and neither accepted nor dismissed.
/// With `current`, a finding of an older head is refused: its lines may have moved.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
fn open_finding(
    shared: &Shared,
    pr: &PrRef,
    id: &str,
    known: &AgentState,
    current: bool,
) -> Result<Finding, Outcome> {
    if known.is_finding_settled(id) {
        return Err(not_open(id));
    }
    let head = current_head(shared, pr);
    for result in stored(shared, pr) {
        let Some(finding) = result.findings.into_iter().find(|f| f.id == id) else {
            continue;
        };
        if current && head.as_deref().is_some_and(|head| head != result.head) {
            return Err(reviews::invalid_state(
                "This finding is from an older version of the pull request; check again first",
            ));
        }
        return Ok(finding);
    }
    Err(not_open(id))
}

fn anchor_of(finding: &Finding) -> Option<AnchorInput> {
    let (start, end) = lines_of(finding)?;
    Some(AnchorInput {
        path: finding.file.clone(),
        line: end,
        start_line: (start != end).then_some(start),
        side: Side::Right,
    })
}

/// `AcceptFinding`: the finding becomes a draft item written by the check that found it. The
/// text may be edited first. A finding with no anchor in the diff becomes a general comment that
/// names its place. The check, the new item and the record that it was accepted happen under one
/// review lock, so two accepts add one item.
pub(crate) async fn accept(
    shared: &Shared,
    client: &str,
    pr: &PrRef,
    id: &str,
    body: Option<String>,
) -> Outcome {
    let guard = reviews::lock(shared, pr).await;
    let known = load_agent_state(&shared.paths, pr).unwrap_or_default();
    let finding = match open_finding(shared, pr, id, &known, true) {
        Ok(finding) => finding,
        Err(out) => return out,
    };
    let edited = body.is_some();
    let text = body.unwrap_or_else(|| finding.comment.clone());
    let (kind, anchor, text) = match anchor_of(&finding).filter(|_| finding.anchored) {
        Some(anchor) => (DraftKind::LineComment, Some(anchor), text),
        None if edited => (DraftKind::General, None, text),
        None => (
            DraftKind::General,
            None,
            format!("{}: {text}", place_of(&finding)),
        ),
    };
    let origin = match finding.kind {
        CheckKind::Security => Origin::Security,
        CheckKind::Audit => Origin::Audit,
    };
    let item = reviews::NewItem {
        kind,
        anchor,
        thread: None,
        body: &text,
    };
    let outcome = reviews::add_item_locked(shared, client, pr, origin, item).await;
    if matches!(outcome, Outcome::Ok(Reply::DraftItem(_))) {
        crate::agent::write_state(shared, pr, |state| {
            state.accept_finding(id);
        });
        drop(guard);
        crate::agent::refresh(shared, pr);
        shared.publish(
            topics::AGENT,
            Event::FindingSettled {
                pr: pr.clone(),
                id: id.to_string(),
                accepted: true,
            },
        );
    }
    outcome
}

/// `DismissFinding`: the finding is not shown or proposed again for this review.
pub(crate) async fn dismiss(shared: &Shared, pr: &PrRef, id: &str) -> Outcome {
    let guard = reviews::lock(shared, pr).await;
    let known = load_agent_state(&shared.paths, pr).unwrap_or_default();
    if let Err(out) = open_finding(shared, pr, id, &known, false) {
        return out;
    }
    crate::agent::write_state(shared, pr, |state| {
        state.dismiss_finding(id);
    });
    drop(guard);
    shared.publish(
        topics::AGENT,
        Event::FindingSettled {
            pr: pr.clone(),
            id: id.to_string(),
            accepted: false,
        },
    );
    Outcome::Ok(Reply::Ack)
}

#[cfg(test)]
mod tests {
    use clusia_core::{Config, Paths, Review};
    use clusia_harness::testkit::{FakeClaude, Script, Turn};
    use clusia_store::save_review;

    use super::*;

    fn pr(n: u64) -> PrRef {
        format!("acme/widgets#{n}").parse().unwrap()
    }

    /// `reviews` saved reviews of acme/widgets (#1 and up), each with an empty worktree, and a
    /// fake `claude` playing `script`.
    fn lab(
        script: Script,
        reviews: u64,
        tweak: impl FnOnce(&mut Config),
    ) -> (tempfile::TempDir, tempfile::TempDir, Arc<Shared>) {
        let home = tempfile::tempdir().unwrap();
        let fake = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(fake.path(), script);
        let mut config = Config::default();
        config.harness.program = Some(program.display().to_string());
        tweak(&mut config);
        let options = crate::options::DaemonOptions {
            claude_program: Some("/nonexistent/claude".into()),
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
        for n in 1..=reviews {
            let review = Review::new(pr(n), "t".into(), "b".into(), "h".into(), 1);
            save_review(&shared.paths, &review).unwrap();
            std::fs::create_dir_all(shared.paths.worktree_for(&pr(n))).unwrap();
        }
        (home, fake, shared)
    }

    async fn calls(fake: &tempfile::TempDir, count: usize) -> Vec<clusia_harness::testkit::Call> {
        let dir = fake.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            FakeClaude::wait_for_calls(&dir, count, Duration::from_secs(5))
        })
        .await
        .unwrap()
    }

    fn running(shared: &Shared, n: u64) -> bool {
        matches!(
            state_of(shared, &pr(n), CheckKind::Security),
            CheckState::Running { .. }
        )
    }

    #[test]
    fn the_note_names_only_what_runs() {
        let plan = |security, audit_areas| Plan {
            security,
            audit_areas,
        };
        assert_eq!(
            note(true, &plan(true, Some(6))).as_deref(),
            Some("Summarizing · Checking security · Auditing (6 areas)")
        );
        assert_eq!(
            note(false, &plan(true, Some(6))).as_deref(),
            Some("Checking security · Auditing (6 areas)")
        );
        assert_eq!(
            note(false, &plan(false, Some(1))).as_deref(),
            Some("Auditing (1 area)")
        );
        assert_eq!(
            note(true, &plan(false, None)).as_deref(),
            Some("Summarizing")
        );
        assert_eq!(note(false, &plan(false, None)), None);
    }

    #[tokio::test]
    async fn at_most_two_checks_run_at_once() {
        let (_home, fake, shared) = lab(Script::one(Turn::hanging()), 5, |_| {});
        for n in 1..=5 {
            assert!(matches!(
                run(&shared, &pr(n), CheckKind::Security).await,
                Outcome::Ok(Reply::Ack)
            ));
        }
        assert_eq!(calls(&fake, 2).await.len(), 2);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            FakeClaude::calls(fake.path()).len(),
            2,
            "the other three wait for a place"
        );
        let going: Vec<u64> = (1..=5).filter(|n| running(&shared, *n)).collect();
        assert_eq!(going.len(), 2, "two run");
        let waiting = (1..=5)
            .filter(|n| state_of(&shared, &pr(*n), CheckKind::Security) == CheckState::Waiting)
            .count();
        assert_eq!(waiting, 3, "three wait");

        let _ = stop(&shared, &pr(going[0]), CheckKind::Security);
        assert_eq!(
            calls(&fake, 3).await.len(),
            3,
            "a free place starts the next"
        );
        shutdown(&shared).await;
        assert!(
            (1..=5).all(|n| !running(&shared, n)),
            "a shutdown leaves nothing running"
        );
    }

    #[tokio::test]
    async fn running_a_check_twice_is_one_run() {
        let (_home, fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        let _ = run(&shared, &pr(1), CheckKind::Security).await;
        let _ = run(&shared, &pr(1), CheckKind::Security).await;
        calls(&fake, 1).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(FakeClaude::calls(fake.path()).len(), 1);
        shutdown(&shared).await;
    }

    #[tokio::test]
    async fn an_audit_with_every_area_off_is_refused() {
        let (_home, fake, shared) = lab(Script::one(Turn::hanging()), 1, |config| {
            for area in &mut config.harness.audit_areas {
                area.enabled = false;
            }
        });
        let Outcome::Err(refusal) = run(&shared, &pr(1), CheckKind::Audit).await else {
            panic!("refused");
        };
        assert_eq!(refusal.code, ErrorCode::InvalidState);
        assert!(refusal.message.contains("Config › Harness"));
        assert!(FakeClaude::calls(fake.path()).is_empty());
    }

    #[tokio::test]
    async fn a_run_past_its_deadline_fails_with_the_limit() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |config| {
            config.harness.check_timeout_secs = 1;
        });
        let _ = run(&shared, &pr(1), CheckKind::Security).await;
        let message = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let CheckState::Failed { message } =
                    state_of(&shared, &pr(1), CheckKind::Security)
                {
                    return message;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the run is stopped at its deadline");
        assert_eq!(
            message,
            "The check ran past its limit of 1s and was stopped"
        );
        shutdown(&shared).await;
    }

    #[tokio::test]
    async fn a_waiting_run_is_in_the_log_and_its_turn_id_is_never_reused() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 3, |_| {});
        for n in 1..=3 {
            let _ = run(&shared, &pr(n), CheckKind::Security).await;
        }
        // Two runs hold the places; the third waits, and is already in its log.
        let waiting = (1..=3)
            .find(|n| state_of(&shared, &pr(*n), CheckKind::Security) == CheckState::Waiting)
            .expect("one run waits for a place");
        let log = shared.paths.agent_log(&pr(waiting));
        let turn = agent_log::read(&log)
            .iter()
            .find_map(|entry| match entry {
                AgentLogEntry::Check { turn, state, .. } if state == "waiting" => Some(*turn),
                _ => None,
            })
            .expect("the run is logged as soon as it has a turn id");
        let _ = stop(&shared, &pr(waiting), CheckKind::Security);
        shutdown(&shared).await;
        shared.sessions.end(&shared, &pr(waiting)).await;
        assert!(
            shared.sessions.next_turn(&shared, &pr(waiting)).await > turn,
            "a turn id handed to a check is not handed out again"
        );
    }

    #[tokio::test]
    async fn a_review_that_is_not_open_cannot_be_checked() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 0, |_| {});
        let Outcome::Err(refusal) = run(&shared, &pr(9), CheckKind::Security).await else {
            panic!("refused");
        };
        assert_eq!(refusal.code, ErrorCode::InvalidState);
        assert!(refusal.message.contains("open it first"));
    }
}
