//! Revalidation of a saved review's draft: when the pull request gets new commits and stays quiet
//! for a while, one forked Claude Code turn re-reads every comment against them. What it says is
//! stored as proposals; the draft changes only when the user decides one.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clusia_core::config::Harness;
use clusia_core::revalidation::{
    AskedItem, CommitLine, DIFF_CAP, Decision, Proposal, Revalidation, RevalidationContext,
    RoundState, RowKind, Verdict, fingerprint, open_round, revalidation_prompt, row_kind,
};
use clusia_core::{
    ActivityKind, Anchor, Draft, DraftKind, FileDiff, PrRef, Relocation, Review, ReviewEvent,
    ReviewState, Side,
};
use clusia_git::{diff_for_prompt, repo_of_worktree};
use clusia_harness::{AgentEvent, RawVerdict, extract_revalidation_blocks};
use clusia_protocol::{AgentErrorKind, AgentLogEntry, Event, TurnOrigin, topics};
use clusia_store::{list_reviews, load_review_cache};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::reviews;
use crate::state::Shared;
use crate::sync::{github_client, now_unix};
use crate::turns::{self, Boxed, SessionSource, Stop, TurnEnd, TurnRun, TurnSink, Why};
use crate::{agent_log, notifications};

/// The failure message a round shows after the user pressed Stop.
pub(crate) const STOPPED: &str = "Stopped";
/// How long a stop waits for the run of a review to end.
const STOP_WAIT: Duration = Duration::from_secs(4);
/// How long a shutdown waits for the runs to end.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// A run that waits for a place or is running.
struct Run {
    turn: u64,
    stop: Arc<Stop>,
    /// The last thing the agent did, told to the windows but not stored.
    activity: Option<String>,
    /// The round as it was last told, so the agent's activity goes out without reading the file.
    told: Option<Revalidation>,
}

#[derive(Default)]
struct Entry {
    /// The timer that starts the round, and the number that tells it apart from a replaced one.
    generation: u64,
    timer: Option<JoinHandle<()>>,
    run: Option<Run>,
}

impl Entry {
    /// Nothing is left to remember: no run, and no timer (a nonzero generation is a timer that
    /// has not fired yet).
    fn empty(&self) -> bool {
        self.run.is_none() && self.generation == 0
    }

    /// Cancels the timer; one that already woke finds its generation gone and does nothing.
    fn cancel_timer(&mut self) {
        self.generation = 0;
        if let Some(timer) = self.timer.take() {
            timer.abort();
        }
    }
}

/// The timers and runs of every review: at most one of each per review.
pub(crate) struct Revalidations {
    table: Mutex<HashMap<PrRef, Entry>>,
    next_generation: AtomicU64,
    /// How many runs live; shutdown waits for zero.
    live: watch::Sender<usize>,
    closing: AtomicBool,
}

impl Default for Revalidations {
    fn default() -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
            live: watch::channel(0).0,
            closing: AtomicBool::new(false),
        }
    }
}

impl Revalidations {
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

    fn activity(&self, pr: &PrRef) -> Option<String> {
        self.with(pr, |entry| {
            entry.run.as_ref().and_then(|r| r.activity.clone())
        })
    }

    fn end_run(&self, pr: &PrRef, turn: u64) {
        self.with(pr, |entry| {
            if entry.run.as_ref().is_some_and(|r| r.turn == turn) {
                entry.run = None;
            }
        });
    }
}

/// Counts a run in `Revalidations::live` while it lives.
struct Live(Arc<Shared>);

impl Live {
    fn new(shared: &Arc<Shared>) -> Self {
        shared.revalidations.live.send_modify(|n| *n += 1);
        Self(shared.clone())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.revalidations.live.send_modify(|n| *n -= 1);
    }
}

/// What the rows of a finished round add up to, for the notification and the activity row.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counts {
    pub moved: u32,
    pub obsolete: u32,
    pub rewording: u32,
}

impl Counts {
    pub(crate) fn of(round: &Revalidation, draft: &Draft) -> Self {
        let mut counts = Self::default();
        for proposal in &round.proposals {
            let Some(item) = draft.get(&proposal.item) else {
                continue;
            };
            match row_kind(proposal, item) {
                RowKind::Moved => counts.moved += 1,
                RowKind::Obsolete => counts.obsolete += 1,
                RowKind::NewWording => counts.rewording += 1,
                RowKind::Kept | RowKind::NotChecked => {}
            }
        }
        counts
    }

    /// `1 moved, 1 obsolete, 1 new wording`; only the parts that are not zero.
    pub(crate) fn text(&self) -> String {
        let mut parts = Vec::new();
        if self.moved > 0 {
            parts.push(format!("{} moved", self.moved));
        }
        if self.obsolete > 0 {
            parts.push(format!("{} obsolete", self.obsolete));
        }
        match self.rewording {
            0 => {}
            1 => parts.push("1 new wording".to_string()),
            n => parts.push(format!("{n} new wordings")),
        }
        if parts.is_empty() {
            "no changes suggested".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Whether an item is one the agent is asked about: a comment the user will publish that carries
/// text of its own. Resolving a thread has nothing to re-read.
fn askable(item: &clusia_core::DraftItem) -> bool {
    item.accepted
        && matches!(
            item.kind,
            DraftKind::LineComment | DraftKind::General | DraftKind::Reply
        )
}

/// The draft's anchors, to be kept before a relocation moves them.
pub(crate) fn snapshot(review: &Review) -> BTreeMap<String, Option<Anchor>> {
    review
        .draft
        .items
        .iter()
        .map(|item| (item.id.clone(), item.anchor.clone()))
        .collect()
}

/// The items to ask about, each with the anchor it had when the round opened.
fn asked_items(review: &Review, round: &Revalidation) -> Vec<AskedItem> {
    review
        .draft
        .items
        .iter()
        .filter(|item| askable(item))
        .map(|item| AskedItem {
            id: item.id.clone(),
            kind: item.kind,
            body: item.body.clone(),
            before: round
                .before
                .get(&item.id)
                .cloned()
                .unwrap_or_else(|| item.anchor.clone()),
            current: item.anchor.clone(),
            status: item.status.clone(),
        })
        .collect()
}

/// Whether this daemon can run Claude Code: the program is a file we can find.
fn available(shared: &Shared, harness: &Harness) -> bool {
    turns::program_path(shared, harness).is_file()
}

/// What the windows and the CLI see of the round: the stored one, with what the agent is doing.
pub(crate) fn current(shared: &Shared, review: &Review) -> Option<Revalidation> {
    let mut round = review.revalidation.clone()?;
    if let RoundState::Running { activity } = &mut round.state
        && let Some(live) = shared.revalidations.activity(&review.pr)
    {
        *activity = Some(live);
    }
    Some(round)
}

/// Tells every window how the round of `review` stands.
pub(crate) fn tell(shared: &Shared, review: &Review) {
    shared.revalidations.with(&review.pr, |entry| {
        if let Some(run) = entry.run.as_mut() {
            run.told = review.revalidation.clone();
        }
    });
    shared.publish(
        topics::REVIEWS,
        Event::RevalidationChanged {
            pr: review.pr.clone(),
            revalidation: current(shared, review),
        },
    );
}

fn note_log(shared: &Shared, pr: &PrRef, turn: u64, state: &str) {
    let entry = AgentLogEntry::Revalidation {
        at: now_unix(),
        turn,
        state: state.to_string(),
    };
    if let Err(e) = agent_log::append(&shared.paths.agent_log(pr), &entry) {
        tracing::warn!(error = %e, pr = %pr, "cannot append to the agent log");
    }
}

/// Opens, or extends, the round of a review that was just moved to a new head, and returns the
/// time the quiet period ends. `None` when nothing is to be revalidated: the switch is off, no
/// Claude Code is configured or the draft has nothing to re-read. `immediate` is a review that
/// is being opened in a window: it does not wait.
///
/// Call it after the relocation, with the anchors taken by [`snapshot`] before it, then save the
/// review and call [`reschedule`].
pub(crate) async fn open_for_new_head(
    shared: &Shared,
    review: &mut Review,
    before: BTreeMap<String, Option<Anchor>>,
    from_head: &str,
    report: Relocation,
    immediate: bool,
) -> Option<i64> {
    let harness = shared.config.read().await.harness.clone();
    if !harness.revalidate
        || !available(shared, &harness)
        || !review.draft.items.iter().any(askable)
    {
        return None;
    }
    let quiet = if immediate {
        0
    } else {
        harness.revalidate_quiet_secs
    };
    let now = now_unix();
    let to_head = review.head_sha.clone();
    open_round(review, before, from_head, &to_head, report, now, quiet);
    Some(now + i64::from(quiet))
}

/// A review that is being opened with a round that has not run yet starts it now. Returns
/// whether a round runs or waits to.
pub(crate) async fn run_on_open(shared: &Arc<Shared>, review: &Review) -> bool {
    let Some(round) = &review.revalidation else {
        return false;
    };
    if !matches!(
        round.state,
        RoundState::Scheduled { .. } | RoundState::Waiting | RoundState::Running { .. }
    ) {
        return false;
    }
    let harness = shared.config.read().await.harness.clone();
    if !harness.revalidate || !available(shared, &harness) {
        return false;
    }
    // Not `schedule`: the quiet period is over because the user is looking.
    let (shared, pr) = (shared.clone(), review.pr.clone());
    tokio::spawn(async move {
        if let Err(out) = start(&shared, &pr, Trigger::Now).await {
            tracing::debug!(pr = %pr, ?out, "the revalidation did not start");
        }
    });
    true
}

/// A round was opened or extended and saved: a run on the older head ends, and the quiet period
/// starts again.
pub(crate) fn reschedule(shared: &Arc<Shared>, pr: &PrRef, at: i64) {
    shared.revalidations.with(pr, |entry| {
        if let Some(run) = &entry.run {
            run.stop.fire(Why::Superseded);
        }
    });
    schedule(shared, pr, at);
}

/// Starts the round of `pr` at `at` (Unix seconds), replacing the timer it had.
pub(crate) fn schedule(shared: &Arc<Shared>, pr: &PrRef, at: i64) {
    if shared.revalidations.closing.load(Ordering::SeqCst) {
        return;
    }
    let generation = shared
        .revalidations
        .next_generation
        .fetch_add(1, Ordering::SeqCst);
    let replaced = shared.revalidations.with(pr, |entry| {
        entry.generation = generation;
        entry.timer.take()
    });
    if let Some(timer) = replaced {
        timer.abort();
    }
    let delay = Duration::from_secs(u64::try_from(at - now_unix()).unwrap_or(0));
    let task = tokio::spawn({
        let shared = shared.clone();
        let pr = pr.clone();
        async move {
            tokio::time::sleep(delay).await;
            // A timer that was replaced while it woke does nothing; the live one detaches
            // itself, so a later `schedule` cannot cancel the run it starts.
            let mine = shared.revalidations.with(&pr, |entry| {
                let mine = entry.generation == generation;
                if mine {
                    entry.generation = 0;
                    entry.timer = None;
                }
                mine
            });
            if mine && let Err(out) = start(&shared, &pr, Trigger::Timer).await {
                unstarted(&shared, &pr, out).await;
            }
        }
    });
    shared.revalidations.with(pr, |entry| {
        if entry.generation == generation && !task.is_finished() {
            entry.timer = Some(task);
        } else {
            task.abort();
        }
    });
}

/// The timer could not start the run (the worktree is gone, the review cannot be read, a publish
/// is under way): the round fails with the reason, and the old notification goes out, so the
/// round is not left waiting for a timer that will not come again.
async fn unstarted(shared: &Arc<Shared>, pr: &PrRef, out: clusia_protocol::Outcome) {
    let clusia_protocol::Outcome::Err(error) = out else {
        return;
    };
    tracing::debug!(pr = %pr, message = %error.message, "the revalidation did not start");
    if shared.revalidations.closing.load(Ordering::SeqCst) {
        return;
    }
    let _guard = reviews::lock(shared, pr).await;
    let Ok(Some(mut review)) = reviews::load_stored(shared, pr) else {
        return;
    };
    let due = review
        .revalidation
        .as_ref()
        .is_some_and(|r| matches!(r.state, RoundState::Scheduled { .. } | RoundState::Waiting));
    if due {
        settle_failed(shared, &mut review, &error.message).await;
    }
}

/// After a restart: every round that was waiting for its quiet period, for a place or running
/// is scheduled again. A round that was running is started over.
pub(crate) async fn resume_all(shared: &Arc<Shared>) {
    let stored = match list_reviews(&shared.paths) {
        Ok(stored) => stored,
        Err(e) => {
            tracing::warn!(error = %e, "cannot list the reviews to revalidate");
            return;
        }
    };
    for review in stored {
        if review.state.is_terminal() {
            continue;
        }
        let at = match review.revalidation.as_ref().map(|r| &r.state) {
            Some(RoundState::Scheduled { at }) => *at,
            Some(RoundState::Waiting | RoundState::Running { .. }) => {
                let _guard = reviews::lock(shared, &review.pr).await;
                let Ok(Some(mut review)) = reviews::load_stored(shared, &review.pr) else {
                    continue;
                };
                let now = now_unix();
                if let Some(round) = review.revalidation.as_mut() {
                    round.state = RoundState::Scheduled { at: now };
                }
                if reviews::save(shared, &review).is_err() {
                    continue;
                }
                now
            }
            _ => continue,
        };
        schedule(shared, &review.pr, at);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Trigger {
    /// The quiet period ended: the round starts only if it is still due.
    Timer,
    /// The user or a window asked for it now.
    Now,
}

/// What a run needs once it has been accepted.
struct Job {
    turn: u64,
    prompt: String,
    /// The prompt for a review with no session to fork.
    fresh_prompt: String,
    asked: Vec<AskedItem>,
    /// The fingerprint of each asked item when it was asked, so an edit made while the agent read
    /// is seen by `decide`.
    fingerprints: HashMap<String, String>,
    /// The head the run reads.
    to_head: String,
    limit: Duration,
    stop: Arc<Stop>,
}

/// The commits after `from_head`, as the prompt and the screen list them.
async fn commit_lines(shared: &Shared, pr: &PrRef, from_head: &str) -> Vec<CommitLine> {
    let Ok(Some(gh)) = github_client(shared).await else {
        return Vec::new();
    };
    let Ok(all) = gh.get_commits(pr).await else {
        return Vec::new();
    };
    let start = all
        .iter()
        .position(|c| c.sha == from_head)
        .map_or(0, |at| at + 1);
    all[start..]
        .iter()
        .map(|c| CommitLine {
            sha: c.sha.chars().take(7).collect(),
            title: clusia_core::printable::printable_lines(c.message.lines().next().unwrap_or("")),
        })
        .collect()
}

/// The pull request's files at `head`, from the cache the last open wrote or from GitHub.
pub(crate) async fn files_at(
    shared: &Shared,
    pr: &PrRef,
    head: &str,
) -> Option<Arc<Vec<FileDiff>>> {
    if let Ok(Some(cache)) = load_review_cache(&shared.paths, pr)
        && cache.pr.head_sha == head
    {
        return Some(Arc::new(cache.files));
    }
    reviews::files_for(shared, pr, head).await.ok()
}

fn invalid(message: impl Into<String>) -> clusia_protocol::Outcome {
    reviews::invalid_state(message)
}

/// Accepts a run for `pr` and starts it; waits for a place in the background. A run that is
/// already waiting or running is left alone.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
async fn start(
    shared: &Arc<Shared>,
    pr: &PrRef,
    trigger: Trigger,
) -> Result<(), clusia_protocol::Outcome> {
    if shared.revalidations.closing.load(Ordering::SeqCst) {
        return Err(invalid("clusiad is stopping"));
    }
    let harness = shared.config.read().await.harness.clone();
    // The commits come from GitHub, so they are read before the review lock is taken.
    let from_head = reviews::load_stored(shared, pr)?
        .and_then(|review| review.revalidation)
        .map(|round| round.from_head);
    let commits = match from_head {
        Some(from) => commit_lines(shared, pr, &from).await,
        None => Vec::new(),
    };
    let _guard = reviews::lock(shared, pr).await;
    let mut review = match reviews::load_stored(shared, pr)? {
        Some(review) if !review.state.is_terminal() && review.state != ReviewState::Publishing => {
            review
        }
        _ => return Err(invalid(format!("no review for {pr}; open it first"))),
    };
    let Some(round) = review.revalidation.as_ref() else {
        return Err(invalid("This review has no new commits"));
    };
    if trigger == Trigger::Timer {
        match round.state {
            RoundState::Scheduled { at } if at <= now_unix() => {}
            // A newer push moved the quiet period on, and a newer timer waits for it.
            _ => return Ok(()),
        }
        if !harness.revalidate {
            settle_failed(
                shared,
                &mut review,
                "Revalidation is off in Config › Harness",
            )
            .await;
            return Ok(());
        }
    }
    let worktree = shared.paths.worktree_for(pr);
    if !worktree.is_dir() {
        return Err(invalid(format!(
            "the worktree of {pr} is gone; open the review again"
        )));
    }
    let asked = asked_items(&review, round);
    let fingerprints: HashMap<String, String> = review
        .draft
        .items
        .iter()
        .map(|item| (item.id.clone(), fingerprint(item)))
        .collect();
    let (from_head, to_head) = (round.from_head.clone(), round.to_head.clone());
    let diff = match repo_of_worktree(&worktree).await {
        Ok(repo) => diff_for_prompt(&repo, &from_head, &to_head, DIFF_CAP)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, pr = %pr, "cannot diff the new commits");
                "[diff unavailable — read the files in the worktree]".to_string()
            }),
        Err(_) => "[diff unavailable — read the files in the worktree]".to_string(),
    };
    let commits = if commits.is_empty() {
        round.commits.clone()
    } else {
        commits
    };
    let prompt_for = |forked: bool| {
        revalidation_prompt(&RevalidationContext {
            pr_title: review.title.clone(),
            diff: diff.clone(),
            commits: commits.clone(),
            items: asked.clone(),
            forked,
        })
    };
    let turn = shared.sessions.next_turn(shared, pr).await;
    let stop = Stop::new();
    let placed = shared.revalidations.with(pr, |entry| {
        if shared.revalidations.closing.load(Ordering::SeqCst) {
            return Err(invalid("clusiad is stopping"));
        }
        if entry.run.is_some() {
            return Ok(None);
        }
        if trigger == Trigger::Now {
            entry.cancel_timer();
        }
        entry.run = Some(Run {
            turn,
            stop: stop.clone(),
            activity: None,
            told: None,
        });
        Ok(Some(Live::new(shared)))
    });
    let live = match placed? {
        Some(live) => live,
        None => return Ok(()),
    };
    let job = Job {
        turn,
        prompt: prompt_for(true),
        fresh_prompt: prompt_for(false),
        asked: asked.clone(),
        fingerprints,
        to_head,
        limit: Duration::from_secs(u64::from(harness.check_timeout_secs)),
        stop,
    };
    if let Some(round) = review.revalidation.as_mut() {
        round.commits = commits;
        round.state = RoundState::Waiting;
        round.proposals.clear();
        round.unreadable = 0;
        round.asked = u32::try_from(asked.len()).unwrap_or(u32::MAX);
        round.finished_at = None;
    }
    if let Err(out) = reviews::save(shared, &review) {
        shared.revalidations.end_run(pr, turn);
        return Err(out);
    }
    tell(shared, &review);
    note_log(shared, pr, turn, "waiting");
    tokio::spawn(drive(live, shared.clone(), pr.clone(), job));
    Ok(())
}

/// Runs one revalidation to its end, and settles it whatever happens inside.
async fn drive(_live: Live, shared: Arc<Shared>, pr: PrRef, job: Job) {
    let (turn, to_head) = (job.turn, job.to_head.clone());
    let task = tokio::spawn(execute(shared.clone(), pr.clone(), job));
    if task.await.is_err() {
        tracing::error!(pr = %pr, turn, "a revalidation failed inside the daemon");
        shared.revalidations.end_run(&pr, turn);
        settle(
            &shared,
            &pr,
            &to_head,
            Ending::Failed("The revalidation stopped unexpectedly".to_string()),
        )
        .await;
    }
}

async fn execute(shared: Arc<Shared>, pr: PrRef, job: Job) {
    // With nothing to ask, the round is done at once and no program starts.
    if job.asked.is_empty() {
        complete(&shared, &pr, &job, String::new()).await;
        return;
    }
    let mut sink = RunSink::new(shared.clone(), pr.clone(), job.turn);
    let run = TurnRun {
        pr: pr.clone(),
        turn: job.turn,
        prompt: job.prompt.clone(),
        fresh_prompt: Some(job.fresh_prompt.clone()),
        session: SessionSource::Fork,
        origin: TurnOrigin::Revalidation,
        places: crate::checks::places(&shared),
        limit: Some(job.limit),
        stop: job.stop.clone(),
    };
    match turns::run(&shared, run, &mut sink).await {
        TurnEnd::Done => {
            let text = sink.answer.unwrap_or(sink.streamed);
            complete(&shared, &pr, &job, text).await;
        }
        TurnEnd::Failed => {
            let message = sink
                .message
                .unwrap_or_else(|| "Claude Code could not finish the revalidation".to_string());
            finish(&shared, &pr, &job, Ending::Failed(message)).await;
        }
        TurnEnd::TimedOut => {
            let message = format!(
                "The revalidation ran past its limit of {}s and was stopped",
                job.limit.as_secs()
            );
            finish(&shared, &pr, &job, Ending::Failed(message)).await;
        }
        TurnEnd::Stopped(why) => {
            let ending = match why {
                // A publish that was refused stops the run too, so an end with the review still
                // there reads as a stop. When the review is gone `settle` finds nothing to mark.
                Why::User | Why::Ended => Ending::Failed(STOPPED.to_string()),
                // The round was started over or the daemon ended: nothing to say.
                Why::Superseded | Why::Shutdown => Ending::Dropped,
            };
            finish(&shared, &pr, &job, ending).await;
        }
    }
}

/// Reads what a run's turn says. The session id is ignored on purpose: a revalidation's session
/// never becomes the chat's.
struct RunSink {
    shared: Arc<Shared>,
    pr: PrRef,
    turn: u64,
    /// The text streamed so far, used when no result line carries the answer.
    streamed: String,
    answer: Option<String>,
    saw_final: bool,
    failed: bool,
    message: Option<String>,
}

impl RunSink {
    fn new(shared: Arc<Shared>, pr: PrRef, turn: u64) -> Self {
        Self {
            shared,
            pr,
            turn,
            streamed: String::new(),
            answer: None,
            saw_final: false,
            failed: false,
            message: None,
        }
    }

    fn fail_with(&mut self, message: String) {
        self.failed = true;
        self.message.get_or_insert(message);
    }

    fn doing(&self, summary: String) {
        let told = self.shared.revalidations.with(&self.pr, |entry| {
            let run = entry.run.as_mut().filter(|r| r.turn == self.turn)?;
            run.activity = Some(summary.clone());
            run.told.clone()
        });
        if let Some(mut round) = told {
            if let RoundState::Running { activity } = &mut round.state {
                *activity = Some(summary);
            }
            self.shared.publish(
                topics::REVIEWS,
                Event::RevalidationChanged {
                    pr: self.pr.clone(),
                    revalidation: Some(round),
                },
            );
        }
    }

    fn handle(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(text) => self.streamed.push_str(&text),
            AgentEvent::ToolUse(summary) => self.doing(summary),
            AgentEvent::Denied { .. } | AgentEvent::SessionId(_) => {}
            AgentEvent::Error { message, .. } => self.fail_with(message),
            AgentEvent::Final { text, is_error, .. } => {
                self.saw_final = true;
                if !is_error {
                    self.answer = Some(text);
                } else if text.trim().is_empty() {
                    self.fail_with("Claude Code could not finish the revalidation".to_string());
                } else {
                    self.fail_with(text.trim().to_string());
                }
            }
        }
    }
}

impl TurnSink for RunSink {
    fn waiting(&mut self) {}

    /// The round is `Running` from here on, unless something else ended it first.
    fn started(&mut self) {
        let (shared, pr, turn) = (self.shared.clone(), self.pr.clone(), self.turn);
        note_log(&shared, &pr, turn, "running");
        tokio::spawn(async move {
            let _guard = reviews::lock(&shared, &pr).await;
            let Ok(Some(mut review)) = reviews::load_stored(&shared, &pr) else {
                return;
            };
            let waiting = review
                .revalidation
                .as_ref()
                .is_some_and(|r| r.state == RoundState::Waiting);
            if !waiting {
                return;
            }
            if let Some(round) = review.revalidation.as_mut() {
                round.state = RoundState::Running { activity: None };
            }
            if reviews::save(&shared, &review).is_ok() {
                tell(&shared, &review);
            }
        });
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

/// How a run that did not produce proposals ends.
enum Ending {
    /// The round fails with this message.
    Failed(String),
    /// The round was started over or the review is gone: the stored round is left as it is.
    Dropped,
}

async fn finish(shared: &Arc<Shared>, pr: &PrRef, job: &Job, ending: Ending) {
    shared.revalidations.end_run(pr, job.turn);
    note_log(
        shared,
        pr,
        job.turn,
        match &ending {
            Ending::Failed(message) if message == STOPPED => "stopped",
            Ending::Failed(_) => "failed",
            Ending::Dropped => "stopped",
        },
    );
    settle(shared, pr, &job.to_head, ending).await;
}

/// Marks the round `Failed` when it is still the one that ran, and tells the user the old way
/// that the pull request moved on, unless the user stopped it, a window shows the review or the
/// round already notified.
async fn settle(shared: &Arc<Shared>, pr: &PrRef, to_head: &str, ending: Ending) {
    let Ending::Failed(message) = ending else {
        return;
    };
    let guard = reviews::lock(shared, pr).await;
    let Ok(Some(mut review)) = reviews::load_stored(shared, pr) else {
        return;
    };
    let ran = review.revalidation.as_ref().is_some_and(|r| {
        r.to_head == to_head && matches!(r.state, RoundState::Waiting | RoundState::Running { .. })
    });
    if !ran {
        return;
    }
    let notify = settle_failed_locked(shared, &mut review, &message);
    drop(guard);
    if notify {
        send_failure(shared, &review).await;
    }
}

/// Saves the round of `review` as `Failed` and tells the windows; the caller holds the review
/// lock and knows the round is the one that ended. Returns whether the old notification is due:
/// not after a Stop, not while a window holds the review, and once for a round.
fn settle_failed_locked(shared: &Shared, review: &mut Review, message: &str) -> bool {
    let held = shared.holds.is_held(&review.pr);
    let mut notify = false;
    if let Some(round) = review.revalidation.as_mut() {
        round.state = RoundState::Failed {
            message: message.to_string(),
        };
        round.finished_at = Some(now_unix());
        notify = message != STOPPED && !held && !round.notified;
        round.notified |= notify;
    }
    if reviews::save(shared, review).is_ok() {
        tell(shared, review);
    }
    notify
}

/// Ends the round of a review whose lock the caller holds, as `Failed`, and sends the old
/// notification when it is due.
async fn settle_failed(shared: &Arc<Shared>, review: &mut Review, message: &str) {
    if settle_failed_locked(shared, review, message) {
        send_failure(shared, review).await;
    }
}

async fn send_failure(shared: &Arc<Shared>, review: &Review) {
    if let Some(round) = &review.revalidation {
        notifications::review_outdated(shared, review, &round.from_head).await;
    }
}

/// Turns the answer of a run that ended well into proposals and stores them. The draft is not
/// touched.
async fn complete(shared: &Arc<Shared>, pr: &PrRef, job: &Job, text: String) {
    let asked_ids: Vec<String> = job.asked.iter().map(|a| a.id.clone()).collect();
    let (_prose, raw, mut unreadable) = extract_revalidation_blocks(&text, &asked_ids);
    let files = if !raw
        .iter()
        .any(|r| matches!(r.verdict, Verdict::Moved { .. }))
    {
        Some(Arc::new(Vec::new()))
    } else {
        files_at(shared, pr, &job.to_head).await
    };
    let Some(files) = files else {
        finish(
            shared,
            pr,
            job,
            Ending::Failed("Clúsia could not read the pull request's files".to_string()),
        )
        .await;
        return;
    };
    let guard = reviews::lock(shared, pr).await;
    let Ok(Some(mut review)) = reviews::load_stored(shared, pr) else {
        shared.revalidations.end_run(pr, job.turn);
        note_log(shared, pr, job.turn, "stopped");
        return;
    };
    let still_due = review.revalidation.as_ref().is_some_and(|r| {
        r.to_head == job.to_head
            && matches!(r.state, RoundState::Waiting | RoundState::Running { .. })
    });
    if !still_due {
        shared.revalidations.end_run(pr, job.turn);
        note_log(shared, pr, job.turn, "stopped");
        return;
    }
    let mut verdicts: HashMap<String, Verdict> = HashMap::new();
    for RawVerdict { item, verdict } in raw {
        let Some(current) = review.draft.get(&item) else {
            continue;
        };
        let verdict = match verdict {
            Verdict::Moved { mut anchor, reason } => {
                anchor.commit = match anchor.side {
                    Side::Right => job.to_head.clone(),
                    Side::Left => review.base_sha.clone(),
                };
                // A comment with no line has nowhere to move, and a line GitHub would refuse
                // must never reach the draft.
                if current.anchor.is_none() || !clusia_core::fits_diff(&files, &anchor) {
                    unreadable += 1;
                    continue;
                }
                Verdict::Moved { anchor, reason }
            }
            other => other,
        };
        verdicts.insert(item, verdict);
    }
    let proposals: Vec<Proposal> = job
        .asked
        .iter()
        .filter(|asked| review.draft.get(&asked.id).is_some())
        .map(|asked| Proposal {
            item: asked.id.clone(),
            before: asked.before.clone(),
            verdict: verdicts.remove(&asked.id).unwrap_or(Verdict::NotChecked),
            decision: Decision::Pending,
            fingerprint: job.fingerprints.get(&asked.id).cloned().unwrap_or_default(),
        })
        .collect();
    let now = now_unix();
    let held = shared.holds.is_held(pr);
    let mut notify = false;
    if let Some(round) = review.revalidation.as_mut() {
        round.proposals = proposals;
        round.unreadable = unreadable;
        round.state = RoundState::Done;
        round.finished_at = Some(now);
        // One notification for a round, however many times it runs.
        notify = !held && !round.notified;
        round.notified |= notify;
    }
    if review.state == ReviewState::Outdated {
        let _ = review.apply(ReviewEvent::Revalidated, now);
    }
    if let Err(out) = reviews::save(shared, &review) {
        tracing::warn!(pr = %pr, ?out, "cannot store the revalidation");
        shared.revalidations.end_run(pr, job.turn);
        return;
    }
    shared.revalidations.end_run(pr, job.turn);
    let counts = review
        .revalidation
        .as_ref()
        .map(|round| Counts::of(round, &review.draft))
        .unwrap_or_default();
    reviews::record(
        shared,
        ActivityKind::ReviewRevalidated,
        pr,
        "",
        None,
        Some(counts.text()),
    );
    note_log(shared, pr, job.turn, "done");
    tell(shared, &review);
    reviews::announce(shared, &review);
    drop(guard);
    if notify {
        notifications::review_revalidated(shared, &review, &counts.text()).await;
    }
}

/// `Revalidate`: starts the round now, or leaves the one that waits or runs.
pub(crate) async fn run_now(shared: &Arc<Shared>, pr: &PrRef) -> clusia_protocol::Outcome {
    match start(shared, pr, Trigger::Now).await {
        Ok(()) => clusia_protocol::Outcome::Ok(clusia_protocol::Reply::Ack),
        Err(out) => out,
    }
}

/// `StopRevalidation`: the run ends, or a round that waits for its quiet period is cancelled;
/// either way the round reads `Failed { "Stopped" }`, which offers Try again.
pub(crate) async fn stop(shared: &Arc<Shared>, pr: &PrRef) {
    let ran = shared.revalidations.with(pr, |entry| {
        entry.cancel_timer();
        match &entry.run {
            Some(run) => {
                run.stop.fire(Why::User);
                true
            }
            None => false,
        }
    });
    if ran {
        return;
    }
    let _guard = reviews::lock(shared, pr).await;
    let Ok(Some(mut review)) = reviews::load_stored(shared, pr) else {
        return;
    };
    let waiting = review.revalidation.as_ref().is_some_and(|r| {
        matches!(
            r.state,
            RoundState::Scheduled { .. } | RoundState::Waiting | RoundState::Running { .. }
        )
    });
    if waiting {
        settle_failed_locked(shared, &mut review, STOPPED);
    }
}

/// A publish or a discard is about to take the review lock: the run is stopped, for `why`, and
/// the call waits until it is gone. The timer stays: the publish may still be refused, and
/// `forget` cancels it once the review is really gone.
pub(crate) async fn stop_all(shared: &Shared, pr: &PrRef, why: Why) {
    shared.revalidations.with(pr, |entry| {
        if let Some(run) = &entry.run {
            run.stop.fire(why);
        }
    });
    let give_up = Instant::now() + STOP_WAIT;
    while shared.revalidations.with(pr, |entry| entry.run.is_some()) && Instant::now() < give_up {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The review was published or discarded: what ran or waited is stopped and the windows hear that
/// no round is left. The caller may hold the review lock.
pub(crate) fn forget(shared: &Shared, pr: &PrRef) {
    shared.revalidations.with(pr, |entry| {
        entry.cancel_timer();
        if let Some(run) = &entry.run {
            run.stop.fire(Why::Ended);
        }
    });
    shared.publish(
        topics::REVIEWS,
        Event::RevalidationChanged {
            pr: pr.clone(),
            revalidation: None,
        },
    );
}

/// Daemon shutdown: no new timer or run, every run stopped, and a wait until none is left. A
/// round that was running stays `Running` in its file; the next start runs it again.
pub(crate) async fn shutdown(shared: &Shared) {
    let revalidations = &shared.revalidations;
    revalidations.closing.store(true, Ordering::SeqCst);
    {
        let mut table = revalidations
            .table
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for entry in table.values_mut() {
            entry.cancel_timer();
            if let Some(run) = &entry.run {
                run.stop.fire(Why::Shutdown);
            }
        }
    }
    let mut live = revalidations.live.subscribe();
    let _ = tokio::time::timeout(SHUTDOWN_WAIT, live.wait_for(|n| *n == 0)).await;
}
#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::{Config, DraftItem, ItemStatus, Origin, Paths};

    fn anchor(line: u32) -> Anchor {
        Anchor {
            path: "src/a.rs".into(),
            line,
            start_line: None,
            side: Side::Right,
            commit: "c1".into(),
        }
    }

    fn item(id: &str, kind: DraftKind, accepted: bool, anchor: Option<Anchor>) -> DraftItem {
        DraftItem {
            id: id.into(),
            kind,
            origin: Origin::Human,
            anchor,
            body: format!("body of {id}"),
            status: ItemStatus::Ok,
            accepted,
            created_at: 0,
            thread: None,
        }
    }

    fn review(items: Vec<DraftItem>) -> Review {
        let mut review = Review::new(
            "acme/widgets#7".parse().unwrap(),
            "Add feature".into(),
            "b1".into(),
            "h1".into(),
            0,
        );
        review.draft.items = items;
        review
    }

    #[test]
    fn counts_read_as_the_notification_says_them() {
        let text = |moved, obsolete, rewording| {
            Counts {
                moved,
                obsolete,
                rewording,
            }
            .text()
        };
        assert_eq!(text(1, 1, 1), "1 moved, 1 obsolete, 1 new wording");
        assert_eq!(text(0, 2, 0), "2 obsolete");
        assert_eq!(text(0, 0, 3), "3 new wordings");
        assert_eq!(text(0, 0, 0), "no changes suggested");
    }

    #[test]
    fn only_comments_the_user_will_publish_are_asked() {
        let review = review(vec![
            item("a", DraftKind::LineComment, true, Some(anchor(4))),
            item("b", DraftKind::General, true, None),
            item("c", DraftKind::Reply, true, None),
            item("d", DraftKind::Resolve, true, None),
            item("e", DraftKind::LineComment, false, Some(anchor(9))),
        ]);
        let round = {
            let mut r = review.clone();
            open_round(
                &mut r,
                snapshot(&review),
                "h0",
                "h1",
                Relocation::default(),
                0,
                300,
            );
            r.revalidation.unwrap()
        };
        let ids: Vec<String> = asked_items(&review, &round)
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn an_item_added_after_the_round_opened_keeps_its_own_anchor_as_before() {
        let mut review = review(vec![item(
            "a",
            DraftKind::LineComment,
            true,
            Some(anchor(4)),
        )]);
        let before = snapshot(&review);
        open_round(
            &mut review,
            before,
            "h0",
            "h1",
            Relocation::default(),
            0,
            300,
        );
        review
            .draft
            .items
            .push(item("n", DraftKind::LineComment, true, Some(anchor(8))));
        let round = review.revalidation.clone().unwrap();
        let asked = asked_items(&review, &round);
        assert_eq!(asked[1].before, Some(anchor(8)));
    }

    #[test]
    fn the_snapshot_keeps_every_anchor() {
        let review = review(vec![
            item("a", DraftKind::LineComment, true, Some(anchor(4))),
            item("b", DraftKind::General, true, None),
        ]);
        let snap = snapshot(&review);
        assert_eq!(snap["a"], Some(anchor(4)));
        assert_eq!(snap["b"], None);
    }

    fn lab() -> (tempfile::TempDir, Arc<Shared>) {
        let home = tempfile::tempdir().unwrap();
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
        let shared = Arc::new(Shared::new(
            Paths::new(home.path()),
            Config::default(),
            options,
        ));
        (home, shared)
    }

    fn has_timer(shared: &Shared, pr: &PrRef) -> bool {
        shared
            .revalidations
            .with(pr, |entry| entry.generation != 0 && entry.timer.is_some())
    }

    #[tokio::test]
    async fn stopping_before_a_publish_keeps_the_timer_and_forgetting_cancels_it() {
        let (_home, shared) = lab();
        let pr: PrRef = "acme/widgets#7".parse().unwrap();
        schedule(&shared, &pr, now_unix() + 3600);
        assert!(has_timer(&shared, &pr));

        stop_all(&shared, &pr, Why::Ended).await;
        assert!(
            has_timer(&shared, &pr),
            "a publish that is refused leaves the round its timer"
        );

        forget(&shared, &pr);
        assert!(
            !has_timer(&shared, &pr),
            "a publish that went through ends it"
        );
    }

    #[tokio::test]
    async fn a_second_schedule_replaces_the_timer_of_the_first() {
        let (_home, shared) = lab();
        let pr: PrRef = "acme/widgets#7".parse().unwrap();
        schedule(&shared, &pr, now_unix() + 3600);
        let first = shared.revalidations.with(&pr, |entry| entry.generation);
        schedule(&shared, &pr, now_unix() + 7200);
        let second = shared.revalidations.with(&pr, |entry| entry.generation);
        assert_ne!(first, second);
        assert!(has_timer(&shared, &pr));
        shutdown(&shared).await;
        assert!(!has_timer(&shared, &pr), "shutdown leaves no timer");
    }
}
