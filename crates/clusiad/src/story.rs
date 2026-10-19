//! The story of a pull request: one turn, forked from the review's session like a check and
//! sharing the places of the checks, that orders the change into steps and writes their words.
//! The version is on disk from the moment the run is accepted, so a stop, a crash or a restart
//! leaves one that reads as not written. Read marks are kept per hunk, so they hold across the
//! static story, the written one and later commits.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clusia_calls::{StaticStep, Target, static_order};
use clusia_core::diagram::{DiagramContext, EntryState, History, repair_prompt};
use clusia_core::hunks::HunkRef;
use clusia_core::printable::printable;
use clusia_core::story::{
    ReadHunk, ReadMarks, StaticStepInfo, StoryContext, StorySpec, StoryUseCase, StoryVersion,
    StoryVersionState, Validated, step_read, story_prompt, validate,
};
use clusia_core::time::civil_from_days;
use clusia_core::{DiffMap, DraftKind, LineMap, Origin, PrRef, ReviewState};
use clusia_git::{diff_between, git, is_object_id, pin_commit, repo_of_worktree, story_ref};
use clusia_harness::{AgentEvent, extract_story};
use clusia_protocol::{
    AgentErrorKind, AgentLogEntry, ErrorCode, Event, Outcome, ProtocolError, Reply, StoryState,
    StoryVersionInfo, TurnOrigin, topics,
};
use clusia_store::diagrams::load_latest_diagram_version;
use clusia_store::load_review_cache;
use clusia_store::stories::{
    delete_stories, load_read_marks, load_story_version, load_story_versions, next_story_number,
    save_read_marks, save_story_version,
};
use tokio::sync::watch;

use crate::agent_log;
use crate::calls::{self, Built};
use crate::reviews;
use crate::state::Shared;
use crate::sync::now_unix;
use crate::turns::{self, Boxed, SessionSource, Stop, TurnEnd, TurnRun, TurnSink, Why};

/// The `number` of the static story in `MarkStepRead` and `AddStepComment`.
pub(crate) const STATIC: u32 = 0;
/// How long `stop_all` waits for the run of a review to end.
const STOP_WAIT: Duration = Duration::from_secs(4);
/// How long a shutdown waits for the runs to end.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
/// How long a run waits for the index of its commit.
const INDEX_WAIT: Duration = Duration::from_secs(30);
/// The longest reason a failed version shows.
const REASON_MAX: usize = 200;
const ID_MAX: usize = 128;
/// The commit subjects a prompt holds.
const MAX_COMMITS: &str = "50";
/// Other commits whose marks are mapped onto the shown one.
const MAX_MAPPED_COMMITS: usize = 8;
const BUSY: &str = "The story is already being written";
const EXISTS: &str = "A story already exists; use Rewrite";
const NO_HARNESS: &str = "No harness is set up";
const NO_NUMBER_LEFT: &str = "There is no version number left for this story";
const UNREADABLE: &str = "The changes of the pull request could not be read";
const NOT_READY: &str = "The call graph is not ready yet";
const READ_ONLY: &str = "Older versions are read-only";

/// A run that waits for a place or is running.
struct Run {
    id: u64,
    stop: Arc<Stop>,
    state: StoryState,
    /// The version this run writes.
    number: u32,
}

/// The running story turns of every review.
pub(crate) struct Stories {
    table: Mutex<HashMap<PrRef, Run>>,
    /// How many runs live; shutdown waits for zero.
    live: watch::Sender<usize>,
    closing: AtomicBool,
    runs: AtomicU64,
}

impl Default for Stories {
    fn default() -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
            live: watch::channel(0).0,
            closing: AtomicBool::new(false),
            runs: AtomicU64::new(0),
        }
    }
}

impl Stories {
    fn with<R>(&self, pr: &PrRef, read: impl FnOnce(Option<&mut Run>) -> R) -> R {
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        read(table.get_mut(pr))
    }

    pub(crate) fn running(&self, pr: &PrRef) -> bool {
        self.with(pr, |run| run.is_some())
    }

    /// What the run of `pr` does; `None` when there is none.
    pub(crate) fn live_state(&self, pr: &PrRef) -> Option<StoryState> {
        self.with(pr, |run| run.map(|r| r.state.clone()))
    }

    /// The version the run of `pr` writes.
    fn writing(&self, pr: &PrRef) -> Option<u32> {
        self.with(pr, |run| run.map(|r| r.number))
    }

    /// `false` when `run` is not the registered one any more.
    fn set_state(&self, pr: &PrRef, run: u64, state: StoryState) -> bool {
        self.with(pr, |r| match r.filter(|r| r.id == run) {
            Some(r) => {
                r.state = state;
                true
            }
            None => false,
        })
    }

    /// Ends run `run`; `false` when that run is not the registered one any more.
    fn end_run(&self, pr: &PrRef, run: u64) -> bool {
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        if table.get(pr).is_some_and(|r| r.id == run) {
            table.remove(pr);
            true
        } else {
            false
        }
    }
}

/// Counts a run in `Stories::live` while it lives.
struct Live(Arc<Shared>);

impl Live {
    fn new(shared: &Arc<Shared>) -> Self {
        shared.stories.live.send_modify(|n| *n += 1);
        Self(shared.clone())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.stories.live.send_modify(|n| *n -= 1);
    }
}

/// What a run needs once it has been accepted.
struct Job {
    run: u64,
    number: u32,
    /// The commit the run reads, and the version is stored for.
    head: String,
    started_at: String,
    pr_title: String,
    pr_body: String,
    /// The display name of the harness, kept in the version.
    name: String,
    limit: Duration,
    stop: Arc<Stop>,
}

impl Job {
    fn version(&self, state: StoryVersionState) -> StoryVersion {
        StoryVersion {
            number: self.number,
            commit: self.head.clone(),
            harness: self.name.clone(),
            started_at: self.started_at.clone(),
            state,
            spec: None,
            notes: Vec::new(),
        }
    }
}

/// Why a turn gave no answer.
enum Halt {
    /// The run was stopped, for this reason.
    Over(Why),
    /// The turn or its answer failed; the message is shown.
    Failed(String),
}

/// `unix` seconds as `YYYY-MM-DDTHH:MM:SSZ`.
fn rfc3339(unix: i64) -> String {
    let (day, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(day);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}

fn tell(shared: &Shared, pr: &PrRef, state: StoryState) {
    shared.publish(
        topics::AGENT,
        Event::StoryChanged {
            pr: pr.clone(),
            state,
        },
    );
}

fn note_log(shared: &Shared, pr: &PrRef, turn: u64, state: &str) {
    let entry = AgentLogEntry::Story {
        at: now_unix(),
        turn,
        state: state.to_string(),
    };
    if let Err(e) = agent_log::append(&shared.paths.agent_log(pr), &entry) {
        tracing::warn!(error = %e, pr = %pr, "cannot append to the agent log");
    }
}

/// A stored version as it is shown: the one a run writes as it is on disk, any other with what a
/// crash or a kill left writing read as not written.
fn as_shown(shared: &Shared, pr: &PrRef, version: StoryVersion) -> StoryVersion {
    if shared.stories.writing(pr) == Some(version.number) {
        version
    } else {
        version.recovered()
    }
}

/// The stored versions of `pr`, oldest first, as they are shown.
fn versions_of(shared: &Shared, pr: &PrRef) -> Vec<StoryVersion> {
    load_story_versions(&shared.paths, pr)
        .into_iter()
        .map(|version| as_shown(shared, pr, version))
        .collect()
}

/// `text` cut to `max` characters.
fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The first of `errors`, shown on one line.
fn reason_of(errors: &[String]) -> String {
    let first = errors
        .first()
        .map_or("The answer was not usable", String::as_str);
    cut(&printable(first), REASON_MAX)
}

fn not_found(message: String) -> Outcome {
    Outcome::Err(ProtocolError::new(ErrorCode::NotFound, message))
}

/// Writes `version` unless the review ended while the run went on. Under the review lock, so a
/// publish or a discard that deleted the stories is never undone by a late write.
async fn keep(shared: &Shared, pr: &PrRef, job: &Job, version: &StoryVersion) -> bool {
    let _guard = reviews::lock(shared, pr).await;
    let alive = !(job.stop.fired() && job.stop.why() == Why::Ended)
        && matches!(reviews::load_stored(shared, pr), Ok(Some(_)));
    if alive && let Err(e) = save_story_version(&shared.paths, pr, version) {
        tracing::warn!(error = %e, pr = %pr, "cannot store the story");
    }
    alive
}

/// Reads what a turn says. The session id is ignored on purpose: a story's session never
/// becomes the chat's.
struct StorySink {
    shared: Arc<Shared>,
    pr: PrRef,
    run: u64,
    turn: u64,
    name: String,
    /// The text streamed so far, used when no result line carries the answer.
    streamed: String,
    answer: Option<String>,
    saw_final: bool,
    failed: bool,
    message: Option<String>,
}

impl StorySink {
    fn fail_with(&mut self, message: String) {
        self.failed = true;
        self.message.get_or_insert(message);
    }

    fn handle(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(text) => self.streamed.push_str(&text),
            AgentEvent::ToolUse(_) | AgentEvent::Denied { .. } | AgentEvent::SessionId(_) => {}
            AgentEvent::Error { message, .. } => self.fail_with(message),
            AgentEvent::Final { text, is_error, .. } => {
                self.saw_final = true;
                if !is_error {
                    self.answer = Some(text);
                } else if text.trim().is_empty() {
                    self.fail_with(format!("{} could not finish the story", self.name));
                } else {
                    self.fail_with(text.trim().to_string());
                }
            }
        }
    }
}

impl TurnSink for StorySink {
    fn waiting(&mut self) {
        if self
            .shared
            .stories
            .set_state(&self.pr, self.run, StoryState::Waiting)
        {
            tell(&self.shared, &self.pr, StoryState::Waiting);
        }
    }

    fn started(&mut self) {
        note_log(&self.shared, &self.pr, self.turn, "running");
        if self
            .shared
            .stories
            .set_state(&self.pr, self.run, StoryState::Writing)
        {
            tell(&self.shared, &self.pr, StoryState::Writing);
        }
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

/// Runs one turn, forked from the review's session (or fresh when it has none), and gives its
/// answer. `prompts` is the prompt for a fork and the one for a session of its own.
async fn ask(
    shared: &Arc<Shared>,
    pr: &PrRef,
    job: &Job,
    prompts: (String, String),
) -> Result<String, Halt> {
    if job.stop.fired() {
        return Err(Halt::Over(job.stop.why()));
    }
    let turn = shared.sessions.next_turn(shared, pr).await;
    note_log(shared, pr, turn, "waiting");
    let mut sink = StorySink {
        shared: shared.clone(),
        pr: pr.clone(),
        run: job.run,
        turn,
        name: job.name.clone(),
        streamed: String::new(),
        answer: None,
        saw_final: false,
        failed: false,
        message: None,
    };
    let run = TurnRun {
        pr: pr.clone(),
        turn,
        prompt: prompts.0,
        fresh_prompt: Some(prompts.1),
        session: SessionSource::Fork,
        origin: TurnOrigin::Story,
        places: crate::checks::places(shared),
        limit: Some(job.limit),
        stop: job.stop.clone(),
    };
    match turns::run(shared, run, &mut sink).await {
        TurnEnd::Done => {
            note_log(shared, pr, turn, "done");
            Ok(sink.answer.unwrap_or(sink.streamed))
        }
        TurnEnd::Failed => {
            note_log(shared, pr, turn, "failed");
            let message = sink
                .message
                .unwrap_or_else(|| format!("{} could not finish the story", job.name));
            Err(Halt::Failed(reason_of(&[message])))
        }
        TurnEnd::TimedOut => {
            note_log(shared, pr, turn, "failed");
            Err(Halt::Failed(format!(
                "The turn ran past its limit of {}s and was stopped",
                job.limit.as_secs()
            )))
        }
        TurnEnd::Stopped(why) => {
            note_log(shared, pr, turn, "stopped");
            Err(Halt::Over(why))
        }
    }
}

/// Asks, and reads the answer with `parse`. An answer that does not parse gets one more turn:
/// a fresh fork whose prompt holds that answer and what was wrong with it.
async fn ask_checked(
    shared: &Arc<Shared>,
    pr: &PrRef,
    job: &Job,
    ctx: &DiagramContext,
    base: &(dyn Fn(bool) -> String + Sync),
    parse: impl Fn(&str) -> Result<Validated, Vec<String>>,
) -> Result<Validated, Halt> {
    let first = ask(shared, pr, job, (base(true), base(false))).await?;
    let errors = match parse(&first) {
        Ok(value) => return Ok(value),
        Err(errors) => errors,
    };
    let again = |forked: bool| {
        let ctx = DiagramContext {
            forked,
            ..ctx.clone()
        };
        repair_prompt(&ctx, &base(forked), &first, &errors)
    };
    let second = ask(shared, pr, job, (again(true), again(false))).await?;
    parse(&second).map_err(|errors| Halt::Failed(reason_of(&errors)))
}

/// `work`, unless the run is stopped first; a stop drops `work`, and with it any child process.
async fn unless_stopped<T>(stop: &Stop, work: impl Future<Output = T>) -> Result<T, Why> {
    if stop.fired() {
        return Err(stop.why());
    }
    tokio::select! {
        out = work => Ok(out),
        () = stop.notified() => Err(stop.why()),
    }
}

/// The static steps of the built index, in their order.
async fn static_steps(built: Arc<Built>) -> Vec<StaticStep> {
    tokio::task::spawn_blocking(move || {
        static_order(&built.index, &built.resolved, &built.changed, &built.hunks)
    })
    .await
    .unwrap_or_default()
}

/// The steps for the prompt, each with how many calls reach its function.
fn step_infos(built: &Built, steps: &[StaticStep]) -> Vec<StaticStepInfo> {
    steps
        .iter()
        .map(|step| {
            let ids: BTreeSet<&str> = built
                .index
                .functions
                .iter()
                .filter(|f| f.qualified == step.id)
                .map(|f| f.id.0.as_str())
                .collect();
            let callers = built
                .resolved
                .edges
                .iter()
                .filter(|(_, target, _)| {
                    matches!(target, Target::Local { ids: to } if to.iter().any(|t| ids.contains(t.0.as_str())))
                })
                .count();
            StaticStepInfo {
                id: step.id.clone(),
                title: step.title.clone(),
                callers: u32::try_from(callers).unwrap_or(u32::MAX),
                hunks: step.hunks.clone(),
            }
        })
        .collect()
}

/// The use cases of the latest keyword map, without the removed ones.
fn map_use_cases(shared: &Shared, pr: &PrRef) -> Vec<StoryUseCase> {
    let Some(version) = load_latest_diagram_version(&shared.paths, pr) else {
        return Vec::new();
    };
    version
        .entries
        .iter()
        .find_map(|entry| match &entry.state {
            EntryState::ReadyMap { map, .. } => Some(map),
            _ => None,
        })
        .map(|map| {
            map.plan
                .use_cases
                .iter()
                .filter(|u| u.history != Some(History::Removed))
                .map(|u| StoryUseCase {
                    id: u.id.clone(),
                    name: u.name.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The subjects of the commits from `base` to `head`, as git lists them; empty when they cannot
/// be listed.
async fn commit_subjects(shared: &Shared, pr: &PrRef, base: &str, head: &str) -> Vec<String> {
    if !is_object_id(base) || !is_object_id(head) {
        return Vec::new();
    }
    let Ok(repo) = repo_of_worktree(&shared.paths.worktree_for(pr)).await else {
        return Vec::new();
    };
    let range = format!("{base}..{head}");
    git(
        &repo,
        &["log", "--format=%s", "-n", MAX_COMMITS, range.as_str()],
    )
    .await
    .map(|out| {
        out.lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

enum End {
    Written(Validated),
    Failed(String),
    Over(Why),
}

/// The index, the prompt and the turn.
async fn compose(shared: &Arc<Shared>, pr: &PrRef, job: &Job) -> End {
    let ready = calls::wait_ready(shared, pr, &job.head, INDEX_WAIT);
    let built = match unless_stopped(&job.stop, ready).await {
        Err(why) => return End::Over(why),
        Ok(None) => return End::Failed(UNREADABLE.to_string()),
        Ok(Some(built)) => built,
    };
    let steps = step_infos(&built, &static_steps(built.clone()).await);
    let previous: Option<StorySpec> = versions_of(shared, pr)
        .into_iter()
        .rev()
        .filter(|v| v.number < job.number && v.state == StoryVersionState::Written)
        .find_map(|v| v.spec);
    let use_cases = map_use_cases(shared, pr);
    let commits = commit_subjects(shared, pr, &built.base, &job.head).await;
    let files: Vec<String> = built
        .hunks
        .iter()
        .filter_map(|f| f.new_path.clone().or_else(|| f.old_path.clone()))
        .collect();
    let base = |forked: bool| {
        story_prompt(&StoryContext {
            pr_title: &job.pr_title,
            pr_body: &job.pr_body,
            commits: &commits,
            static_steps: &steps,
            use_cases: &use_cases,
            previous: previous.as_ref(),
            forked,
        })
    };
    let ctx = DiagramContext {
        pr_title: job.pr_title.clone(),
        files,
        forked: true,
    };
    let parsed = ask_checked(shared, pr, job, &ctx, &base, |text| {
        extract_story(text)
            .map_err(|e| vec![e])
            .and_then(|raw| validate(&raw, &built.hunks, previous.as_ref()))
    })
    .await;
    match parsed {
        Ok(validated) => End::Written(validated),
        Err(Halt::Failed(reason)) => End::Failed(reason),
        Err(Halt::Over(why)) => End::Over(why),
    }
}

/// Runs a job to its end, and settles it whatever happens inside.
async fn drive(_live: Live, shared: Arc<Shared>, pr: PrRef, job: Arc<Job>) {
    let task = tokio::spawn(execute(shared.clone(), pr.clone(), job.clone()));
    if task.await.is_err() {
        tracing::error!(pr = %pr, run = job.run, "a story failed inside the daemon");
        let message = "The story stopped unexpectedly".to_string();
        let failed = job.version(StoryVersionState::Failed {
            message: message.clone(),
        });
        keep(&shared, &pr, &job, &failed).await;
        if shared.stories.end_run(&pr, job.run) {
            tell(&shared, &pr, StoryState::Failed { message });
        }
    }
    shared.sessions.release_closed(&shared, &pr).await;
}

/// Writes the end of the run into the version. A run that was replaced or forgotten while it
/// wound down tells nothing: the state now belongs to whatever came after it.
async fn execute(shared: Arc<Shared>, pr: PrRef, job: Arc<Job>) {
    let end = compose(&shared, &pr, &job).await;
    let mut version = job.version(StoryVersionState::NotWritten);
    let (told, why) = match end {
        End::Written(Validated { spec, notes }) => {
            version.state = StoryVersionState::Written;
            version.spec = Some(spec);
            version.notes = notes;
            (StoryState::Idle, None)
        }
        End::Failed(message) => {
            version.state = StoryVersionState::Failed {
                message: message.clone(),
            };
            (StoryState::Failed { message }, None)
        }
        End::Over(why) => (StoryState::Idle, Some(why)),
    };
    keep(&shared, &pr, &job, &version).await;
    if shared.stories.end_run(&pr, job.run) && why != Some(Why::Shutdown) {
        tell(&shared, &pr, told);
    }
}

/// Keeps the commit of the job's version reachable (best effort).
async fn pin(shared: &Shared, pr: &PrRef, job: &Job) {
    if !is_object_id(&job.head) {
        return;
    }
    let pinned = match repo_of_worktree(&shared.paths.worktree_for(pr)).await {
        Ok(repo) => pin_commit(&repo, &story_ref(pr.number, job.number), &job.head)
            .await
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    if let Err(e) = pinned {
        tracing::warn!(error = %e, pr = %pr, "cannot pin the commit of the story");
    }
}

/// Removes every story pin of `pr` (best effort).
async fn unpin(shared: &Shared, pr: &PrRef) {
    if let Ok(repo) = repo_of_worktree(&shared.paths.worktree_for(pr)).await
        && let Err(e) = clusia_git::unpin_stories(&repo, pr.number).await
    {
        tracing::warn!(error = %e, "cannot unpin the commits of the stories");
    }
}

enum Refused {
    Busy,
    Closing,
}

/// Accepts a run, stores its version as writing, pins its commit and starts it.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
async fn start(shared: &Arc<Shared>, pr: &PrRef, rewrite: bool) -> Result<(), Outcome> {
    if shared.stories.closing.load(Ordering::SeqCst) {
        return Err(reviews::invalid_state("clusiad is stopping"));
    }
    let review = match reviews::load_stored(shared, pr) {
        Ok(Some(review))
            if review.state == ReviewState::Active && shared.paths.worktree_for(pr).is_dir() =>
        {
            review
        }
        Ok(_) => {
            return Err(reviews::invalid_state(format!(
                "no review for {pr}; open it first"
            )));
        }
        Err(out) => return Err(out),
    };
    if shared.stories.running(pr) {
        return Err(Outcome::Err(ProtocolError::new(ErrorCode::Busy, BUSY)));
    }
    let stored = versions_of(shared, pr);
    if !rewrite && !stored.is_empty() {
        return Err(reviews::invalid_state(EXISTS));
    }
    let harness = turns::harness_for(shared, pr).await;
    if !turns::program_path(shared, &harness).is_file() {
        return Err(Outcome::Err(ProtocolError::new(
            ErrorCode::NotConfigured,
            NO_HARNESS,
        )));
    }
    let reusable = stored.last().filter(|v| {
        v.commit == review.head_sha
            && matches!(
                v.state,
                StoryVersionState::Failed { .. } | StoryVersionState::NotWritten
            )
    });
    let number = match reusable {
        Some(version) => version.number,
        None => next_story_number(&shared.paths, pr)
            .ok_or_else(|| reviews::invalid_state(NO_NUMBER_LEFT))?,
    };
    let limit = Duration::from_secs(u64::from(harness.check_timeout_secs));
    let (pr_title, pr_body) = match load_review_cache(&shared.paths, pr) {
        Ok(Some(cache)) => (cache.pr.summary.title, cache.pr.body),
        _ => (review.title.clone(), String::new()),
    };
    let stop = Stop::new();
    let run = shared.stories.runs.fetch_add(1, Ordering::SeqCst) + 1;
    let placed = {
        let mut table = shared
            .stories
            .table
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if shared.stories.closing.load(Ordering::SeqCst) {
            Err(Refused::Closing)
        } else if table.contains_key(pr) {
            Err(Refused::Busy)
        } else {
            table.insert(
                pr.clone(),
                Run {
                    id: run,
                    stop: stop.clone(),
                    state: StoryState::Writing,
                    number,
                },
            );
            Ok(Live::new(shared))
        }
    };
    let live = match placed {
        Ok(live) => live,
        Err(Refused::Busy) => {
            return Err(Outcome::Err(ProtocolError::new(ErrorCode::Busy, BUSY)));
        }
        Err(Refused::Closing) => return Err(reviews::invalid_state("clusiad is stopping")),
    };
    let job = Arc::new(Job {
        run,
        number,
        head: review.head_sha.clone(),
        started_at: rfc3339(now_unix()),
        pr_title,
        pr_body,
        name: crate::sessions::name_for(shared, pr).await,
        limit,
        stop,
    });
    // Pinned before the first write: `forget` and `cleanup_checkout` unpin every ref of the
    // review, so a pin made after them would be left behind, and one made before is removed.
    pin(shared, pr, &job).await;
    if !keep(shared, pr, &job, &job.version(StoryVersionState::Writing)).await {
        shared.stories.end_run(pr, run);
        unpin(shared, pr).await;
        return Err(reviews::invalid_state(format!(
            "no review for {pr}; open it first"
        )));
    }
    tell(shared, pr, StoryState::Writing);
    tokio::spawn(drive(live, shared.clone(), pr.clone(), job));
    Ok(())
}

/// `WriteStory`: writes the first story of the review.
pub(crate) async fn write(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    match start(shared, pr, false).await {
        Ok(()) => Outcome::Ok(Reply::Ack),
        Err(out) => out,
    }
}

/// `RewriteStory`: writes a new version, or again the one that did not come out.
pub(crate) async fn rewrite(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    match start(shared, pr, true).await {
        Ok(()) => Outcome::Ok(Reply::Ack),
        Err(out) => out,
    }
}

/// `StopStory`: stops the run, if one waits or runs.
pub(crate) fn stop(shared: &Shared, pr: &PrRef) -> Outcome {
    shared.stories.with(pr, |run| {
        if let Some(run) = run {
            run.stop.fire(Why::User);
        }
    });
    Outcome::Ok(Reply::Ack)
}

/// The review ended or was left (closed, published, discarded): its run is stopped, for `why`.
/// The stored versions stay (`forget` deletes them). Waits until the run is gone, so the caller
/// may take the review lock after.
pub(crate) async fn stop_all(shared: &Shared, pr: &PrRef, why: Why) {
    shared.stories.with(pr, |run| {
        if let Some(run) = run {
            run.stop.fire(why);
        }
    });
    let give_up = Instant::now() + STOP_WAIT;
    while shared.stories.running(pr) && Instant::now() < give_up {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Daemon shutdown: no new run, every run stopped, and a wait until none is left.
pub(crate) async fn shutdown(shared: &Shared) {
    let stories = &shared.stories;
    stories.closing.store(true, Ordering::SeqCst);
    {
        let table = stories.table.lock().unwrap_or_else(|p| p.into_inner());
        for run in table.values() {
            run.stop.fire(Why::Shutdown);
        }
    }
    let mut live = stories.live.subscribe();
    let _ = tokio::time::timeout(SHUTDOWN_WAIT, live.wait_for(|n| *n == 0)).await;
}

/// The review was published or discarded: the run is stopped, the versions are deleted and
/// unpinned, and the windows hear that nothing is written. The caller may hold the review lock.
pub(crate) fn forget(shared: &Shared, pr: &PrRef) {
    let removed = shared
        .stories
        .table
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(pr);
    if let Some(run) = removed {
        run.stop.fire(Why::Ended);
    }
    if let Err(e) = delete_stories(&shared.paths, pr) {
        tracing::warn!(error = %e, pr = %pr, "cannot delete the stored stories");
    }
    let (worktree, number) = (shared.paths.worktree_for(pr), pr.number);
    tokio::spawn(async move {
        if let Ok(repo) = repo_of_worktree(&worktree).await
            && let Err(e) = clusia_git::unpin_stories(&repo, number).await
        {
            tracing::warn!(error = %e, "cannot unpin the commits of the stories");
        }
    });
    tell(shared, pr, StoryState::Idle);
}

/// Maps the marks of other commits onto `to`: one `DiffMap` per commit, for at most
/// `MAX_MAPPED_COMMITS` commits, the most recently marked first.
async fn mappings(
    shared: &Shared,
    pr: &PrRef,
    marks: &ReadMarks,
    to: &str,
) -> HashMap<String, DiffMap> {
    let mut others: Vec<&str> = Vec::new();
    for mark in marks.hunks.iter().rev() {
        if mark.commit != to && !others.contains(&mark.commit.as_str()) {
            others.push(&mark.commit);
        }
    }
    others.truncate(MAX_MAPPED_COMMITS);
    let mut maps = HashMap::new();
    if others.is_empty() || !is_object_id(to) {
        return maps;
    }
    let Ok(repo) = repo_of_worktree(&shared.paths.worktree_for(pr)).await else {
        return maps;
    };
    for commit in others {
        if is_object_id(commit)
            && let Ok(diff) = diff_between(&repo, commit, to).await
        {
            maps.insert(commit.to_string(), DiffMap::parse(&diff));
        }
    }
    maps
}

/// Where `mark` lies at the commit `maps` were made for: both ends `Same` or `Moved` into one
/// file.
fn mapped(maps: &HashMap<String, DiffMap>, mark: &ReadHunk) -> Option<HunkRef> {
    let map = maps.get(&mark.commit)?;
    let at = |line: u32| match map.map_line(&mark.path, line) {
        LineMap::Same => Some((mark.path.clone(), line)),
        LineMap::Moved { path, line } => Some((path, line)),
        LineMap::Changed | LineMap::Deleted => None,
    };
    let ((path, start), (end_path, end)) = (at(mark.start_line)?, at(mark.end_line)?);
    (path == end_path && start <= end).then_some(HunkRef {
        path,
        start_line: start,
        end_line: end,
    })
}

/// A step as (id, title, hunks).
type StepRow = (String, String, Vec<HunkRef>);

fn no_version(number: u32) -> Outcome {
    not_found(format!("there is no version {number} of the story"))
}

/// The steps of story `number` and the commit their hunks are at: the static story at the
/// review's head for `STATIC`, else the spec of that version.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
async fn steps_of(
    shared: &Arc<Shared>,
    pr: &PrRef,
    number: u32,
) -> Result<(String, Vec<StepRow>), Outcome> {
    if number == STATIC {
        let head = reviews::load_stored(shared, pr)?
            .ok_or_else(|| reviews::invalid_state(format!("no review for {pr}; open it first")))?
            .head_sha;
        calls::ensure_index(shared, pr).await;
        let Some(built) = shared.indexes.ready(pr, &head) else {
            return Err(reviews::invalid_state(NOT_READY));
        };
        let steps = static_steps(built)
            .await
            .into_iter()
            .map(|s| (s.id, s.title, s.hunks))
            .collect();
        return Ok((head, steps));
    }
    let Some(version) = load_story_version(&shared.paths, pr, number) else {
        return Err(no_version(number));
    };
    let version = as_shown(shared, pr, version);
    let Some(spec) = version.spec else {
        return Err(reviews::invalid_state("That version has no story"));
    };
    let steps = spec
        .steps
        .into_iter()
        .map(|s| (s.id, s.title, s.hunks))
        .collect();
    Ok((version.commit, steps))
}

/// How many commits `head` has that `from` has not; 0 when they cannot be counted.
async fn new_commits(shared: &Shared, pr: &PrRef, from: &str, head: &str) -> u32 {
    if from == head || !is_object_id(from) || !is_object_id(head) {
        return 0;
    }
    let Ok(repo) = repo_of_worktree(&shared.paths.worktree_for(pr)).await else {
        return 0;
    };
    git(&repo, &["rev-list", "--count", &format!("{from}..{head}")])
        .await
        .ok()
        .and_then(|count| count.trim().parse().ok())
        .unwrap_or(0)
}

/// `GetStory`: the versions, the latest as shown, what the daemon does, the static story and
/// the steps read.
pub(crate) async fn get(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    let head = match reviews::load_stored(shared, pr) {
        Ok(Some(review)) => review.head_sha,
        Ok(None) => {
            return reviews::invalid_state(format!("no review for {pr}; open it first"));
        }
        Err(out) => return out,
    };
    calls::ensure_index(shared, pr).await;
    let static_story = match shared.indexes.ready(pr, &head) {
        Some(built) => static_steps(built).await,
        None => Vec::new(),
    };
    let stored = versions_of(shared, pr);
    let versions: Vec<StoryVersionInfo> = stored.iter().map(StoryVersionInfo::from).collect();
    let latest = stored.last().cloned();
    let state =
        shared
            .stories
            .live_state(pr)
            .unwrap_or_else(|| match latest.as_ref().map(|v| &v.state) {
                Some(StoryVersionState::Failed { message }) => StoryState::Failed {
                    message: message.clone(),
                },
                _ => StoryState::Idle,
            });
    let (commit, shown): (String, Vec<(String, Vec<HunkRef>)>) = match latest
        .as_ref()
        .and_then(|v| v.spec.as_ref().map(|s| (v, s)))
    {
        Some((version, spec)) => (
            version.commit.clone(),
            spec.steps
                .iter()
                .map(|s| (s.id.clone(), s.hunks.clone()))
                .collect(),
        ),
        None => (
            head.clone(),
            static_story
                .iter()
                .map(|s| (s.id.clone(), s.hunks.clone()))
                .collect(),
        ),
    };
    let marks = load_read_marks(&shared.paths, pr);
    let maps = mappings(shared, pr, &marks, &commit).await;
    let read = shown
        .iter()
        .filter(|(_, hunks)| step_read(&marks, &commit, hunks, &|mark| mapped(&maps, mark)))
        .map(|(id, _)| id.clone())
        .collect();
    let new_commits = match &latest {
        Some(version) => new_commits(shared, pr, &version.commit, &head).await,
        None => 0,
    };
    Outcome::Ok(Reply::Story {
        versions,
        latest,
        state,
        static_steps: static_story,
        read,
        head,
        new_commits,
    })
}

/// `GetStoryVersion`: one version as shown.
pub(crate) fn version(shared: &Shared, pr: &PrRef, number: u32) -> Outcome {
    match load_story_version(&shared.paths, pr, number) {
        Some(version) => Outcome::Ok(Reply::StoryVersion(as_shown(shared, pr, version))),
        None => no_version(number),
    }
}

/// The step `id` of `steps`, or the refusal.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
fn find_step(steps: Vec<StepRow>, id: &str) -> Result<StepRow, Outcome> {
    steps.into_iter().find(|s| s.0 == id).ok_or_else(|| {
        not_found(format!(
            "there is no step {} in this story",
            printable(&cut(id, ID_MAX))
        ))
    })
}

/// `MarkStepRead`: marks the hunks of a step read, or unread. Only the static story and the
/// latest version can change.
pub(crate) async fn mark_read(
    shared: &Arc<Shared>,
    pr: &PrRef,
    number: u32,
    step: &str,
    read: bool,
) -> Outcome {
    let (commit, steps) = match steps_of(shared, pr, number).await {
        Ok(found) => found,
        Err(out) => return out,
    };
    if number != STATIC && versions_of(shared, pr).last().map(|v| v.number) != Some(number) {
        return reviews::invalid_state(READ_ONLY);
    }
    let (_, _, hunks) = match find_step(steps, step) {
        Ok(step) => step,
        Err(out) => return out,
    };
    let maps = mappings(shared, pr, &load_read_marks(&shared.paths, pr), &commit).await;
    let _guard = reviews::lock(shared, pr).await;
    // A publish or a discard may have deleted the stories while this waited for the lock.
    if !matches!(reviews::load_stored(shared, pr), Ok(Some(_))) {
        return reviews::invalid_state(format!("no review for {pr}; open it first"));
    }
    let mut marks = load_read_marks(&shared.paths, pr);
    marks.set(&commit, &hunks, read, &|mark| mapped(&maps, mark));
    match save_read_marks(&shared.paths, pr, &marks) {
        Ok(()) => Outcome::Ok(Reply::Ack),
        Err(e) => Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("could not save the read marks: {e}"),
        )),
    }
}

/// `AddStepComment`: a general draft item that names its step.
pub(crate) async fn add_step_comment(
    shared: &Arc<Shared>,
    client: &str,
    pr: &PrRef,
    number: u32,
    step: &str,
    body: &str,
) -> Outcome {
    let comment = body.trim();
    if comment.is_empty() {
        return reviews::bad_request("the comment is empty");
    }
    let steps = match steps_of(shared, pr, number).await {
        Ok((_, steps)) => steps,
        Err(out) => return out,
    };
    let (_, title, _) = match find_step(steps, step) {
        Ok(step) => step,
        Err(out) => return out,
    };
    reviews::add_item_as(
        shared,
        client,
        pr,
        Origin::Human,
        reviews::NewItem {
            kind: DraftKind::General,
            anchor: None,
            thread: None,
            body: &format!("On \"{}\": {comment}", printable(&title)),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use clusia_core::checks::CheckKind;
    use clusia_core::story::NOT_PLACED_ID;
    use clusia_harness::testkit::{Call, FakeClaude, Script, Turn, story_block};
    use serde_json::json;

    use super::*;
    use crate::calls::tests::{STORE_HEAD, code_repo, commit, pr, set_review};
    use crate::checks::tests::lab;

    const SENTENCE: &str = "The token refreshes to a new value.";

    /// A story of the code repository's three hunks: the store's line 9, the README's line 2 and
    /// the new `tool.go`.
    fn story() -> serde_json::Value {
        json!({
            "sentence": SENTENCE,
            "before": [{"text": "one", "tone": "bad"}],
            "after": [{"text": "two", "tone": "good"}],
            "use_cases": [{"id": "refresh", "name": "Refresh the token"}],
            "steps": [
                {"id": "TokenStore::refresh", "title": "The refresh returns two",
                 "subtitle": "store.rs", "explanation": "The value changes.",
                 "hunks": [{"path": "src/store.rs", "start_line": 9, "end_line": 9}],
                 "use_cases": ["refresh"]},
                {"id": "small", "title": "Small and mechanical", "mechanical": true,
                 "hunks": [{"path": "README.md", "start_line": 2, "end_line": 2},
                           {"path": "tool.go", "start_line": 1, "end_line": 3}]}
            ]
        })
    }

    fn answer(spec: serde_json::Value) -> Turn {
        Turn::answer_blocks("Here is the story.", &[story_block(spec)])
    }

    fn good() -> Turn {
        answer(story())
    }

    async fn calls(fake: &tempfile::TempDir, count: usize) -> Vec<Call> {
        let dir = fake.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            FakeClaude::wait_for_calls(&dir, count, Duration::from_secs(10))
        })
        .await
        .unwrap()
    }

    fn prompt_of(call: &Call) -> String {
        let at = call.argv.iter().position(|a| a == "-p").expect("a prompt");
        call.argv[at + 1].clone()
    }

    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only asks whether the process exists.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    async fn settled(shared: &Shared, n: u64) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while shared.stories.running(&pr(n)) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the run ends");
    }

    /// A lab whose review 1 is the code repository, with `script` as the agent.
    fn story_lab(script: Script) -> (tempfile::TempDir, tempfile::TempDir, Arc<Shared>, String) {
        let (home, fake, shared) = lab(script, 1, |_| {});
        let (_base, head) = code_repo(&shared, 1);
        (home, fake, shared, head)
    }

    fn latest(shared: &Shared, n: u64) -> StoryVersion {
        versions_of(shared, &pr(n)).pop().expect("a version")
    }

    fn refused(out: Outcome) -> ProtocolError {
        match out {
            Outcome::Err(e) => e,
            other => panic!("refused, got {other:?}"),
        }
    }

    struct Story {
        versions: Vec<StoryVersionInfo>,
        latest: Option<StoryVersion>,
        state: StoryState,
        static_steps: Vec<clusia_calls::StaticStep>,
        read: Vec<String>,
        head: String,
        new_commits: u32,
    }

    async fn story_of(shared: &Arc<Shared>, n: u64) -> Story {
        let Outcome::Ok(Reply::Story {
            versions,
            latest,
            state,
            static_steps,
            read,
            head,
            new_commits,
        }) = get(shared, &pr(n)).await
        else {
            panic!("a story");
        };
        Story {
            versions,
            latest,
            state,
            static_steps,
            read,
            head,
            new_commits,
        }
    }

    /// The story once the index of the head is ready.
    async fn static_story(shared: &Arc<Shared>, n: u64) -> Story {
        crate::calls::tests::built(shared, n).await;
        story_of(shared, n).await
    }

    #[tokio::test]
    async fn writing_forks_one_turn_that_never_reaches_the_chat_and_stores_a_written_version() {
        let (_home, fake, shared, head) = story_lab(Script::one(good()));
        let mut events = shared.events.subscribe();
        assert!(matches!(
            write(&shared, &pr(1)).await,
            Outcome::Ok(Reply::Ack)
        ));
        settled(&shared, 1).await;
        let version = latest(&shared, 1);
        assert_eq!(
            (
                version.number,
                version.commit.as_str(),
                version.harness.as_str()
            ),
            (1, head.as_str(), "Claude Code")
        );
        assert_eq!(version.state, StoryVersionState::Written);
        assert!(clusia_core::time::parse_rfc3339(&version.started_at).is_some());
        let spec = version.spec.expect("a spec");
        assert_eq!(spec.sentence, SENTENCE);
        let ids: Vec<&str> = spec.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["TokenStore::refresh", "small"]);
        let calls = FakeClaude::calls(fake.path());
        assert_eq!(calls.len(), 1);
        let prompt = prompt_of(&calls[0]);
        assert!(
            prompt.contains("TokenStore::refresh"),
            "the static order is in the prompt: {prompt}"
        );
        assert!(prompt.contains("Refresh twice"), "so is the commit message");
        let told: Vec<StoryState> = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|(_, event)| match event {
                Event::StoryChanged { state, .. } => Some(state),
                _ => None,
            })
            .collect();
        assert_eq!(told.first(), Some(&StoryState::Writing));
        assert_eq!(told.last(), Some(&StoryState::Idle));
        let entries = agent_log::read(&shared.paths.agent_log(&pr(1)));
        let states: Vec<&str> = entries
            .iter()
            .filter_map(|e| match e {
                AgentLogEntry::Story { state, .. } => Some(state.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(states, ["waiting", "running", "done"]);
        assert_eq!(agent_log::check_turns(&entries).len(), 1);
        assert!(agent_log::unfinished_turns(&entries).is_empty());
        assert_eq!(
            agent_log::last_summary(&entries),
            None,
            "the story is not the summary"
        );
    }

    #[tokio::test]
    async fn the_static_story_answers_first_and_read_marks_carry_into_the_written_one() {
        let (_home, _fake, shared, head) = story_lab(Script::one(good()));
        let early = story_of(&shared, 1).await;
        assert!(
            early.static_steps.is_empty(),
            "the index is still being built"
        );
        let shown = static_story(&shared, 1).await;
        assert!(shown.versions.is_empty() && shown.latest.is_none());
        assert_eq!(
            (shown.state, shown.head.as_str()),
            (StoryState::Idle, head.as_str())
        );
        assert_eq!(shown.new_commits, 0, "no version, no new commits");
        assert_eq!(shown.static_steps[0].id, "TokenStore::refresh");
        assert!(
            shown.static_steps.iter().any(|s| s.id == "tool.go"),
            "an unsupported file is its own step"
        );
        let last = shown.static_steps.last().unwrap();
        assert!(last.other && last.hunks.iter().any(|h| h.path == "README.md"));
        assert!(shown.read.is_empty());

        assert!(matches!(
            mark_read(&shared, &pr(1), STATIC, "TokenStore::refresh", true).await,
            Outcome::Ok(Reply::Ack)
        ));
        assert_eq!(story_of(&shared, 1).await.read, ["TokenStore::refresh"]);
        assert_eq!(
            load_read_marks(&shared.paths, &pr(1)).hunks,
            [ReadHunk {
                commit: head.clone(),
                path: "src/store.rs".into(),
                start_line: 9,
                end_line: 9
            }]
        );
        // Marking twice keeps one mark.
        let _ = mark_read(&shared, &pr(1), STATIC, "TokenStore::refresh", true).await;
        assert_eq!(load_read_marks(&shared.paths, &pr(1)).hunks.len(), 1);

        let _ = write(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        assert_eq!(
            story_of(&shared, 1).await.read,
            ["TokenStore::refresh"],
            "the hunk was read in the static story, so the written step is read"
        );
        let _ = mark_read(&shared, &pr(1), 1, "TokenStore::refresh", false).await;
        assert!(story_of(&shared, 1).await.read.is_empty());
        assert!(load_read_marks(&shared.paths, &pr(1)).hunks.is_empty());
    }

    #[tokio::test]
    async fn a_mark_at_an_older_commit_is_read_where_its_lines_moved() {
        let (_home, _fake, shared, head) = story_lab(Script::one(good()));
        static_story(&shared, 1).await;
        let _ = mark_read(&shared, &pr(1), STATIC, "TokenStore::refresh", true).await;
        // A push adds two lines above the store: line 9 is now line 11.
        let root = shared.paths.worktree_for(&pr(1));
        let moved = commit(
            &root,
            &[(
                "src/store.rs",
                Some(&format!("// one\n// two\n{STORE_HEAD}")),
            )],
            "Comment the store",
        );
        let base = reviews::load_stored(&shared, &pr(1))
            .unwrap()
            .unwrap()
            .base_sha;
        set_review(&shared, 1, &base, &moved);
        let shown = static_story(&shared, 1).await;
        assert_ne!(shown.head, head);
        let refresh = shown
            .static_steps
            .iter()
            .find(|s| s.id == "TokenStore::refresh")
            .expect("the step");
        assert!(
            refresh
                .hunks
                .iter()
                .any(|h| h.path == "src/store.rs" && h.start_line == 11)
        );
        assert!(
            shown.read.iter().any(|id| id == "TokenStore::refresh"),
            "{:?}",
            shown.read
        );
    }

    #[tokio::test]
    async fn the_trigger_rules_busy_exists_and_retry_replaces_the_unwritten_version() {
        let script = Script::turns(vec![Turn::hanging(), good(), good()]);
        let (_home, fake, shared, _head) = story_lab(script);
        assert!(matches!(
            write(&shared, &pr(1)).await,
            Outcome::Ok(Reply::Ack)
        ));
        let pid = calls(&fake, 1).await[0].pid;
        let busy = refused(write(&shared, &pr(1)).await);
        assert_eq!(
            (busy.code, busy.message.as_str()),
            (ErrorCode::Busy, "The story is already being written")
        );
        assert_eq!(
            refused(rewrite(&shared, &pr(1)).await).code,
            ErrorCode::Busy
        );
        assert!(matches!(stop(&shared, &pr(1)), Outcome::Ok(Reply::Ack)));
        settled(&shared, 1).await;
        assert!(!alive(pid), "no process is left behind");
        assert_eq!(latest(&shared, 1).state, StoryVersionState::NotWritten);
        let exists = refused(write(&shared, &pr(1)).await);
        assert_eq!(
            (exists.code, exists.message.as_str()),
            (
                ErrorCode::InvalidState,
                "A story already exists; use Rewrite"
            )
        );
        // Retry: the unwritten version of this commit is replaced, not followed.
        assert!(matches!(
            rewrite(&shared, &pr(1)).await,
            Outcome::Ok(Reply::Ack)
        ));
        settled(&shared, 1).await;
        assert_eq!(versions_of(&shared, &pr(1)).len(), 1);
        assert_eq!(
            (latest(&shared, 1).number, latest(&shared, 1).state),
            (1, StoryVersionState::Written)
        );
        assert!(
            !prompt_of(&calls(&fake, 2).await[1]).contains(SENTENCE),
            "an unwritten version gives no previous story"
        );
        // Rewrite after a written version: a new version, given the previous story.
        assert!(matches!(
            rewrite(&shared, &pr(1)).await,
            Outcome::Ok(Reply::Ack)
        ));
        settled(&shared, 1).await;
        assert_eq!(latest(&shared, 1).number, 2);
        assert!(prompt_of(&calls(&fake, 3).await[2]).contains(SENTENCE));
        assert_eq!(
            story_of(&shared, 1).await.new_commits,
            0,
            "written from the head"
        );
        // Version 1 is older now, and read-only.
        let older = refused(mark_read(&shared, &pr(1), 1, "TokenStore::refresh", true).await);
        assert_eq!(
            (older.code, older.message.as_str()),
            (ErrorCode::InvalidState, "Older versions are read-only")
        );
        assert!(matches!(
            mark_read(&shared, &pr(1), 2, "TokenStore::refresh", true).await,
            Outcome::Ok(Reply::Ack)
        ));
        assert!(matches!(
            mark_read(&shared, &pr(1), STATIC, "TokenStore::refresh", true).await,
            Outcome::Ok(Reply::Ack)
        ));
    }

    #[tokio::test]
    async fn no_harness_and_no_review_are_refused_and_nothing_runs() {
        let (_home, fake, shared) = lab(Script::one(good()), 1, |config| {
            config.harness.program = Some("/nonexistent/claude".into());
        });
        code_repo(&shared, 1);
        let none = refused(write(&shared, &pr(1)).await);
        assert_eq!(
            (none.code, none.message.as_str()),
            (ErrorCode::NotConfigured, "No harness is set up")
        );
        let no_review = refused(write(&shared, &pr(9)).await);
        assert_eq!(no_review.code, ErrorCode::InvalidState);
        assert!(no_review.message.contains("open it first"));
        assert!(versions_of(&shared, &pr(1)).is_empty());
        assert!(FakeClaude::calls(fake.path()).is_empty());
    }

    /// The setting gates only the window's automatic write; a click always runs.
    #[tokio::test]
    async fn the_story_setting_does_not_refuse_a_write_that_was_asked_for() {
        let (_home, fake, shared) = lab(Script::one(good()), 1, |config| {
            config.harness.story = false;
        });
        code_repo(&shared, 1);
        assert!(matches!(
            write(&shared, &pr(1)).await,
            Outcome::Ok(Reply::Ack)
        ));
        settled(&shared, 1).await;
        assert_eq!(latest(&shared, 1).state, StoryVersionState::Written);
        assert_eq!(FakeClaude::calls(fake.path()).len(), 1);
    }

    #[tokio::test]
    async fn an_unusable_answer_gets_one_repair_turn_then_the_version_fails_with_its_reason() {
        let script = Script::turns(vec![
            Turn::answer("no block"),
            Turn::answer("still no block"),
        ]);
        let (_home, fake, shared, _head) = story_lab(script);
        let _ = write(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        let calls = FakeClaude::calls(fake.path());
        assert_eq!(calls.len(), 2, "one repair, not two");
        assert!(
            prompt_of(&calls[1]).contains("no block"),
            "the repair holds the answer"
        );
        let StoryVersionState::Failed { message } = latest(&shared, 1).state else {
            panic!("failed");
        };
        assert!(
            !message.is_empty() && message.chars().count() <= REASON_MAX,
            "{message}"
        );
        assert_eq!(
            story_of(&shared, 1).await.state,
            StoryState::Failed { message }
        );
    }

    /// Thirteen steps are sent back once, and the repaired answer is kept.
    #[tokio::test]
    async fn thirteen_steps_are_sent_back_once_and_the_repaired_story_kept() {
        let mut thirteen = story();
        thirteen["steps"] = json!(
            (0..13)
                .map(
                    |i| json!({"id": format!("s{i}"), "title": format!("Step {i}"),
                            "hunks": [{"path": "src/store.rs", "start_line": 9, "end_line": 9}]})
                )
                .collect::<Vec<_>>()
        );
        let script = Script::turns(vec![answer(thirteen), good()]);
        let (_home, fake, shared, _head) = story_lab(script);
        let _ = write(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        assert_eq!(FakeClaude::calls(fake.path()).len(), 2);
        assert!(prompt_of(&FakeClaude::calls(fake.path())[1]).contains("Step 12"));
        assert_eq!(latest(&shared, 1).state, StoryVersionState::Written);
    }

    /// Agent text is cleaned, and every hunk stays reachable.
    #[tokio::test]
    async fn agent_text_is_cleaned_and_every_hunk_stays_reachable() {
        let odd = json!({
            "sentence": "A \u{1b}[2J change \u{202e}here",
            "use_cases": [{"id": "refresh", "name": "Refresh the token"}],
            "steps": [
                {"id": "TokenStore::refresh", "title": "Refresh \u{1b}[31mred",
                 "hunks": [{"path": "src/store.rs", "start_line": 9, "end_line": 9},
                           {"path": "src/store.rs", "start_line": 40, "end_line": 41}],
                 "use_cases": ["refresh", "nope"],
                 "since_previous": "There is no previous version."}
            ],
            "removed": [{"id": "never", "title": "Never existed"}]
        });
        let (_home, _fake, shared, _head) = story_lab(Script::one(answer(odd)));
        let _ = write(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        let spec = latest(&shared, 1).spec.expect("a spec");
        assert!(!spec.sentence.contains('\u{1b}') && !spec.sentence.contains('\u{202e}'));
        let first = &spec.steps[0];
        assert!(!first.title.contains('\u{1b}'));
        assert_eq!(
            first.hunks.len(),
            1,
            "the lines outside the diff are dropped"
        );
        assert_eq!(first.use_cases, ["refresh"]);
        assert_eq!(first.since_previous, None, "version 1 has no previous");
        assert!(spec.removed.is_empty());
        let not_placed = spec.steps.last().unwrap();
        assert_eq!(not_placed.id, NOT_PLACED_ID);
        let mut paths: Vec<&str> = not_placed.hunks.iter().map(|h| h.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, ["README.md", "tool.go"]);
    }

    /// A stop while the turn waits for a place leaves the version not written.
    #[tokio::test]
    async fn stopping_while_waiting_for_a_place_leaves_not_written_and_starts_no_process() {
        let (_home, fake, shared) = lab(Script::one(Turn::hanging()), 3, |_| {});
        code_repo(&shared, 3);
        for n in 1..=2 {
            let _ = crate::checks::run(&shared, &pr(n), CheckKind::Security).await;
        }
        calls(&fake, 2).await;
        let _ = write(&shared, &pr(3)).await;
        tokio::time::timeout(Duration::from_secs(30), async {
            while shared.stories.live_state(&pr(3)) != Some(StoryState::Waiting) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the turn waits for a place");
        assert_eq!(story_of(&shared, 3).await.state, StoryState::Waiting);
        let _ = stop(&shared, &pr(3));
        settled(&shared, 3).await;
        assert_eq!(latest(&shared, 3).state, StoryVersionState::NotWritten);
        assert_eq!(story_of(&shared, 3).await.state, StoryState::Idle);
        assert_eq!(
            FakeClaude::calls(fake.path()).len(),
            2,
            "the story never started"
        );
        crate::checks::shutdown(&shared).await;
    }

    /// A shutdown, or the review ending during the turn, leaves no process and no late version.
    #[tokio::test]
    async fn a_shutdown_or_the_review_ending_stops_the_turn_and_leaves_no_process() {
        let (_home, fake, shared, _head) = story_lab(Script::one(Turn::hanging()));
        let _ = write(&shared, &pr(1)).await;
        let pid = calls(&fake, 1).await[0].pid;
        shutdown(&shared).await;
        assert!(!shared.stories.running(&pr(1)));
        assert!(!alive(pid), "no process is left behind");
        assert_eq!(latest(&shared, 1).state, StoryVersionState::NotWritten);
        assert_eq!(
            refused(rewrite(&shared, &pr(1)).await).message,
            "clusiad is stopping"
        );

        // A publish during the turn: nothing is stored after the review ends.
        let (_home, fake, shared, _head) = story_lab(Script::one(good().delay_ms(800)));
        let _ = write(&shared, &pr(1)).await;
        calls(&fake, 1).await;
        stop_all(&shared, &pr(1), Why::Ended).await;
        forget(&shared, &pr(1));
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(!shared.stories.running(&pr(1)));
        assert!(
            !shared.paths.stories_for(&pr(1)).exists(),
            "no stale version is stored"
        );
    }

    /// A version a stopped daemon left writing reads as not written.
    #[tokio::test]
    async fn a_version_left_writing_reads_as_not_written_and_a_retry_takes_its_number() {
        let (_home, _fake, shared, head) = story_lab(Script::one(good()));
        let crashed = StoryVersion {
            number: 3,
            commit: head.clone(),
            harness: "Claude Code".into(),
            started_at: "2026-10-19T12:00:00Z".into(),
            state: StoryVersionState::Writing,
            spec: None,
            notes: Vec::new(),
        };
        save_story_version(&shared.paths, &pr(1), &crashed).unwrap();
        let shown = story_of(&shared, 1).await;
        assert_eq!(shown.latest.unwrap().state, StoryVersionState::NotWritten);
        assert_eq!(shown.state, StoryState::Idle);
        assert_eq!(
            load_story_versions(&shared.paths, &pr(1))[0].state,
            StoryVersionState::Writing,
            "reading never rewrites the file"
        );
        let Outcome::Ok(Reply::StoryVersion(one)) = version(&shared, &pr(1), 3) else {
            panic!("a version");
        };
        assert_eq!(one.state, StoryVersionState::NotWritten);
        assert_eq!(
            refused(version(&shared, &pr(1), 9)).code,
            ErrorCode::NotFound
        );
        let _ = rewrite(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        assert_eq!(
            (latest(&shared, 1).number, latest(&shared, 1).state),
            (3, StoryVersionState::Written)
        );
    }

    /// A push during the turn keeps the story for the commit the turn was given.
    #[tokio::test]
    async fn a_push_during_the_turn_keeps_the_story_for_the_commit_it_was_given() {
        let (_home, fake, shared, head) = story_lab(Script::one(good().delay_ms(600)));
        let _ = write(&shared, &pr(1)).await;
        calls(&fake, 1).await;
        let root = shared.paths.worktree_for(&pr(1));
        let pushed = commit(&root, &[("NOTES.md", Some("later\n"))], "Later");
        let base = reviews::load_stored(&shared, &pr(1))
            .unwrap()
            .unwrap()
            .base_sha;
        set_review(&shared, 1, &base, &pushed);
        settled(&shared, 1).await;
        let version = latest(&shared, 1);
        assert_eq!(
            (version.commit.as_str(), version.state),
            (head.as_str(), StoryVersionState::Written)
        );
        let shown = story_of(&shared, 1).await;
        assert_eq!(shown.head, pushed, "the window shows it outdated");
        assert_eq!(shown.new_commits, 1, "one commit since the story's");
    }

    #[tokio::test]
    async fn a_version_pins_its_commit_until_the_review_is_forgotten() {
        let (_home, _fake, shared, head) = story_lab(Script::one(good()));
        let _ = write(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        let root = shared.paths.worktree_for(&pr(1));
        let pins = || {
            crate::calls::tests::git_in(
                &root,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/clusia/story",
                ],
            )
        };
        assert_eq!(pins(), format!("refs/clusia/story/pr-1-v1 {head}"));
        forget(&shared, &pr(1));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !pins().is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the pin goes with the review");
    }

    #[tokio::test]
    async fn a_step_comment_names_its_step_and_bad_ones_are_refused() {
        let (_home, _fake, shared, _head) = story_lab(Script::one(good()));
        let _ = write(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        let Outcome::Ok(Reply::DraftItem(item)) = add_step_comment(
            &shared,
            "t",
            &pr(1),
            1,
            "TokenStore::refresh",
            "  Why two?\n",
        )
        .await
        else {
            panic!("a draft item");
        };
        assert_eq!(
            (item.kind, item.origin, item.anchor, item.body.as_str()),
            (
                DraftKind::General,
                Origin::Human,
                None,
                "On \"The refresh returns two\": Why two?"
            )
        );
        let title = static_story(&shared, 1).await.static_steps[0].title.clone();
        let Outcome::Ok(Reply::DraftItem(on_static)) =
            add_step_comment(&shared, "t", &pr(1), STATIC, "TokenStore::refresh", "Ok").await
        else {
            panic!("a draft item");
        };
        assert_eq!(on_static.body, format!("On \"{}\": Ok", printable(&title)));
        let codes: Vec<ErrorCode> = [
            add_step_comment(&shared, "t", &pr(1), 1, "TokenStore::refresh", " \n").await,
            add_step_comment(&shared, "t", &pr(1), 1, "nope", "x").await,
            add_step_comment(&shared, "t", &pr(1), 9, "TokenStore::refresh", "x").await,
            mark_read(&shared, &pr(1), 1, "nope", true).await,
            mark_read(&shared, &pr(1), 9, "small", true).await,
        ]
        .into_iter()
        .map(|out| refused(out).code)
        .collect();
        assert_eq!(
            codes,
            [
                ErrorCode::BadRequest,
                ErrorCode::NotFound,
                ErrorCode::NotFound,
                ErrorCode::NotFound,
                ErrorCode::NotFound
            ]
        );
    }

    /// A publish that deletes the stories while a mark waits for the review lock.
    #[tokio::test]
    async fn a_mark_racing_a_publish_writes_nothing() {
        let (_home, _fake, shared, _head) = story_lab(Script::one(good()));
        static_story(&shared, 1).await;
        let guard = reviews::lock(&shared, &pr(1)).await;
        let racing = {
            let shared = shared.clone();
            tokio::spawn(async move {
                mark_read(&shared, &pr(1), STATIC, "TokenStore::refresh", true).await
            })
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        std::fs::remove_file(shared.paths.review_file(&pr(1))).unwrap();
        delete_stories(&shared.paths, &pr(1)).unwrap();
        drop(guard);
        assert_eq!(refused(racing.await.unwrap()).code, ErrorCode::InvalidState);
        assert!(
            !shared.paths.stories_for(&pr(1)).exists(),
            "nothing recreated it"
        );
    }

    /// A forget that runs while a write is being accepted leaves no pin behind.
    #[tokio::test]
    async fn a_forget_during_start_leaves_no_pin() {
        let (_home, _fake, shared, _head) = story_lab(Script::one(good()));
        let root = shared.paths.worktree_for(&pr(1));
        let pins = || {
            crate::calls::tests::git_in(
                &root,
                &["for-each-ref", "--format=%(refname)", "refs/clusia/story"],
            )
        };
        for _ in 0..5 {
            let starting = {
                let shared = shared.clone();
                tokio::spawn(async move { write(&shared, &pr(1)).await })
            };
            forget(&shared, &pr(1));
            let _ = starting.await.unwrap();
            stop_all(&shared, &pr(1), Why::Ended).await;
            // A run accepted after the first forget is the review's own: its forget is this one.
            forget(&shared, &pr(1));
            // The unpin runs in a task of its own: wait for it.
            tokio::time::timeout(Duration::from_secs(20), async {
                while !pins().is_empty() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("no story ref outlives the forget");
        }
    }

    #[test]
    fn the_start_time_is_rfc_3339_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_792_411_200), "2026-10-19T12:00:00Z");
        assert_eq!(rfc3339(1_792_411_200 + 3_661), "2026-10-19T13:01:01Z");
    }
}
