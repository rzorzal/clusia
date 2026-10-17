//! Diagrams of a change: a plan turn that picks up to eight diagrams, then one turn for each,
//! every turn forked from the review's session like a check and sharing the places of the
//! checks. A review has at most one run at a time. The version being drawn is on disk from the
//! moment the plan is made, so an entry changes there as soon as it changes on screen.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clusia_core::diagram::{
    DiagramContext, DiagramEntry, DiagramKind, DiagramVersion, EntryState, ID_MAX, PlanEntry,
    VersionPhase, context_block, draw_prompt, keywords_entry, plan_prompt, repair_prompt, validate,
    validate_plan,
};
use clusia_core::keywords::{
    AddedLines, CheckedPlan, FileHits, Hit, KEYWORDS_KEY, KeywordMap, KeywordPlan, MAX_FILE_BYTES,
    MAX_MAP_BYTES, MAX_PLACEMENT_FILES, build_map, file_hits, has_word, keyword_plan_prompt,
    placement_prompt, validate_keyword_plan, validate_placement,
};
use clusia_core::printable::printable;
use clusia_core::{
    Anchor, DiffMap, DraftKind, FileDiff, LineMap, Origin, PrRef, ReviewState, Side, added_lines,
    fits_diff,
};
use clusia_git::{
    GrepCaps, GrepHit, diagram_ref, diff_between, file_at, git, grep_words, is_object_id,
    pin_commit, repo_of_worktree,
};
use clusia_harness::AgentEvent;
use clusia_harness::{
    extract_diagram, extract_keyword_placement, extract_keyword_plan, extract_plan,
};
use clusia_protocol::{
    AgentErrorKind, AgentLogEntry, AnchorInput, DiagramsState, ErrorCode, Event, Outcome,
    ProtocolError, Reply, TurnOrigin, topics,
};
use clusia_store::diagrams::{
    delete_diagrams, load_diagram_version, load_diagram_versions, load_latest_diagram_version,
    next_diagram_number, save_diagram_version,
};
use clusia_store::load_review_cache;
use tokio::sync::watch;

use crate::agent_log;
use crate::reviews;
use crate::state::Shared;
use crate::sync::now_unix;
use crate::turns::{self, Boxed, SessionSource, Stop, TurnEnd, TurnRun, TurnSink, Why};

/// How long `stop_all` waits for the run of a review to end.
const STOP_WAIT: Duration = Duration::from_secs(4);
/// How long a shutdown waits for the runs to end.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
/// The longest reason an entry or a plan shows.
const REASON_MAX: usize = 200;

/// What a run draws.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    /// A plan, then every diagram of it, as a new version.
    All,
    /// One entry of the latest version.
    One(String),
}

/// A run that waits for a place or is running.
struct Run {
    id: u64,
    stop: Arc<Stop>,
    state: DiagramsState,
    /// The number of the version this run writes, once it has one.
    writing: Option<u32>,
}

#[derive(Default)]
struct Entry {
    run: Option<Run>,
    /// A run starts when the summary turn ends well.
    awaiting: bool,
    /// Why the last plan failed, until the next run starts.
    failed: Option<String>,
}

impl Entry {
    fn empty(&self) -> bool {
        self.run.is_none() && !self.awaiting && self.failed.is_none()
    }
}

/// The running diagrams of every review.
pub(crate) struct Diagrams {
    table: Mutex<HashMap<PrRef, Entry>>,
    /// How many runs live; shutdown waits for zero.
    live: watch::Sender<usize>,
    closing: AtomicBool,
    runs: AtomicU64,
}

impl Default for Diagrams {
    fn default() -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
            live: watch::channel(0).0,
            closing: AtomicBool::new(false),
            runs: AtomicU64::new(0),
        }
    }
}

impl Diagrams {
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

    /// What the daemon does for `pr`: the run, the summary it waits for, or why the last plan
    /// failed; `None` when idle.
    fn live_state(&self, pr: &PrRef) -> Option<DiagramsState> {
        self.with(pr, |entry| match &entry.run {
            Some(run) => Some(run.state.clone()),
            None if entry.awaiting => Some(DiagramsState::Planning),
            None => entry
                .failed
                .clone()
                .map(|message| DiagramsState::Failed { message }),
        })
    }

    fn running(&self, pr: &PrRef) -> bool {
        self.with(pr, |entry| entry.run.is_some())
    }

    fn set_writing(&self, pr: &PrRef, run: u64, number: u32) {
        self.with(pr, |entry| {
            if let Some(r) = entry.run.as_mut().filter(|r| r.id == run) {
                r.writing = Some(number);
            }
        });
    }

    /// The version the run of `pr` writes, if it has begun one.
    fn writing(&self, pr: &PrRef) -> Option<u32> {
        self.with(pr, |entry| entry.run.as_ref().and_then(|r| r.writing))
    }

    fn set_state(&self, pr: &PrRef, run: u64, state: DiagramsState) {
        self.with(pr, |entry| {
            if let Some(r) = entry.run.as_mut().filter(|r| r.id == run) {
                r.state = state;
            }
        });
    }

    /// Ends run `run`, leaving `failed` as the reason; `false` when that run is not the
    /// registered one any more.
    fn end_run(&self, pr: &PrRef, run: u64, failed: Option<String>) -> bool {
        self.with(pr, |entry| {
            if entry.run.as_ref().is_some_and(|r| r.id == run) {
                entry.run = None;
                entry.failed = failed;
                true
            } else {
                false
            }
        })
    }
}

/// Counts a run in `Diagrams::live` while it lives.
struct Live(Arc<Shared>);

impl Live {
    fn new(shared: &Arc<Shared>) -> Self {
        shared.diagrams.live.send_modify(|n| *n += 1);
        Self(shared.clone())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.diagrams.live.send_modify(|n| *n -= 1);
    }
}

/// What a run needs once it has been accepted.
struct Job {
    run: u64,
    mode: Mode,
    pr_title: String,
    /// The paths of the pull request's files, for the anchors of a drawing.
    files: Vec<String>,
    /// Whether the 3D keyword view is drawn after the agent's diagrams.
    keywords: bool,
    /// The lines the pull request adds, per path, for seeds and changed places.
    added: AddedLines,
    /// The commit the run reads.
    head: String,
    /// The display name of the harness, kept in the version.
    name: String,
    limit: Duration,
    stop: Arc<Stop>,
}

impl Job {
    fn context(&self, forked: bool) -> DiagramContext {
        DiagramContext {
            pr_title: self.pr_title.clone(),
            files: self.files.clone(),
            forked,
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

/// How a run ended.
enum End {
    Done,
    Failed(String),
    Over(Why),
    /// The entry to draw again is not there any more; the run ends without drawing.
    Refused(String),
}

fn tell_state(shared: &Shared, pr: &PrRef, state: DiagramsState) {
    shared.publish(
        topics::AGENT,
        Event::DiagramsChanged {
            pr: pr.clone(),
            state,
        },
    );
}

fn note_log(shared: &Shared, pr: &PrRef, turn: u64, state: &str, key: Option<&str>) {
    let entry = AgentLogEntry::Diagrams {
        at: now_unix(),
        turn,
        state: state.to_string(),
        key: key.map(str::to_string),
    };
    if let Err(e) = agent_log::append(&shared.paths.agent_log(pr), &entry) {
        tracing::warn!(error = %e, pr = %pr, "cannot append to the agent log");
    }
}

fn current_head(shared: &Shared, pr: &PrRef) -> Option<String> {
    reviews::load_stored(shared, pr)
        .ok()
        .flatten()
        .map(|review| review.head_sha)
}

/// A stored version as it is shown: the one a run writes as it is on disk, any other with what a
/// crash or a kill left waiting or drawing read as not drawn.
fn as_shown(shared: &Shared, pr: &PrRef, version: DiagramVersion) -> DiagramVersion {
    if shared.diagrams.writing(pr) == Some(version.number) {
        version
    } else {
        version.recovered()
    }
}

/// The stored versions of `pr`, oldest first, as they are shown.
fn versions_of(shared: &Shared, pr: &PrRef) -> Vec<DiagramVersion> {
    load_diagram_versions(&shared.paths, pr)
        .into_iter()
        .map(|version| as_shown(shared, pr, version))
        .collect()
}

/// A key the client sent, as it can be quoted: cut to an id's length, with escapes shown.
fn shown_key(key: &str) -> String {
    printable(&cut(key, ID_MAX))
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

/// Writes `version` unless the review ended while the run went on. Under the review lock, so a
/// publish or a discard that deleted the diagrams is never undone by a late write.
async fn keep(shared: &Shared, pr: &PrRef, job: &Job, version: &DiagramVersion) -> bool {
    let _guard = reviews::lock(shared, pr).await;
    let alive = !(job.stop.fired() && job.stop.why() == Why::Ended)
        && matches!(reviews::load_stored(shared, pr), Ok(Some(_)));
    if alive && let Err(e) = save_diagram_version(&shared.paths, pr, version) {
        tracing::warn!(error = %e, pr = %pr, "cannot store the diagrams");
    }
    alive
}

/// Sets entry `index` to `state`, stores the version and tells the windows. `false` when the
/// review ended.
async fn settle_entry(
    shared: &Shared,
    pr: &PrRef,
    job: &Job,
    version: &mut DiagramVersion,
    index: usize,
    state: EntryState,
) -> bool {
    version.entries[index].state = state.clone();
    if !keep(shared, pr, job, version).await {
        return false;
    }
    shared.publish(
        topics::AGENT,
        Event::DiagramEntryChanged {
            pr: pr.clone(),
            number: version.number,
            key: version.entries[index].plan.key.clone(),
            state,
        },
    );
    true
}

/// A run was stopped: the entries from `from` on that did not finish are not drawn, and the
/// version is done. After a review that ended nothing is written, and after a shutdown nothing
/// is told.
async fn abandon(
    shared: &Shared,
    pr: &PrRef,
    job: &Job,
    version: &mut DiagramVersion,
    from: usize,
    why: Why,
) {
    let mut changed = Vec::new();
    for entry in version.entries.iter_mut().skip(from) {
        if matches!(entry.state, EntryState::Waiting | EntryState::Drawing) {
            entry.state = EntryState::NotDrawn;
            changed.push(entry.plan.key.clone());
        }
    }
    version.phase = VersionPhase::Done;
    if !keep(shared, pr, job, version).await || why == Why::Shutdown {
        return;
    }
    for key in changed {
        shared.publish(
            topics::AGENT,
            Event::DiagramEntryChanged {
                pr: pr.clone(),
                number: version.number,
                key,
                state: EntryState::NotDrawn,
            },
        );
    }
}

/// Reads what a turn says. The session id is ignored on purpose: a drawing's session never
/// becomes the chat's.
struct DiagramSink {
    shared: Arc<Shared>,
    pr: PrRef,
    turn: u64,
    key: Option<String>,
    name: String,
    /// The text streamed so far, used when no result line carries the answer.
    streamed: String,
    answer: Option<String>,
    saw_final: bool,
    failed: bool,
    message: Option<String>,
}

impl DiagramSink {
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
                    self.fail_with(format!("{} could not finish the diagram", self.name));
                } else {
                    self.fail_with(text.trim().to_string());
                }
            }
        }
    }
}

impl TurnSink for DiagramSink {
    fn waiting(&mut self) {}

    fn started(&mut self) {
        note_log(
            &self.shared,
            &self.pr,
            self.turn,
            "running",
            self.key.as_deref(),
        );
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
    key: Option<&str>,
    prompts: (String, String),
) -> Result<String, Halt> {
    if job.stop.fired() {
        return Err(Halt::Over(job.stop.why()));
    }
    let turn = shared.sessions.next_turn(shared, pr).await;
    note_log(shared, pr, turn, "waiting", key);
    let mut sink = DiagramSink {
        shared: shared.clone(),
        pr: pr.clone(),
        turn,
        key: key.map(str::to_string),
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
        origin: TurnOrigin::Diagrams,
        places: crate::checks::places(shared),
        limit: Some(job.limit),
        stop: job.stop.clone(),
    };
    match turns::run(shared, run, &mut sink).await {
        TurnEnd::Done => {
            note_log(shared, pr, turn, "done", key);
            Ok(sink.answer.unwrap_or(sink.streamed))
        }
        TurnEnd::Failed => {
            note_log(shared, pr, turn, "failed", key);
            let message = sink
                .message
                .unwrap_or_else(|| format!("{} could not finish the diagram", job.name));
            Err(Halt::Failed(reason_of(&[message])))
        }
        TurnEnd::TimedOut => {
            note_log(shared, pr, turn, "failed", key);
            Err(Halt::Failed(format!(
                "The turn ran past its limit of {}s and was stopped",
                job.limit.as_secs()
            )))
        }
        TurnEnd::Stopped(why) => {
            note_log(shared, pr, turn, "stopped", key);
            Err(Halt::Over(why))
        }
    }
}

/// Asks, and reads the answer with `parse`. An answer that does not parse gets one more turn:
/// a fresh fork whose prompt holds that answer and what was wrong with it.
async fn ask_checked<T>(
    shared: &Arc<Shared>,
    pr: &PrRef,
    job: &Job,
    key: Option<&str>,
    base: &(dyn Fn(bool) -> String + Sync),
    parse: impl Fn(&str) -> Result<T, Vec<String>>,
) -> Result<T, Halt> {
    let first = ask(shared, pr, job, key, (base(true), base(false))).await?;
    let errors = match parse(&first) {
        Ok(value) => return Ok(value),
        Err(errors) => errors,
    };
    let again = |forked: bool| repair_prompt(&job.context(forked), &base(forked), &first, &errors);
    let second = ask(shared, pr, job, key, (again(true), again(false))).await?;
    parse(&second).map_err(|errors| Halt::Failed(reason_of(&errors)))
}

/// The drawing of `key` in the latest version before `number` that has one.
fn previous_spec(
    earlier: &[DiagramVersion],
    number: u32,
    key: &str,
) -> Option<clusia_core::diagram::DiagramSpec> {
    earlier
        .iter()
        .rev()
        .filter(|v| v.number < number)
        .find_map(|v| match &v.entry(key)?.state {
            EntryState::Ready { spec, .. } => Some(spec.clone()),
            _ => None,
        })
}

/// Keeps the commit of a version reachable, so the code of an older drawing can still be read
/// after a force-push. Best effort.
async fn pin_version(shared: &Shared, pr: &PrRef, number: u32, commit: &str) {
    if !is_object_id(commit) {
        return;
    }
    let Ok(repo) = repo_of_worktree(&shared.paths.worktree_for(pr)).await else {
        return;
    };
    if let Err(e) = pin_commit(&repo, &diagram_ref(pr.number, number), commit).await {
        tracing::warn!(error = %e, pr = %pr, "cannot pin the commit of a diagram version");
    }
}

/// Draws entry `index` of `version`. A drawing that stays unusable is that entry's failure, not
/// the run's.
async fn draw_entry(
    shared: &Arc<Shared>,
    pr: &PrRef,
    job: &Job,
    version: &DiagramVersion,
    earlier: &[DiagramVersion],
    index: usize,
) -> Result<EntryState, Why> {
    let plan = version.entries[index].plan.clone();
    if plan.kind == DiagramKind::Keywords {
        return draw_keywords(shared, pr, job, version, earlier).await;
    }
    let others: Vec<PlanEntry> = version
        .entries
        .iter()
        .filter(|e| e.plan.key != plan.key && e.plan.kind != DiagramKind::Keywords)
        .map(|e| e.plan.clone())
        .collect();
    let previous = previous_spec(earlier, version.number, &plan.key);
    let base = |forked: bool| draw_prompt(&job.context(forked), &plan, &others, previous.as_ref());
    let parsed = ask_checked(shared, pr, job, Some(&plan.key), &base, |text| {
        extract_diagram(text).and_then(|raw| validate(raw, &plan, &job.files))
    })
    .await;
    match parsed {
        Ok(checked) => Ok(EntryState::Ready {
            spec: checked.spec,
            notes: checked.notes,
        }),
        Err(Halt::Failed(reason)) => Ok(EntryState::Failed { reason }),
        Err(Halt::Over(why)) => Err(why),
    }
}

/// The lines each file's patch adds; a file without a patch (binary, too big) adds none.
fn added_of(files: &[FileDiff]) -> AddedLines {
    files
        .iter()
        .filter_map(|file| Some((file.path.clone(), added_lines(file.patch.as_deref()?))))
        .collect()
}

/// What the keywords entry says while its search and its second turn run.
const STEP_FINDING: &str = "Finding where the words live…";
const STEP_PLACING: &str = "Placing files on the axes…";
/// The note of a map whose search ran out of time.
const SEARCH_CUT_NOTE: &str = "Search stopped after 10 s; some places are missing.";
const CHECKOUT_MOVED: &str = "The checkout moved while drawing.";
const MAP_TOO_LARGE: &str = "The map is too large to keep.";
/// Validation's own reason for a plan with no family left.
const UNTRACED: &str = "No word of this change could be traced.";

/// Tells the windows what the entry being drawn does now.
fn tell_step(shared: &Shared, pr: &PrRef, job: &Job, step: &str) {
    let state = shared.diagrams.with(pr, |entry| {
        let run = entry.run.as_mut().filter(|r| r.id == job.run)?;
        let DiagramsState::Drawing { step: now, .. } = &mut run.state else {
            return None;
        };
        *now = Some(step.to_string());
        Some(run.state.clone())
    });
    if let Some(state) = state {
        tell_state(shared, pr, state);
    }
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

/// The plan of the latest map before version `number`, so a new map keeps its ids.
fn previous_map_plan(earlier: &[DiagramVersion], number: u32) -> Option<KeywordPlan> {
    earlier
        .iter()
        .rev()
        .filter(|v| v.number < number)
        .find_map(|v| match &v.entry(KEYWORDS_KEY)?.state {
            EntryState::ReadyMap { map, .. } => Some(map.plan.clone()),
            _ => None,
        })
}

fn map_notes(plan: Vec<String>, placement: Vec<String>, cut: bool) -> Vec<String> {
    let mut notes = plan;
    notes.extend(placement);
    if cut {
        notes.push(SEARCH_CUT_NOTE.to_string());
    }
    notes
}

/// The note for `dropped` families whose seed word is not on their seed line.
fn seed_note(dropped: usize) -> Option<String> {
    match dropped {
        0 => None,
        1 => Some("1 family dropped: its seed word is not on its line".to_string()),
        n => Some(format!(
            "{n} families dropped: their seed words are not on their lines"
        )),
    }
}

/// Keeps the families whose seed word is on their seed line at the run's commit; says how many
/// went. Each seed file is read once.
async fn keep_traced_seeds(repo: &Path, job: &Job, plan: &mut KeywordPlan) -> usize {
    let mut texts: BTreeMap<String, Option<String>> = BTreeMap::new();
    for family in &plan.families {
        if !texts.contains_key(&family.seed.path) {
            let text = file_at(repo, &job.head, &family.seed.path, MAX_FILE_BYTES)
                .await
                .ok();
            texts.insert(family.seed.path.clone(), text);
        }
    }
    let before = plan.families.len();
    plan.families.retain(|family| {
        let seed = &family.seed;
        texts
            .get(&seed.path)
            .and_then(Option::as_ref)
            .and_then(|text| text.lines().nth((seed.line as usize).checked_sub(1)?))
            .is_some_and(|line| has_word(line, &seed.word))
    });
    before - plan.families.len()
}

/// Whether `map` is too big to store with its version.
fn too_large(map: &KeywordMap) -> bool {
    serde_json::to_vec(map).map_or(true, |bytes| bytes.len() > MAX_MAP_BYTES)
}

/// The files with hits, the pull request's first and then by hits, with the functions around
/// their hits, read at the run's commit.
async fn files_with_hits(repo: &Path, job: &Job, hits: &[GrepHit]) -> Vec<FileHits> {
    let mut lines: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    for hit in hits {
        lines.entry(hit.path.as_str()).or_default().push(hit.line);
    }
    let mut ranked: Vec<(&str, Vec<u32>, bool)> = lines
        .into_iter()
        .map(|(path, lines)| (path, lines, job.files.iter().any(|f| f == path)))
        .collect();
    ranked.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then(b.1.len().cmp(&a.1.len()))
            .then(a.0.cmp(b.0))
    });
    ranked.truncate(MAX_PLACEMENT_FILES);
    let mut files = Vec::with_capacity(ranked.len());
    for (path, mut lines, in_pr) in ranked {
        lines.sort_unstable();
        lines.dedup();
        let text = file_at(repo, &job.head, path, MAX_FILE_BYTES)
            .await
            .unwrap_or_default();
        files.push(file_hits(path, in_pr, &text, &lines));
    }
    files
}

/// The keywords entry: the words of the change, where they appear at the run's commit, and
/// where their files sit on the agent's axes. Its failures are the entry's, not the run's.
async fn draw_keywords(
    shared: &Arc<Shared>,
    pr: &PrRef,
    job: &Job,
    version: &DiagramVersion,
    earlier: &[DiagramVersion],
) -> Result<EntryState, Why> {
    let failed = |reason: String| Ok(EntryState::Failed { reason });
    let previous = previous_map_plan(earlier, version.number);
    let words_prompt =
        |forked: bool| keyword_plan_prompt(&context_block(&job.context(forked)), previous.as_ref());
    let checked = ask_checked(shared, pr, job, Some(KEYWORDS_KEY), &words_prompt, |text| {
        extract_keyword_plan(text).and_then(|raw| validate_keyword_plan(raw, &job.added))
    })
    .await;
    let CheckedPlan {
        mut plan,
        mut notes,
    } = match checked {
        Ok(checked) => checked,
        Err(Halt::Failed(reason)) => return failed(reason),
        Err(Halt::Over(why)) => return Err(why),
    };
    tell_step(shared, pr, job, STEP_FINDING);
    let repo = shared.paths.worktree_for(pr);
    // The search reads the commit, but the attributes that leave files out come from the
    // worktree, which must still be at that commit.
    match unless_stopped(&job.stop, git(&repo, &["rev-parse", "HEAD"])).await? {
        Ok(head) if head == job.head => {}
        _ => return failed(CHECKOUT_MOVED.to_string()),
    }
    let dropped = unless_stopped(&job.stop, keep_traced_seeds(&repo, job, &mut plan)).await?;
    if plan.families.is_empty() {
        return failed(UNTRACED.to_string());
    }
    notes.extend(seed_note(dropped));
    let words = plan.all_words();
    let search = grep_words(&repo, &job.head, &words, GrepCaps::keywords());
    let found = match unless_stopped(&job.stop, search).await? {
        Ok(found) => found,
        Err(e) => {
            return failed(reason_of(&[format!(
                "The repository could not be searched: {e}"
            )]));
        }
    };
    let files = unless_stopped(&job.stop, files_with_hits(&repo, job, &found.hits)).await?;
    let sent: BTreeSet<String> = files.iter().map(|f| f.path.clone()).collect();
    tell_step(shared, pr, job, STEP_PLACING);
    let place_prompt = |_forked: bool| placement_prompt(&plan, &files);
    let placed = ask_checked(shared, pr, job, Some(KEYWORDS_KEY), &place_prompt, |text| {
        extract_keyword_placement(text).and_then(|raw| validate_placement(raw, &plan, &sent))
    })
    .await;
    let placed = match placed {
        Ok(placed) => placed,
        Err(Halt::Failed(reason)) => return failed(reason),
        Err(Halt::Over(why)) => return Err(why),
    };
    let hits: Vec<Hit> = found
        .hits
        .into_iter()
        .map(|h| Hit {
            path: h.path,
            line: h.line,
            text: h.text,
        })
        .collect();
    let notes = map_notes(notes, placed.notes, found.cut);
    let map = build_map(
        plan,
        placed.placement,
        &hits,
        found.overflow,
        found.cut,
        &job.added,
    );
    if too_large(&map) {
        return failed(MAP_TOO_LARGE.to_string());
    }
    Ok(EntryState::ReadyMap {
        map: Box::new(map),
        notes,
    })
}

/// Draws entries `from..` of `version`, one after the other.
async fn draw_entries(
    shared: &Arc<Shared>,
    pr: &PrRef,
    job: &Job,
    version: &mut DiagramVersion,
    earlier: &[DiagramVersion],
    range: std::ops::Range<usize>,
) -> End {
    let total = u32::try_from(range.len()).unwrap_or(u32::MAX);
    for (done, index) in (0u32..).zip(range.clone()) {
        let moved = current_head(shared, pr).is_some_and(|head| head != job.head);
        let over = if job.stop.fired() {
            Some(job.stop.why())
        } else {
            moved.then_some(Why::Superseded)
        };
        if let Some(why) = over {
            abandon(shared, pr, job, version, index, why).await;
            return End::Over(why);
        }
        let key = version.entries[index].plan.key.clone();
        let state = DiagramsState::Drawing {
            done,
            total,
            key,
            step: None,
        };
        shared.diagrams.set_state(pr, job.run, state.clone());
        tell_state(shared, pr, state);
        if !settle_entry(shared, pr, job, version, index, EntryState::Drawing).await {
            return End::Over(Why::Ended);
        }
        match draw_entry(shared, pr, job, version, earlier, index).await {
            Ok(state) => {
                if !settle_entry(shared, pr, job, version, index, state).await {
                    return End::Over(Why::Ended);
                }
            }
            Err(why) => {
                abandon(shared, pr, job, version, index, why).await;
                return End::Over(why);
            }
        }
    }
    version.phase = VersionPhase::Done;
    if !keep(shared, pr, job, version).await {
        return End::Over(Why::Ended);
    }
    End::Done
}

/// The plan turn, then every diagram of the plan as a new version.
async fn draw_all(shared: &Arc<Shared>, pr: &PrRef, job: &Job) -> End {
    let earlier = versions_of(shared, pr);
    // The agent never sees the keywords entry: it would have to copy it, and it is not the
    // agent's to plan.
    let previous: Option<Vec<PlanEntry>> = earlier.last().map(|v| {
        v.entries
            .iter()
            .filter(|e| e.plan.kind != DiagramKind::Keywords)
            .map(|e| e.plan.clone())
            .collect()
    });
    let base = |forked: bool| plan_prompt(&job.context(forked), previous.as_deref());
    let plan = match ask_checked(shared, pr, job, None, &base, |text| {
        extract_plan(text).and_then(validate_plan)
    })
    .await
    {
        Ok(plan) => plan,
        Err(Halt::Over(why)) => return End::Over(why),
        Err(Halt::Failed(reason)) => {
            return End::Failed(format!(
                "{} could not plan the diagrams: {reason}",
                job.name
            ));
        }
    };
    let mut plan = plan;
    if job.keywords {
        plan.push(keywords_entry());
    }
    // `start` refused a run with no number left; a file written since then ends it here.
    let Some(number) = next_diagram_number(&shared.paths, pr) else {
        return End::Failed(NO_NUMBER_LEFT.to_string());
    };
    let mut version = DiagramVersion {
        number,
        commit: job.head.clone(),
        harness: job.name.clone(),
        drawn_at: now_unix(),
        phase: VersionPhase::Drawing,
        entries: plan
            .into_iter()
            .map(|plan| DiagramEntry {
                plan,
                state: EntryState::Waiting,
            })
            .collect(),
    };
    shared.diagrams.set_writing(pr, job.run, version.number);
    if !keep(shared, pr, job, &version).await {
        return End::Over(Why::Ended);
    }
    pin_version(shared, pr, version.number, &version.commit).await;
    shared.publish(
        topics::AGENT,
        Event::DiagramsPlanned {
            pr: pr.clone(),
            version: Box::new(version.clone()),
        },
    );
    let count = version.entries.len();
    draw_entries(shared, pr, job, &mut version, &earlier, 0..count).await
}

/// One entry of the latest version, drawn again.
async fn draw_one(shared: &Arc<Shared>, pr: &PrRef, job: &Job, key: &str) -> End {
    let mut earlier = versions_of(shared, pr);
    let Some(mut version) = earlier.pop().map(DiagramVersion::recovered) else {
        return End::Refused("There are no diagrams to draw again".to_string());
    };
    let Some(index) = version.entries.iter().position(|e| e.plan.key == key) else {
        return End::Refused(format!(
            "There is no diagram {} in the latest version",
            shown_key(key)
        ));
    };
    version.phase = VersionPhase::Drawing;
    shared.diagrams.set_writing(pr, job.run, version.number);
    if !settle_entry(shared, pr, job, &mut version, index, EntryState::Waiting).await {
        return End::Over(Why::Ended);
    }
    draw_entries(shared, pr, job, &mut version, &earlier, index..index + 1).await
}

/// Runs a job to its end, and settles it whatever happens inside.
async fn drive(_live: Live, shared: Arc<Shared>, pr: PrRef, job: Job) {
    let run = job.run;
    let task = tokio::spawn(execute(shared.clone(), pr.clone(), job));
    if task.await.is_err() {
        tracing::error!(pr = %pr, run, "a drawing failed inside the daemon");
        let message = "The drawing stopped unexpectedly".to_string();
        if shared.diagrams.end_run(&pr, run, Some(message.clone())) {
            tell_state(&shared, &pr, DiagramsState::Failed { message });
        }
    }
    shared.sessions.release_closed(&shared, &pr).await;
}

async fn execute(shared: Arc<Shared>, pr: PrRef, job: Job) {
    let end = match &job.mode {
        Mode::All => draw_all(&shared, &pr, &job).await,
        Mode::One(key) => draw_one(&shared, &pr, &job, key).await,
    };
    // A run that was replaced or forgotten while it wound down tells nothing: the state now
    // belongs to whatever came after it.
    match end {
        End::Done => {
            if shared.diagrams.end_run(&pr, job.run, None) {
                tell_state(&shared, &pr, DiagramsState::Idle);
            }
        }
        End::Failed(message) => {
            if shared.diagrams.end_run(&pr, job.run, Some(message.clone())) {
                tell_state(&shared, &pr, DiagramsState::Failed { message });
            }
        }
        End::Over(why) => {
            if shared.diagrams.end_run(&pr, job.run, None) && why != Why::Shutdown {
                tell_state(&shared, &pr, DiagramsState::Idle);
            }
        }
        End::Refused(message) => {
            tracing::warn!(pr = %pr, %message, "a diagram could not be drawn again");
            if shared.diagrams.end_run(&pr, job.run, None) {
                tell_state(&shared, &pr, DiagramsState::Idle);
            }
        }
    }
}

#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
fn check_redraw(shared: &Shared, pr: &PrRef, head: &str, key: &str) -> Result<(), Outcome> {
    let Some(latest) = versions_of(shared, pr).pop() else {
        return Err(reviews::invalid_state(
            "There are no diagrams to draw again",
        ));
    };
    if latest.entry(key).is_none() {
        return Err(Outcome::Err(ProtocolError::new(
            ErrorCode::NotFound,
            format!(
                "there is no diagram {} in the latest version",
                shown_key(key)
            ),
        )));
    }
    if latest.commit != head {
        return Err(reviews::invalid_state(
            "The pull request has new commits; draw the diagrams again",
        ));
    }
    Ok(())
}

/// Why a drawing cannot start once the last version number is taken.
const NO_NUMBER_LEFT: &str = "There is no version number left for these diagrams";

enum Refused {
    Busy,
    Closing,
}

/// Accepts a run for `pr` and starts it; it waits for a place in the background.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
async fn start(shared: &Arc<Shared>, pr: &PrRef, mode: Mode) -> Result<(), Outcome> {
    if shared.diagrams.closing.load(Ordering::SeqCst) {
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
    match &mode {
        Mode::One(key) => check_redraw(shared, pr, &review.head_sha, key)?,
        Mode::All if next_diagram_number(&shared.paths, pr).is_none() => {
            return Err(reviews::invalid_state(NO_NUMBER_LEFT));
        }
        Mode::All => {}
    }
    let (limit, keywords) = {
        let harness = &shared.config.read().await.harness;
        (
            Duration::from_secs(u64::from(harness.check_timeout_secs)),
            harness.keywords_3d,
        )
    };
    // Code links are kept only for the pull request's files, so a missing cache reads them
    // again; only when that fails too is every link dropped.
    let (pr_title, diffs) = match load_review_cache(&shared.paths, pr) {
        Ok(Some(cache)) => (cache.pr.summary.title, cache.files),
        _ => {
            let files = match reviews::files_for(shared, pr, &review.head_sha).await {
                Ok(files) => files.as_ref().clone(),
                Err(_) => {
                    tracing::warn!(pr = %pr, "the pull request's files could not be read; the diagrams get no code links");
                    Vec::new()
                }
            };
            (review.title.clone(), files)
        }
    };
    let files = diffs.iter().map(|file| file.path.clone()).collect();
    let added = added_of(&diffs);
    let first = match &mode {
        Mode::All => DiagramsState::Planning,
        Mode::One(key) => DiagramsState::Drawing {
            done: 0,
            total: 1,
            key: key.clone(),
            step: None,
        },
    };
    let stop = Stop::new();
    let run = shared.diagrams.runs.fetch_add(1, Ordering::SeqCst) + 1;
    let placed = shared.diagrams.with(pr, |entry| {
        if shared.diagrams.closing.load(Ordering::SeqCst) {
            return Err(Refused::Closing);
        }
        if entry.run.is_some() {
            return Err(Refused::Busy);
        }
        entry.failed = None;
        entry.awaiting = false;
        entry.run = Some(Run {
            id: run,
            stop: stop.clone(),
            state: first.clone(),
            writing: None,
        });
        Ok(Live::new(shared))
    });
    let live = match placed {
        Ok(live) => live,
        Err(Refused::Busy) => {
            return Err(reviews::invalid_state("Diagrams are already being drawn"));
        }
        Err(Refused::Closing) => return Err(reviews::invalid_state("clusiad is stopping")),
    };
    tell_state(shared, pr, first);
    let job = Job {
        run,
        mode,
        pr_title,
        files,
        keywords,
        added,
        head: review.head_sha.clone(),
        name: crate::sessions::name_for(shared, pr).await,
        limit,
        stop,
    };
    tokio::spawn(drive(live, shared.clone(), pr.clone(), job));
    Ok(())
}

/// `DrawDiagrams`: plans and draws the diagrams of the review.
pub(crate) async fn run(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    match start(shared, pr, Mode::All).await {
        Ok(()) => Outcome::Ok(Reply::Ack),
        Err(out) => out,
    }
}

/// `DrawDiagram`: draws one entry of the latest version again.
pub(crate) async fn run_one(shared: &Arc<Shared>, pr: &PrRef, key: &str) -> Outcome {
    match start(shared, pr, Mode::One(key.to_string())).await {
        Ok(()) => Outcome::Ok(Reply::Ack),
        Err(out) => out,
    }
}

/// `StopDiagrams`: stops the run, if one waits or runs, and forgets that one was to start when
/// the summary ends.
pub(crate) fn stop(shared: &Shared, pr: &PrRef) -> Outcome {
    let was_awaiting = shared.diagrams.with(pr, |entry| {
        let was = std::mem::take(&mut entry.awaiting);
        if let Some(run) = &entry.run {
            run.stop.fire(Why::User);
        }
        was && entry.run.is_none()
    });
    if was_awaiting {
        tell_state(shared, pr, DiagramsState::Idle);
    }
    Outcome::Ok(Reply::Ack)
}

/// The review ended or was left (closed, published, discarded): its run is stopped, for `why`,
/// and nothing more starts. The stored versions stay (`forget` deletes them). Waits until the
/// run is gone, so the caller may take the review lock after.
pub(crate) async fn stop_all(shared: &Shared, pr: &PrRef, why: Why) {
    shared.diagrams.with(pr, |entry| {
        entry.awaiting = false;
        if let Some(run) = &entry.run {
            run.stop.fire(why);
        }
    });
    let give_up = Instant::now() + STOP_WAIT;
    while shared.diagrams.running(pr) && Instant::now() < give_up {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Daemon shutdown: no new run, every run stopped, and a wait until none is left.
pub(crate) async fn shutdown(shared: &Shared) {
    let diagrams = &shared.diagrams;
    diagrams.closing.store(true, Ordering::SeqCst);
    {
        let mut table = diagrams.table.lock().unwrap_or_else(|p| p.into_inner());
        for entry in table.values_mut() {
            entry.awaiting = false;
            if let Some(run) = &entry.run {
                run.stop.fire(Why::Shutdown);
            }
        }
    }
    let mut live = diagrams.live.subscribe();
    let _ = tokio::time::timeout(SHUTDOWN_WAIT, live.wait_for(|n| *n == 0)).await;
}

/// The review was published or discarded: the run is stopped, the versions are deleted, and the
/// windows hear that nothing is drawn. Closing an empty review does not come here: its versions
/// stay until retention. The caller may hold the review lock.
pub(crate) fn forget(shared: &Shared, pr: &PrRef) {
    let removed = shared
        .diagrams
        .table
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(pr);
    if let Some(run) = removed.and_then(|entry| entry.run) {
        run.stop.fire(Why::Ended);
    }
    if let Err(e) = delete_diagrams(&shared.paths, pr) {
        tracing::warn!(error = %e, pr = %pr, "cannot delete the stored diagrams");
    }
    tell_state(shared, pr, DiagramsState::Idle);
}

/// What the diagrams do as the review opens. With `harness.diagrams` on and no version drawn
/// from this head, the run starts at once, or right after the summary turn when one runs
/// (`on_summary_over`). Says whether a run starts or waits.
///
/// Called by `reviews::open` while it holds the review lock; `on_summary_over` takes the same
/// lock before it reads `awaiting`, so a summary that ends between the check that one runs and
/// `awaiting` being set is not missed.
pub(crate) async fn on_open(
    shared: &Arc<Shared>,
    pr: &PrRef,
    head: &str,
    summarized: bool,
) -> bool {
    if !shared.config.read().await.harness.diagrams {
        return false;
    }
    if shared.diagrams.running(pr) {
        return true;
    }
    if versions_of(shared, pr).iter().any(|v| v.commit == head) {
        return false;
    }
    if summarized {
        shared.diagrams.with(pr, |entry| entry.awaiting = true);
        tell_state(shared, pr, DiagramsState::Planning);
        return true;
    }
    match start(shared, pr, Mode::All).await {
        Ok(()) => true,
        Err(_) => {
            tracing::warn!(pr = %pr, "could not draw diagrams as the review opened");
            false
        }
    }
}

/// The summary turn is over, whatever way it ended except with the review or the daemon (the chat
/// calls this for those only): the run that waited for it starts now.
pub(crate) async fn on_summary_over(shared: &Arc<Shared>, pr: &PrRef) {
    let waited = {
        let _guard = reviews::lock(shared, pr).await;
        shared
            .diagrams
            .with(pr, |entry| std::mem::take(&mut entry.awaiting))
    };
    if !waited {
        return;
    }
    // Switched off in Config while it waited: it does not start.
    let on = shared.config.read().await.harness.diagrams;
    if !on || start(shared, pr, Mode::All).await.is_err() {
        tracing::warn!(pr = %pr, "no diagrams were drawn after the summary");
        tell_state(shared, pr, DiagramsState::Idle);
    }
}

/// `GetDiagrams`: every version without its drawings, the latest in full, what the daemon does
/// now, and whether the latest was drawn from another commit than the review's.
pub(crate) fn get(shared: &Shared, pr: &PrRef) -> Outcome {
    let versions = versions_of(shared, pr);
    let review = reviews::load_stored(shared, pr).ok().flatten();
    let head = review
        .as_ref()
        .map(|r| r.head_sha.clone())
        .unwrap_or_default();
    let stale = !head.is_empty() && versions.last().is_some_and(|v| v.commit != head);
    // What the agent said when the revalidation round asked it, for the latest commits only.
    let stale_reason = review
        .and_then(|r| r.revalidation)
        .filter(|round| round.to_head == head)
        .and_then(|round| round.diagrams_stale)
        .map(|said| said.reason)
        .filter(|reason| stale && !reason.is_empty());
    let infos = versions.iter().map(DiagramVersion::info).collect();
    Outcome::Ok(Reply::Diagrams {
        versions: infos,
        latest: versions.last().cloned().map(Box::new),
        state: shared
            .diagrams
            .live_state(pr)
            .unwrap_or(DiagramsState::Idle),
        head,
        stale,
        stale_reason,
    })
}

/// Version `number`, read as `get` reads it.
pub(crate) fn stored_version(shared: &Shared, pr: &PrRef, number: u32) -> Option<DiagramVersion> {
    load_diagram_version(&shared.paths, pr, number).map(|v| as_shown(shared, pr, v))
}

/// The latest version as stored, read off the async threads; only its file is parsed.
async fn latest_stored(shared: &Shared, pr: &PrRef) -> Option<DiagramVersion> {
    let (paths, pr) = (shared.paths.clone(), pr.clone());
    tokio::task::spawn_blocking(move || load_latest_diagram_version(&paths, &pr))
        .await
        .ok()
        .flatten()
}

/// The commit the latest version was drawn from.
pub(crate) async fn latest_commit(shared: &Shared, pr: &PrRef) -> Option<String> {
    latest_stored(shared, pr)
        .await
        .map(|version| version.commit)
}

/// The ready diagrams of the latest version, with the files their boxes point at, for the
/// revalidation prompt; none when the review has no diagrams.
pub(crate) async fn digests(shared: &Shared, pr: &PrRef) -> Vec<clusia_core::DiagramDigest> {
    let Some(latest) = latest_stored(shared, pr).await else {
        return Vec::new();
    };
    latest
        .entries
        .iter()
        .filter_map(|entry| {
            let EntryState::Ready { spec, .. } = &entry.state else {
                return None;
            };
            let mut paths: Vec<String> = spec
                .nodes
                .iter()
                .filter_map(|node| node.anchor.as_ref().map(|a| a.path.clone()))
                .collect();
            paths.sort();
            paths.dedup();
            Some(clusia_core::DiagramDigest {
                key: entry.plan.key.clone(),
                title: entry.plan.title.clone(),
                kind: entry.plan.kind,
                paths,
            })
        })
        .collect()
}

/// `GetDiagramVersion`: one version in full.
pub(crate) fn version(shared: &Shared, pr: &PrRef, number: u32) -> Outcome {
    match stored_version(shared, pr, number) {
        Some(version) => Outcome::Ok(Reply::DiagramVersion(Box::new(version))),
        None => Outcome::Err(ProtocolError::new(
            ErrorCode::NotFound,
            format!("there is no version {number} of the diagrams"),
        )),
    }
}

/// The longest range `lines_at_head` follows line by line.
const MAX_MAPPED_LINES: u32 = 5_000;

/// Where lines `anchor.start_line..=anchor.end_line` of `commit` are at `head`: the path and the
/// first and last line. Every line of the range is followed: `None` when one was changed or
/// deleted, when they no longer fall one after the other in one file (a line was inserted inside
/// the range), when the range is very long, or when the commits cannot be compared.
async fn lines_at_head(
    shared: &Shared,
    pr: &PrRef,
    commit: &str,
    head: &str,
    anchor: &clusia_core::diagram::CodeAnchor,
) -> Option<(String, u32, u32)> {
    if commit == head {
        return Some((anchor.path.clone(), anchor.start_line, anchor.end_line));
    }
    let repo = repo_of_worktree(&shared.paths.worktree_for(pr))
        .await
        .ok()?;
    let map = DiffMap::parse(&diff_between(&repo, commit, head).await.ok()?);
    let span = anchor.end_line.checked_sub(anchor.start_line)?;
    if span >= MAX_MAPPED_LINES {
        return None;
    }
    let mut start: Option<(String, u32)> = None;
    for line in anchor.start_line..=anchor.end_line {
        let (path, to) = match map.map_line(&anchor.path, line) {
            LineMap::Same => (anchor.path.clone(), line),
            LineMap::Moved { path, line } => (path, line),
            LineMap::Changed | LineMap::Deleted => return None,
        };
        // Every line must still be there, in the same file and in the same order, one after the
        // other: a line rewritten or an insertion inside the range is not the code the reviewer
        // saw.
        match &start {
            None => start = Some((path, to)),
            Some((first_path, first)) => {
                if *first_path != path || to != first + (line - anchor.start_line) {
                    return None;
                }
            }
        }
    }
    let (path, first) = start?;
    Some((path, first, first + span))
}

/// `AddDiagramComment`: the comment on a box becomes a draft item written by the diagram. A box
/// whose lines are still in the pull request's diff at the review's head (mapped forward from the
/// version's commit when the head moved) gets a line comment there. Any other box, and a box with
/// no code, gets a general comment, which names `path:lines` when the box has code. The anchor is
/// the stored one, never the client's.
pub(crate) async fn add_comment(
    shared: &Shared,
    client: &str,
    pr: &PrRef,
    number: u32,
    key: &str,
    node: &str,
    body: &str,
) -> Outcome {
    let comment = body.trim();
    if comment.is_empty() {
        return reviews::bad_request("the comment is empty");
    }
    // The git diff and a GitHub read of the files run without the review's lock; only the
    // write takes it, and a head that moved meanwhile sends the comment to the review body.
    let (commit, anchor) = match crate::diagram_code::anchored_node(shared, pr, number, key, node) {
        Ok(found) => found,
        Err(out) => return out,
    };
    let Some(anchor) = anchor else {
        let _guard = reviews::lock(shared, pr).await;
        return add_general(shared, client, pr, comment).await;
    };
    let head = match reviews::load_existing(shared, pr) {
        Ok(review) => review.head_sha,
        Err(out) => return out,
    };
    let placed = match lines_at_head(shared, pr, &commit, &head, &anchor).await {
        Some(lines) => reviews::files_for(shared, pr, &head)
            .await
            .ok()
            .map(|files| (lines, files)),
        None => None,
    };
    let on_lines = placed.and_then(|((path, start, end), files)| {
        let at = Anchor {
            path: path.clone(),
            line: end,
            start_line: (start != end).then_some(start),
            side: Side::Right,
            commit: head.clone(),
        };
        fits_diff(&files, &at).then_some(AnchorInput {
            path,
            line: end,
            start_line: at.start_line,
            side: Side::Right,
        })
    });
    let _guard = reviews::lock(shared, pr).await;
    let head_now = match reviews::load_existing(shared, pr) {
        Ok(review) => review.head_sha,
        Err(out) => return out,
    };
    if let Some(anchor) = on_lines
        && head_now == head
    {
        let item = reviews::NewItem {
            kind: DraftKind::LineComment,
            anchor: Some(anchor),
            thread: None,
            body: comment,
        };
        return reviews::add_item_locked(shared, client, pr, Origin::Diagram, item).await;
    }
    let lines = if anchor.start_line == anchor.end_line {
        anchor.start_line.to_string()
    } else {
        format!("{}-{}", anchor.start_line, anchor.end_line)
    };
    let text = format!("{}:{lines}: {comment}", printable(&anchor.path));
    add_general(shared, client, pr, &text).await
}

async fn add_general(shared: &Shared, client: &str, pr: &PrRef, text: &str) -> Outcome {
    let item = reviews::NewItem {
        kind: DraftKind::General,
        anchor: None,
        thread: None,
        body: text,
    };
    reviews::add_item_locked(shared, client, pr, Origin::Diagram, item).await
}

#[cfg(test)]
mod tests {
    use clusia_core::checks::CheckKind;
    use clusia_core::diagram::REPAIR_ECHO_MAX;
    use clusia_core::keywords::KEYWORDS_TITLE;
    use clusia_harness::testkit::{
        Call, FakeClaude, Script, Turn, diagram_block, diagram_plan_block, keyword_placement_block,
        keyword_plan_block,
    };
    use clusia_store::save_review;
    use serde_json::json;

    use super::*;
    use crate::checks::tests::lab;

    fn pr(n: u64) -> PrRef {
        format!("acme/widgets#{n}").parse().unwrap()
    }

    fn plan_turn(keys: &[&str]) -> Turn {
        let entries: Vec<_> = keys
            .iter()
            .map(|key| {
                json!({
                    "key": key, "kind": "flow",
                    "title": format!("The {key}"), "purpose": "Shows what changed"
                })
            })
            .collect();
        Turn::answer_blocks("Plan.", &[diagram_plan_block(json!(entries))])
    }

    fn drawing(key: &str, caption: &str) -> Turn {
        Turn::answer_blocks(
            "Drawn.",
            &[diagram_block(json!({
                "key": key, "kind": "flow", "title": format!("The {key}"), "caption": caption,
                "nodes": [
                    {"id": "a", "label": "refresh()", "shape": "box"},
                    {"id": "b", "label": "store.save()", "shape": "box"}
                ],
                "edges": [{"from": "a", "to": "b"}]
            }))],
        )
    }

    fn draw_turn(key: &str) -> Turn {
        drawing(key, "first drawing")
    }

    async fn calls(fake: &tempfile::TempDir, count: usize) -> Vec<Call> {
        let dir = fake.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            FakeClaude::wait_for_calls(&dir, count, Duration::from_secs(5))
        })
        .await
        .unwrap()
    }

    /// The prompt a recorded call was given.
    fn prompt_of(call: &Call) -> String {
        let at = call.argv.iter().position(|a| a == "-p").expect("a prompt");
        call.argv[at + 1].clone()
    }

    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only asks whether the process exists.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    /// Waits until the review has no run.
    async fn settled(shared: &Shared, n: u64) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while shared.diagrams.running(&pr(n)) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the run ends");
    }

    fn latest(shared: &Shared, n: u64) -> DiagramVersion {
        versions_of(shared, &pr(n)).pop().expect("a version")
    }

    fn states(version: &DiagramVersion) -> Vec<&'static str> {
        version
            .entries
            .iter()
            .map(|e| match &e.state {
                EntryState::Waiting => "waiting",
                EntryState::Drawing => "drawing",
                EntryState::Ready { .. } | EntryState::ReadyMap { .. } => "ready",
                EntryState::Failed { .. } => "failed",
                EntryState::NotDrawn => "not drawn",
            })
            .collect()
    }

    async fn drawn(
        script: Script,
        keys: &[&str],
    ) -> (tempfile::TempDir, tempfile::TempDir, Arc<Shared>) {
        let (home, fake, shared) = lab(script, 1, |_| {});
        assert!(matches!(
            run(&shared, &pr(1)).await,
            Outcome::Ok(Reply::Ack)
        ));
        settled(&shared, 1).await;
        assert_eq!(latest(&shared, 1).entries.len(), keys.len());
        (home, fake, shared)
    }

    fn set_head(shared: &Shared, n: u64, head: &str) {
        let mut review = reviews::load_stored(shared, &pr(n)).unwrap().unwrap();
        review.head_sha = head.into();
        save_review(&shared.paths, &review).unwrap();
    }

    #[tokio::test]
    async fn a_plan_then_one_turn_per_diagram() {
        let script = Script::turns(vec![
            plan_turn(&["flow", "graph"]),
            draw_turn("flow"),
            draw_turn("graph"),
        ]);
        let (_home, fake, shared) = drawn(script, &["flow", "graph"]).await;
        let version = latest(&shared, 1);
        assert_eq!(version.number, 1);
        assert_eq!(version.commit, "h");
        assert_eq!(version.harness, "Claude Code");
        assert_eq!(version.phase, VersionPhase::Done);
        assert_eq!(states(&version), ["ready", "ready"]);
        assert_eq!(FakeClaude::calls(fake.path()).len(), 3);
        assert_eq!(shared.diagrams.live_state(&pr(1)), None);
    }

    #[tokio::test]
    async fn the_draw_prompts_carry_their_entry_and_the_plan_prompt_none() {
        let script = Script::turns(vec![
            plan_turn(&["flow", "graph"]),
            draw_turn("flow"),
            draw_turn("graph"),
        ]);
        let (_home, fake, _shared) = drawn(script, &["flow", "graph"]).await;
        let calls = FakeClaude::calls(fake.path());
        let prompts: Vec<String> = calls.iter().map(prompt_of).collect();
        assert!(prompts[1].contains("flow"), "{}", prompts[1]);
        assert!(prompts[2].contains("graph"), "{}", prompts[2]);
    }

    #[tokio::test]
    async fn every_turn_of_a_drawing_is_logged_apart_from_the_chat() {
        let script = Script::turns(vec![plan_turn(&["flow"]), draw_turn("flow")]);
        let (_home, _fake, shared) = drawn(script, &["flow"]).await;
        let entries = agent_log::read(&shared.paths.agent_log(&pr(1)));
        let runs: Vec<(String, Option<String>)> = entries
            .iter()
            .filter_map(|e| match e {
                AgentLogEntry::Diagrams { state, key, .. } => Some((state.clone(), key.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            runs,
            [
                ("waiting".to_string(), None),
                ("running".to_string(), None),
                ("done".to_string(), None),
                ("waiting".to_string(), Some("flow".to_string())),
                ("running".to_string(), Some("flow".to_string())),
                ("done".to_string(), Some("flow".to_string())),
            ]
        );
        assert_eq!(agent_log::check_turns(&entries).len(), 2);
        assert!(agent_log::unfinished_turns(&entries).is_empty());
        assert_eq!(agent_log::last_summary(&entries), None);
    }

    #[tokio::test]
    async fn an_unreadable_drawing_gets_one_repair_turn_that_holds_the_bad_answer() {
        let script = Script::turns(vec![
            plan_turn(&["flow"]),
            Turn::answer("This is not a diagram block."),
            draw_turn("flow"),
        ]);
        let (_home, fake, shared) = drawn(script, &["flow"]).await;
        assert_eq!(states(&latest(&shared, 1)), ["ready"]);
        let calls = FakeClaude::calls(fake.path());
        assert_eq!(calls.len(), 3);
        assert!(
            prompt_of(&calls[2]).contains("This is not a diagram block."),
            "the repair turn is given the answer it repairs"
        );
    }

    #[tokio::test]
    async fn a_repair_prompt_says_when_the_answer_it_holds_was_cut() {
        let long = format!("{}{}", "x".repeat(REPAIR_ECHO_MAX + 10), " no block");
        let script = Script::turns(vec![
            plan_turn(&["flow"]),
            Turn::answer(&long),
            draw_turn("flow"),
        ]);
        let (_home, fake, _shared) = drawn(script, &["flow"]).await;
        let calls = FakeClaude::calls(fake.path());
        assert!(
            prompt_of(&calls[2]).contains(", cut"),
            "{}",
            prompt_of(&calls[2])
        );
    }

    #[tokio::test]
    async fn a_drawing_that_stays_unreadable_fails_only_its_entry() {
        let script = Script::turns(vec![
            plan_turn(&["flow", "graph"]),
            Turn::answer("no block"),
            Turn::answer("still no block"),
            draw_turn("graph"),
        ]);
        let (_home, fake, shared) = drawn(script, &["flow", "graph"]).await;
        let version = latest(&shared, 1);
        assert_eq!(states(&version), ["failed", "ready"]);
        let EntryState::Failed { reason } = &version.entries[0].state else {
            panic!("failed");
        };
        assert!(!reason.is_empty() && reason.chars().count() <= REASON_MAX);
        assert_eq!(version.phase, VersionPhase::Done);
        assert_eq!(
            FakeClaude::calls(fake.path()).len(),
            4,
            "one repair, not two"
        );
    }

    #[tokio::test]
    async fn a_drawing_for_another_key_is_repaired_then_failed() {
        let script = Script::turns(vec![
            plan_turn(&["flow"]),
            draw_turn("other"),
            draw_turn("other"),
        ]);
        let (_home, _fake, shared) = drawn(script, &["flow"]).await;
        assert_eq!(states(&latest(&shared, 1)), ["failed"]);
    }

    #[tokio::test]
    async fn a_plan_that_stays_unreadable_keeps_no_version_and_says_why() {
        let (_home, fake, shared) = lab(Script::one(Turn::answer("I cannot plan.")), 1, |_| {});
        let _ = run(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        assert_eq!(FakeClaude::calls(fake.path()).len(), 2, "one repair");
        assert!(load_diagram_versions(&shared.paths, &pr(1)).is_empty());
        let Some(DiagramsState::Failed { message }) = shared.diagrams.live_state(&pr(1)) else {
            panic!("the plan failed");
        };
        assert!(
            message.starts_with("Claude Code could not plan the diagrams"),
            "{message}"
        );
        let Outcome::Ok(Reply::Diagrams {
            state, versions, ..
        }) = get(&shared, &pr(1))
        else {
            panic!("diagrams");
        };
        assert!(versions.is_empty());
        assert!(matches!(state, DiagramsState::Failed { .. }));
    }

    #[tokio::test]
    async fn a_new_draw_forgets_the_failed_plan() {
        let script = Script::turns(vec![
            Turn::answer("no plan"),
            Turn::answer("no plan either"),
            plan_turn(&["flow"]),
            draw_turn("flow"),
        ]);
        let (_home, _fake, shared) = lab(script, 1, |_| {});
        let _ = run(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        assert!(shared.diagrams.live_state(&pr(1)).is_some());
        let _ = run(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        assert_eq!(shared.diagrams.live_state(&pr(1)), None);
        assert_eq!(states(&latest(&shared, 1)), ["ready"]);
    }

    #[tokio::test]
    async fn a_second_draw_is_refused_while_one_runs() {
        let script = Script::turns(vec![
            plan_turn(&["flow"]),
            draw_turn("flow"),
            plan_turn(&["flow"]),
            Turn::hanging(),
        ]);
        let (_home, fake, shared) = drawn(script, &["flow"]).await;
        let _ = run(&shared, &pr(1)).await;
        calls(&fake, 4).await;
        let Outcome::Err(refusal) = run(&shared, &pr(1)).await else {
            panic!("refused");
        };
        assert_eq!(refusal.code, ErrorCode::InvalidState);
        assert_eq!(refusal.message, "Diagrams are already being drawn");
        let Outcome::Err(again) = run_one(&shared, &pr(1), "flow").await else {
            panic!("refused");
        };
        assert_eq!(again.code, ErrorCode::InvalidState);
        assert_eq!(again.message, "Diagrams are already being drawn");
        shutdown(&shared).await;
    }

    #[tokio::test]
    async fn stopping_during_the_plan_leaves_nothing() {
        let (_home, fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        let _ = run(&shared, &pr(1)).await;
        let pid = calls(&fake, 1).await[0].pid;
        assert!(matches!(stop(&shared, &pr(1)), Outcome::Ok(Reply::Ack)));
        settled(&shared, 1).await;
        assert!(load_diagram_versions(&shared.paths, &pr(1)).is_empty());
        assert_eq!(shared.diagrams.live_state(&pr(1)), None);
        assert!(!alive(pid), "no process is left behind");
    }

    #[tokio::test]
    async fn stopping_between_entries_marks_the_rest_not_drawn_and_retry_works() {
        let script = Script::turns(vec![
            plan_turn(&["flow", "graph", "states"]),
            draw_turn("flow"),
            Turn::hanging(),
            drawing("graph", "retried"),
        ]);
        let (_home, fake, shared) = lab(script, 1, |_| {});
        let _ = run(&shared, &pr(1)).await;
        let pid = calls(&fake, 3).await[2].pid;
        let _ = stop(&shared, &pr(1));
        settled(&shared, 1).await;
        let stored = load_diagram_versions(&shared.paths, &pr(1));
        assert_eq!(stored.len(), 1, "the file stays readable");
        assert_eq!(states(&stored[0]), ["ready", "not drawn", "not drawn"]);
        assert_eq!(stored[0].phase, VersionPhase::Done);
        assert!(!alive(pid));
        // Retry draws the entry again.
        assert!(matches!(
            run_one(&shared, &pr(1), "graph").await,
            Outcome::Ok(Reply::Ack)
        ));
        settled(&shared, 1).await;
        assert_eq!(states(&latest(&shared, 1)), ["ready", "ready", "not drawn"]);
        assert_eq!(latest(&shared, 1).number, 1, "a retry is not a new version");
    }

    #[tokio::test]
    async fn a_shutdown_stops_the_run_and_the_version_stays_readable() {
        let script = Script::turns(vec![plan_turn(&["flow", "graph"]), Turn::hanging()]);
        let (_home, fake, shared) = lab(script, 1, |_| {});
        let _ = run(&shared, &pr(1)).await;
        let pid = calls(&fake, 2).await[1].pid;
        shutdown(&shared).await;
        assert!(!shared.diagrams.running(&pr(1)));
        assert!(!alive(pid), "no process is left behind");
        let stored = load_diagram_versions(&shared.paths, &pr(1));
        assert_eq!(states(&stored[0]), ["not drawn", "not drawn"]);
        let Outcome::Err(refusal) = run(&shared, &pr(1)).await else {
            panic!("refused");
        };
        assert_eq!(refusal.message, "clusiad is stopping");
    }

    #[tokio::test]
    async fn a_review_that_ends_leaves_no_version_behind() {
        let script = Script::turns(vec![plan_turn(&["flow", "graph"]), Turn::hanging()]);
        let (_home, fake, shared) = lab(script, 1, |_| {});
        let _ = run(&shared, &pr(1)).await;
        calls(&fake, 2).await;
        stop_all(&shared, &pr(1), Why::Ended).await;
        forget(&shared, &pr(1));
        assert!(!shared.diagrams.running(&pr(1)));
        assert!(load_diagram_versions(&shared.paths, &pr(1)).is_empty());
    }

    #[tokio::test]
    async fn a_run_that_the_review_outlives_keeps_its_version_when_closed() {
        let script = Script::turns(vec![plan_turn(&["flow"]), Turn::hanging()]);
        let (_home, fake, shared) = lab(script, 1, |_| {});
        let _ = run(&shared, &pr(1)).await;
        calls(&fake, 2).await;
        stop_all(&shared, &pr(1), Why::User).await;
        assert_eq!(
            states(&load_diagram_versions(&shared.paths, &pr(1))[0]),
            ["not drawn"]
        );
    }

    #[tokio::test]
    async fn a_turn_past_its_limit_fails_its_entry_without_a_repair() {
        let script = Script::turns(vec![plan_turn(&["flow"]), Turn::hanging()]);
        let (_home, fake, shared) = lab(script, 1, |config| {
            config.harness.check_timeout_secs = 1;
        });
        let _ = run(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        let version = latest(&shared, 1);
        let EntryState::Failed { reason } = &version.entries[0].state else {
            panic!("failed");
        };
        assert_eq!(reason, "The turn ran past its limit of 1s and was stopped");
        assert_eq!(FakeClaude::calls(fake.path()).len(), 2);
    }

    #[tokio::test]
    async fn the_queue_is_shared_with_the_checks() {
        let (_home, fake, shared) = lab(Script::one(Turn::hanging()), 3, |_| {});
        for n in 1..=2 {
            let _ = crate::checks::run(&shared, &pr(n), CheckKind::Security).await;
        }
        calls(&fake, 2).await;
        let _ = run(&shared, &pr(3)).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            FakeClaude::calls(fake.path()).len(),
            2,
            "the plan waits for a place"
        );
        assert_eq!(
            shared.diagrams.live_state(&pr(3)),
            Some(DiagramsState::Planning)
        );
        let _ = crate::checks::stop(&shared, &pr(1), CheckKind::Security);
        assert_eq!(
            calls(&fake, 3).await.len(),
            3,
            "a free place starts the plan"
        );
        let _ = stop(&shared, &pr(3));
        settled(&shared, 3).await;
        crate::checks::shutdown(&shared).await;
    }

    #[tokio::test]
    async fn a_redraw_replaces_one_entry_of_the_latest_version() {
        let script = Script::turns(vec![
            plan_turn(&["flow", "graph"]),
            draw_turn("flow"),
            draw_turn("graph"),
            drawing("flow", "second drawing"),
        ]);
        let (_home, fake, shared) = drawn(script, &["flow", "graph"]).await;
        assert!(matches!(
            run_one(&shared, &pr(1), "flow").await,
            Outcome::Ok(Reply::Ack)
        ));
        settled(&shared, 1).await;
        let version = latest(&shared, 1);
        assert_eq!(version.number, 1);
        let caption = |i: usize| match &version.entries[i].state {
            EntryState::Ready { spec, .. } => spec.caption.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(caption(0), "second drawing");
        assert_eq!(caption(1), "first drawing");
        assert_eq!(FakeClaude::calls(fake.path()).len(), 4);
    }

    #[tokio::test]
    async fn a_new_version_draws_with_the_previous_drawing_of_the_same_key() {
        let script = Script::turns(vec![
            plan_turn(&["flow"]),
            draw_turn("flow"),
            plan_turn(&["flow"]),
            drawing("flow", "second drawing"),
        ]);
        let (_home, fake, shared) = drawn(script, &["flow"]).await;
        let _ = run(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        let version = latest(&shared, 1);
        assert_eq!(version.number, 2);
        let prompts: Vec<String> = FakeClaude::calls(fake.path())
            .iter()
            .map(prompt_of)
            .collect();
        assert!(
            !prompts[1].contains("first drawing"),
            "the first drawing has no previous"
        );
        assert!(
            prompts[3].contains("first drawing"),
            "the second one is given the first: {}",
            prompts[3]
        );
        assert_eq!(load_diagram_versions(&shared.paths, &pr(1)).len(), 2);
    }

    #[tokio::test]
    async fn a_redraw_names_what_it_cannot_do() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        let Outcome::Err(none) = run_one(&shared, &pr(1), "flow").await else {
            panic!("refused");
        };
        assert_eq!(none.code, ErrorCode::InvalidState);
        let script = Script::turns(vec![plan_turn(&["flow"]), draw_turn("flow")]);
        let (_home, _fake, shared) = drawn(script, &["flow"]).await;
        let Outcome::Err(unknown) = run_one(&shared, &pr(1), "nope").await else {
            panic!("refused");
        };
        assert_eq!(unknown.code, ErrorCode::NotFound);
        set_head(&shared, 1, "h2");
        let Outcome::Err(moved) = run_one(&shared, &pr(1), "flow").await else {
            panic!("refused");
        };
        assert_eq!(moved.code, ErrorCode::InvalidState);
        assert!(moved.message.contains("new commits"), "{}", moved.message);
    }

    #[tokio::test]
    async fn new_commits_during_a_run_leave_the_rest_not_drawn() {
        let script = Script::turns(vec![
            plan_turn(&["flow", "graph"]),
            draw_turn("flow").delay_ms(300),
            draw_turn("graph"),
        ]);
        let (_home, fake, shared) = lab(script, 1, |_| {});
        let _ = run(&shared, &pr(1)).await;
        calls(&fake, 2).await;
        set_head(&shared, 1, "h2");
        settled(&shared, 1).await;
        assert_eq!(states(&latest(&shared, 1)), ["ready", "not drawn"]);
        assert_eq!(FakeClaude::calls(fake.path()).len(), 2);
    }

    #[tokio::test]
    async fn a_version_a_crash_left_unfinished_reads_as_not_drawn_and_retry_works() {
        let script = Script::one(drawing("graph", "after the crash").delay_ms(400));
        let (_home, fake, shared) = lab(script, 1, |_| {});
        let plan = |key: &str| PlanEntry {
            key: key.into(),
            kind: clusia_core::diagram::DiagramKind::Flow,
            title: format!("The {key}"),
            purpose: "p".into(),
        };
        let crashed = DiagramVersion {
            number: 4,
            commit: "h".into(),
            harness: "Claude Code".into(),
            drawn_at: 1,
            phase: VersionPhase::Drawing,
            entries: vec![
                DiagramEntry {
                    plan: plan("flow"),
                    state: EntryState::Drawing,
                },
                DiagramEntry {
                    plan: plan("graph"),
                    state: EntryState::Waiting,
                },
            ],
        };
        save_diagram_version(&shared.paths, &pr(1), &crashed).unwrap();
        let Outcome::Ok(Reply::Diagrams { latest, state, .. }) = get(&shared, &pr(1)) else {
            panic!("diagrams");
        };
        let latest = latest.expect("the version");
        assert_eq!(latest.phase, VersionPhase::Done);
        assert_eq!(states(&latest), ["not drawn", "not drawn"]);
        assert_eq!(state, DiagramsState::Idle);
        let Outcome::Ok(Reply::DiagramVersion(one)) = version(&shared, &pr(1), 4) else {
            panic!("a version");
        };
        assert_eq!(states(&one), ["not drawn", "not drawn"]);
        assert_eq!(
            load_diagram_versions(&shared.paths, &pr(1))[0].phase,
            VersionPhase::Drawing,
            "reading never rewrites the file"
        );
        let _ = run_one(&shared, &pr(1), "graph").await;
        // While the retry runs, the entry it draws is on disk as it is drawn and the one it does
        // not touch still reads as not drawn, not as a second drawing.
        calls(&fake, 1).await;
        let Outcome::Ok(Reply::Diagrams { latest, state, .. }) = get(&shared, &pr(1)) else {
            panic!("diagrams");
        };
        assert_eq!(
            states(&latest.expect("the version")),
            ["not drawn", "drawing"]
        );
        assert!(matches!(state, DiagramsState::Drawing { .. }));
        settled(&shared, 1).await;
        assert_eq!(states(&latest_of(&shared)), ["not drawn", "ready"]);
        assert_eq!(latest_of(&shared).number, 4);
        let stored = load_diagram_versions(&shared.paths, &pr(1));
        assert_eq!(
            states(&stored[0]),
            ["not drawn", "ready"],
            "the file is cleaned up"
        );
        assert_eq!(stored[0].phase, VersionPhase::Done);
    }

    #[tokio::test]
    async fn a_failure_the_harness_reports_is_cut_and_printable_like_any_reason() {
        let long = format!("x\\u001b[2J{}", "y".repeat(5_000));
        let failing = Turn::lines(&[&format!(
            r#"{{"type":"result","subtype":"success","is_error":true,"duration_ms":1,"result":"{long}"}}"#
        )]);
        let script = Script::turns(vec![plan_turn(&["flow"]), failing.clone()]);
        let (_home, fake, shared) = lab(script, 1, |_| {});
        let _ = run(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        let version = latest(&shared, 1);
        let EntryState::Failed { reason } = &version.entries[0].state else {
            panic!("failed");
        };
        assert!(reason.chars().count() <= REASON_MAX, "{}", reason.len());
        assert!(
            !reason.contains('\u{1b}') && reason.starts_with("x\\u{1b}[2J"),
            "{reason}"
        );
        assert_eq!(
            FakeClaude::calls(fake.path()).len(),
            2,
            "a harness failure is not repaired"
        );

        // The same for a plan the harness fails.
        let (_home, _fake, shared) = lab(Script::one(failing), 1, |_| {});
        let _ = run(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        let Some(DiagramsState::Failed { message }) = shared.diagrams.live_state(&pr(1)) else {
            panic!("the plan failed");
        };
        assert!(
            message.chars().count() <= REASON_MAX + 80,
            "{}",
            message.len()
        );
        assert!(!message.contains('\u{1b}'), "{message}");
    }

    #[tokio::test]
    async fn stopping_during_a_repair_turn_leaves_the_entry_not_drawn_and_no_process() {
        let script = Script::turns(vec![
            plan_turn(&["flow", "graph"]),
            Turn::answer("no block"),
            Turn::hanging(),
        ]);
        let (_home, fake, shared) = lab(script, 1, |_| {});
        let _ = run(&shared, &pr(1)).await;
        let repair = calls(&fake, 3).await.remove(2);
        assert!(
            prompt_of(&repair).contains("no block"),
            "it is the repair turn"
        );
        let _ = stop(&shared, &pr(1));
        settled(&shared, 1).await;
        let stored = load_diagram_versions(&shared.paths, &pr(1));
        assert_eq!(states(&stored[0]), ["not drawn", "not drawn"]);
        assert_eq!(stored[0].phase, VersionPhase::Done);
        assert!(!alive(repair.pid), "no process is left behind");
    }

    #[tokio::test]
    async fn a_shutdown_during_the_plan_keeps_no_version_and_no_process() {
        let (_home, fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        let _ = run(&shared, &pr(1)).await;
        let pid = calls(&fake, 1).await[0].pid;
        shutdown(&shared).await;
        assert!(!shared.diagrams.running(&pr(1)));
        assert!(!alive(pid), "no process is left behind");
        assert!(load_diagram_versions(&shared.paths, &pr(1)).is_empty());
    }

    fn latest_of(shared: &Shared) -> DiagramVersion {
        latest(shared, 1)
    }

    #[tokio::test]
    async fn get_says_where_the_latest_stands() {
        let script = Script::turns(vec![plan_turn(&["flow"]), draw_turn("flow")]);
        let (_home, _fake, shared) = drawn(script, &["flow"]).await;
        let Outcome::Ok(Reply::Diagrams {
            versions,
            latest,
            state,
            head,
            stale,
            ..
        }) = get(&shared, &pr(1))
        else {
            panic!("diagrams");
        };
        assert_eq!(versions.len(), 1);
        assert_eq!(latest.map(|v| v.number), Some(1));
        assert_eq!(
            (state, head.as_str(), stale),
            (DiagramsState::Idle, "h", false)
        );
        set_head(&shared, 1, "h2");
        let Outcome::Ok(Reply::Diagrams { head, stale, .. }) = get(&shared, &pr(1)) else {
            panic!("diagrams");
        };
        assert_eq!((head.as_str(), stale), ("h2", true));
        let Outcome::Err(missing) = version(&shared, &pr(1), 9) else {
            panic!("missing");
        };
        assert_eq!(missing.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn a_review_that_is_not_open_or_not_active_cannot_be_drawn() {
        let (_home, fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        let Outcome::Err(refusal) = run(&shared, &pr(9)).await else {
            panic!("refused");
        };
        assert_eq!(refusal.code, ErrorCode::InvalidState);
        assert!(refusal.message.contains("open it first"));
        let mut review = reviews::load_stored(&shared, &pr(1)).unwrap().unwrap();
        review.state = ReviewState::Saved;
        save_review(&shared.paths, &review).unwrap();
        assert!(matches!(run(&shared, &pr(1)).await, Outcome::Err(_)));
        assert!(FakeClaude::calls(fake.path()).is_empty());
    }

    #[tokio::test]
    async fn opening_waits_for_the_summary_and_the_summary_ending_starts_the_run() {
        let script = Script::turns(vec![plan_turn(&["flow"]), draw_turn("flow")]);
        let (_home, fake, shared) = lab(script, 1, |config| config.harness.diagrams = true);
        assert!(on_open(&shared, &pr(1), "h", true).await);
        assert_eq!(
            shared.diagrams.live_state(&pr(1)),
            Some(DiagramsState::Planning)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            FakeClaude::calls(fake.path()).is_empty(),
            "it waits for the summary"
        );
        on_summary_over(&shared, &pr(1)).await;
        calls(&fake, 1).await;
        settled(&shared, 1).await;
        assert_eq!(states(&latest(&shared, 1)), ["ready"]);
        // A version for this head exists: opening again draws nothing.
        assert!(!on_open(&shared, &pr(1), "h", false).await);
    }

    #[tokio::test]
    async fn opening_without_a_summary_starts_at_once_and_only_when_it_is_on() {
        let script = Script::turns(vec![plan_turn(&["flow"]), draw_turn("flow")]);
        let (_home, fake, shared) = lab(script, 1, |_| {});
        assert!(
            !on_open(&shared, &pr(1), "h", false).await,
            "off by default"
        );
        shared.config.write().await.harness.diagrams = true;
        assert!(on_open(&shared, &pr(1), "h", false).await);
        calls(&fake, 1).await;
        // A run already going is left alone.
        assert!(on_open(&shared, &pr(1), "h", false).await);
        settled(&shared, 1).await;
        assert_eq!(FakeClaude::calls(fake.path()).len(), 2);
        // A version drawn from another head does not count.
        set_head(&shared, 1, "h2");
        assert!(on_open(&shared, &pr(1), "h2", true).await);
        let _ = stop(&shared, &pr(1));
        assert_eq!(shared.diagrams.live_state(&pr(1)), None);
    }

    #[tokio::test]
    async fn stopping_or_switching_off_while_it_waits_for_the_summary_starts_nothing() {
        let (_home, fake, shared) = lab(Script::one(Turn::hanging()), 1, |config| {
            config.harness.diagrams = true;
        });
        assert!(on_open(&shared, &pr(1), "h", true).await);
        let _ = stop(&shared, &pr(1));
        on_summary_over(&shared, &pr(1)).await;
        assert!(on_open(&shared, &pr(1), "h", true).await);
        shared.config.write().await.harness.diagrams = false;
        on_summary_over(&shared, &pr(1)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(FakeClaude::calls(fake.path()).is_empty());
        assert_eq!(shared.diagrams.live_state(&pr(1)), None);
    }

    #[test]
    fn a_reason_is_one_printable_line_of_bounded_length() {
        let long = "x".repeat(500);
        assert_eq!(reason_of(&[long]).chars().count(), REASON_MAX);
        assert_eq!(
            reason_of(&["a\u{1b}[2J\n\u{202e}b".into()]),
            "a\\u{1b}[2J\\n\\u{202e}b"
        );
        assert_eq!(reason_of(&[]), "The answer was not usable");
    }

    #[test]
    fn the_previous_drawing_is_the_latest_earlier_one_that_was_ready() {
        let spec = |caption: &str| {
            let mut spec: clusia_core::diagram::DiagramSpec = serde_json::from_value(json!({
                "key": "flow", "kind": "flow", "title": "t", "caption": caption,
                "nodes": [{"id": "a", "label": "A", "shape": "box"}]
            }))
            .unwrap();
            spec.key = "flow".into();
            spec
        };
        let version = |number: u32, state: EntryState| DiagramVersion {
            number,
            commit: "h".into(),
            harness: "Claude Code".into(),
            drawn_at: 1,
            phase: VersionPhase::Done,
            entries: vec![DiagramEntry {
                plan: PlanEntry {
                    key: "flow".into(),
                    kind: clusia_core::diagram::DiagramKind::Flow,
                    title: "t".into(),
                    purpose: "p".into(),
                },
                state,
            }],
        };
        let earlier = [
            version(
                1,
                EntryState::Ready {
                    spec: spec("one"),
                    notes: Vec::new(),
                },
            ),
            version(2, EntryState::NotDrawn),
            version(
                3,
                EntryState::Ready {
                    spec: spec("three"),
                    notes: Vec::new(),
                },
            ),
        ];
        assert_eq!(previous_spec(&earlier, 4, "flow").unwrap().caption, "three");
        assert_eq!(previous_spec(&earlier, 3, "flow").unwrap().caption, "one");
        assert_eq!(previous_spec(&earlier, 1, "flow"), None);
        assert_eq!(previous_spec(&earlier, 4, "graph"), None);
    }

    /// A version `number` drawn from `commit` whose diagram `flow` has box `a` pointing at
    /// `anchor` (when given) and a box `b` that is not tied to code.
    fn seed_version(shared: &Shared, number: u32, commit: &str, anchor: Option<(&str, u32, u32)>) {
        let mut node = json!({"id": "a", "label": "refresh()", "shape": "box"});
        if let Some((path, start, end)) = anchor {
            node["anchor"] = json!({"path": path, "start_line": start, "end_line": end});
        }
        let spec = serde_json::from_value(json!({
            "key": "flow", "kind": "flow", "title": "The flow", "caption": "c",
            "nodes": [node, {"id": "b", "label": "other", "shape": "box"}]
        }))
        .unwrap();
        let version = DiagramVersion {
            number,
            commit: commit.into(),
            harness: "Claude Code".into(),
            drawn_at: 1,
            phase: VersionPhase::Done,
            entries: vec![
                DiagramEntry {
                    plan: PlanEntry {
                        key: "flow".into(),
                        kind: clusia_core::diagram::DiagramKind::Flow,
                        title: "The flow".into(),
                        purpose: "p".into(),
                    },
                    state: EntryState::Ready {
                        spec,
                        notes: Vec::new(),
                    },
                },
                DiagramEntry {
                    plan: PlanEntry {
                        key: "graph".into(),
                        kind: clusia_core::diagram::DiagramKind::Graph,
                        title: "The graph".into(),
                        purpose: "p".into(),
                    },
                    state: EntryState::NotDrawn,
                },
            ],
        };
        save_diagram_version(&shared.paths, &pr(1), &version).unwrap();
    }

    /// What GitHub says the pull request changed at `head`: `src/a.rs` gets five new lines.
    async fn serve_files(shared: &Shared, head: &str) {
        let files = vec![clusia_core::FileDiff {
            path: "src/a.rs".into(),
            previous_path: None,
            status: "added".into(),
            additions: 5,
            deletions: 0,
            patch: Some("@@ -0,0 +1,5 @@\n+a\n+b\n+c\n+d\n+e".into()),
        }];
        shared
            .files_cache
            .lock()
            .await
            .insert(pr(1), (head.to_string(), Arc::new(files)));
    }

    async fn comment(shared: &Shared, key: &str, node: &str, number: u32) -> Outcome {
        add_comment(shared, "test", &pr(1), number, key, node, "  Why this?\n").await
    }

    fn item_of(out: Outcome) -> clusia_core::DraftItem {
        let Outcome::Ok(Reply::DraftItem(item)) = out else {
            panic!("a draft item, got {out:?}");
        };
        item
    }

    #[tokio::test]
    async fn a_box_with_no_code_becomes_a_general_comment_of_the_diagram() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        seed_version(&shared, 1, "h", None);
        let item = item_of(comment(&shared, "flow", "a", 1).await);
        assert_eq!(item.kind, DraftKind::General);
        assert_eq!(item.origin, Origin::Diagram);
        assert_eq!((item.anchor, item.body.as_str()), (None, "Why this?"));
    }

    #[tokio::test]
    async fn a_box_on_lines_of_the_diff_becomes_a_line_comment_at_the_head() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        seed_version(&shared, 1, "h", Some(("src/a.rs", 2, 3)));
        serve_files(&shared, "h").await;
        let item = item_of(comment(&shared, "flow", "a", 1).await);
        assert_eq!(
            (item.kind, item.origin),
            (DraftKind::LineComment, Origin::Diagram)
        );
        let anchor = item.anchor.expect("an anchor");
        assert_eq!(
            (
                anchor.path.as_str(),
                anchor.start_line,
                anchor.line,
                anchor.side
            ),
            ("src/a.rs", Some(2), 3, Side::Right)
        );
        assert_eq!(
            (anchor.commit.as_str(), item.body.as_str()),
            ("h", "Why this?")
        );
        // One line: no range.
        seed_version(&shared, 2, "h", Some(("src/a.rs", 4, 4)));
        let single = item_of(comment(&shared, "flow", "a", 2).await);
        assert_eq!(
            single.anchor.map(|a| (a.start_line, a.line)),
            Some((None, 4))
        );
    }

    #[tokio::test]
    async fn code_the_diff_does_not_have_becomes_a_general_comment_that_names_its_lines() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        serve_files(&shared, "h").await;
        seed_version(&shared, 1, "h", Some(("src/a.rs", 10, 12)));
        let range = item_of(comment(&shared, "flow", "a", 1).await);
        assert_eq!(
            (range.kind, range.origin),
            (DraftKind::General, Origin::Diagram)
        );
        assert_eq!(range.body, "src/a.rs:10-12: Why this?");
        seed_version(&shared, 2, "h", Some(("src/other.rs", 7, 7)));
        let single = item_of(comment(&shared, "flow", "a", 2).await);
        assert_eq!(single.body, "src/other.rs:7: Why this?");
        // A range that crosses the end of the diff does not fit either.
        seed_version(&shared, 3, "h", Some(("src/a.rs", 4, 8)));
        assert_eq!(
            item_of(comment(&shared, "flow", "a", 3).await).kind,
            DraftKind::General
        );
    }

    #[tokio::test]
    async fn a_version_of_a_commit_that_cannot_be_compared_gives_a_general_comment() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        serve_files(&shared, "h").await;
        seed_version(&shared, 1, "an-older-commit", Some(("src/a.rs", 2, 3)));
        let item = item_of(comment(&shared, "flow", "a", 1).await);
        assert_eq!(
            (item.kind, item.body.as_str()),
            (DraftKind::General, "src/a.rs:2-3: Why this?")
        );
    }

    #[tokio::test]
    async fn a_comment_the_daemon_cannot_place_is_refused_with_its_reason() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        seed_version(&shared, 1, "h", None);
        let refused = |out: Outcome| match out {
            Outcome::Err(e) => e,
            other => panic!("refused, got {other:?}"),
        };
        let empty = refused(add_comment(&shared, "t", &pr(1), 1, "flow", "a", " \n").await);
        assert_eq!(empty.code, ErrorCode::BadRequest);
        assert_eq!(
            refused(comment(&shared, "flow", "a", 9).await).code,
            ErrorCode::NotFound
        );
        assert_eq!(
            refused(comment(&shared, "nope", "a", 1).await).code,
            ErrorCode::NotFound
        );
        assert_eq!(
            refused(comment(&shared, "flow", "zz", 1).await).code,
            ErrorCode::NotFound
        );
        let not_drawn = refused(comment(&shared, "graph", "a", 1).await);
        assert_eq!(not_drawn.code, ErrorCode::InvalidState);
        let mut review = reviews::load_stored(&shared, &pr(1)).unwrap().unwrap();
        review.state = ReviewState::Published;
        save_review(&shared.paths, &review).unwrap();
        assert!(matches!(
            comment(&shared, "flow", "a", 1).await,
            Outcome::Err(_)
        ));
    }

    #[tokio::test]
    async fn what_the_agent_said_about_the_diagrams_reaches_the_listing_only_for_the_head_it_read()
    {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        seed_version(&shared, 1, "h", None);
        let mut review = reviews::load_stored(&shared, &pr(1)).unwrap().unwrap();
        review.head_sha = "h2".into();
        clusia_core::open_round(
            &mut review,
            std::collections::BTreeMap::new(),
            "h",
            "h2",
            clusia_core::Relocation::default(),
            0,
            0,
        );
        review.revalidation.as_mut().unwrap().diagrams_stale = Some(clusia_core::DiagramsStale {
            stale: false,
            reason: "Only docs changed.".into(),
        });
        save_review(&shared.paths, &review).unwrap();
        let reason = |shared: &Shared| match get(shared, &pr(1)) {
            Outcome::Ok(Reply::Diagrams {
                stale,
                stale_reason,
                ..
            }) => (stale, stale_reason),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            reason(&shared),
            (true, Some("Only docs changed.".to_string()))
        );
        // The head moved on again: the answer was about another one.
        review.head_sha = "h3".into();
        save_review(&shared.paths, &review).unwrap();
        assert_eq!(reason(&shared), (true, None));
        // Nothing stale, nothing to explain.
        seed_version(&shared, 2, "h3", None);
        assert_eq!(reason(&shared), (false, None));
    }

    #[tokio::test]
    async fn a_key_the_client_sent_is_quoted_escaped_and_short() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        seed_version(&shared, 1, "h", None);
        let key = format!("x\u{1b}[2J{}", "k".repeat(500));
        let Err(Outcome::Err(e)) = check_redraw(&shared, &pr(1), "h", &key) else {
            panic!("refused");
        };
        assert!(!e.message.contains('\u{1b}'), "{}", e.message);
        assert!(e.message.contains("x\\u{1b}[2J"), "{}", e.message);
        assert!(e.message.len() < 200, "{}", e.message);
    }

    #[tokio::test]
    async fn the_digests_list_what_each_ready_diagram_draws() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        assert!(digests(&shared, &pr(1)).await.is_empty());
        seed_version(&shared, 1, "h", Some(("src/a.rs", 1, 2)));
        let found = digests(&shared, &pr(1)).await;
        assert_eq!(found.len(), 1, "an entry not drawn is not described");
        assert_eq!(
            (found[0].key.as_str(), found[0].paths.as_slice()),
            ("flow", ["src/a.rs".to_string()].as_slice())
        );
    }

    const TOKEN_RS: &str = "pub fn refresh_token() {}\nfn other() {\n    refresh_token();\n}\n";
    const HTTP_RS: &str = "fn sign() {\n    refresh_token();\n}\n";

    fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Makes the worktree of review 1 a repository whose commit holds `src/token.rs` and
    /// `src/http.rs`, makes that commit the review's head, and has GitHub say the pull request
    /// adds the four lines of `src/token.rs`. Returns the commit.
    async fn keyword_repo(shared: &Shared) -> String {
        let root = shared.paths.worktree_for(&pr(1));
        git_in(&root, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/token.rs"), TOKEN_RS).unwrap();
        std::fs::write(root.join("src/http.rs"), HTTP_RS).unwrap();
        git_in(&root, &["add", "."]);
        git_in(&root, &["commit", "-q", "-m", "one"]);
        let head = git_in(&root, &["rev-parse", "HEAD"]);
        set_head(shared, 1, &head);
        let files = vec![clusia_core::FileDiff {
            path: "src/token.rs".into(),
            previous_path: None,
            status: "added".into(),
            additions: 4,
            deletions: 0,
            patch: Some(
                "@@ -0,0 +1,4 @@\n+pub fn refresh_token() {}\n+fn other() {\n+    refresh_token();\n+}"
                    .into(),
            ),
        }];
        shared
            .files_cache
            .lock()
            .await
            .insert(pr(1), (head.clone(), Arc::new(files)));
        head
    }

    fn keyword_plan_turn(seed_line: u32) -> Turn {
        Turn::answer_blocks(
            "The words.",
            &[keyword_plan_block(json!({
                "title": "Where the words live", "caption": "Refresh tokens",
                "axes": [
                    {"name": "Layer", "low": "client", "high": "storage"},
                    {"name": "Time", "low": "login", "high": "expiry"},
                    {"name": "Risk", "low": "safe", "high": "risky"}
                ],
                "families": [{"id": "refresh", "label": "refresh",
                    "seed": {"word": "refresh_token", "path": "src/token.rs", "line": seed_line},
                    "words": ["refresh_token"]}],
                "use_cases": [{"id": "refresh", "name": "Token refresh"}]
            }))],
        )
    }

    fn keyword_placement_turn() -> Turn {
        Turn::answer_blocks(
            "Placed.",
            &[keyword_placement_block(json!({
                "regions": [{"path": "src/token.rs", "use_case": "refresh", "at": [0.5, 0.5, 0.5]}]
            }))],
        )
    }

    fn keywords_on(config: &mut clusia_core::Config) {
        config.harness.keywords_3d = true;
    }

    fn map_of(version: &DiagramVersion) -> &KeywordMap {
        match &version
            .entry(KEYWORDS_KEY)
            .expect("the keywords entry")
            .state
        {
            EntryState::ReadyMap { map, .. } => map,
            other => panic!("a map, got {other:?}"),
        }
    }

    fn reason_at(version: &DiagramVersion, key: &str) -> String {
        match &version.entry(key).expect("the entry").state {
            EntryState::Failed { reason } => reason.clone(),
            other => panic!("failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_keywords_entry_comes_last_and_maps_the_words_of_the_commit_in_two_turns() {
        let script = Script::turns(vec![
            plan_turn(&["flow"]),
            draw_turn("flow"),
            keyword_plan_turn(1),
            keyword_placement_turn(),
        ]);
        let (_home, fake, shared) = lab(script, 1, keywords_on);
        let head = keyword_repo(&shared).await;
        assert!(matches!(
            run(&shared, &pr(1)).await,
            Outcome::Ok(Reply::Ack)
        ));
        settled(&shared, 1).await;
        let version = latest(&shared, 1);
        assert_eq!(version.commit, head);
        let keys: Vec<&str> = version
            .entries
            .iter()
            .map(|e| e.plan.key.as_str())
            .collect();
        assert_eq!(keys, ["flow", "keywords"]);
        assert_eq!(
            version.entries[1].plan.kind,
            clusia_core::diagram::DiagramKind::Keywords
        );
        let map = map_of(&version);
        let places: Vec<(String, u32, bool, bool, bool)> = (0..map.places.len())
            .map(|i| {
                let anchor = map.anchor(i).expect("an anchor");
                let place = &map.places[i];
                (
                    anchor.path,
                    anchor.start_line,
                    place.declared(),
                    place.changed(),
                    map.use_case_of(place).is_some(),
                )
            })
            .collect();
        assert_eq!(
            places,
            [
                ("src/http.rs".to_string(), 2, false, false, false),
                ("src/token.rs".to_string(), 1, true, true, true),
                ("src/token.rs".to_string(), 3, false, true, true),
            ]
        );
        assert!(!map.search_cut);
        let calls = FakeClaude::calls(fake.path());
        assert_eq!(calls.len(), 4, "plan, flow, then two keyword turns");
        let placing = prompt_of(&calls[3]);
        assert!(placing.contains("src/http.rs"), "{placing}");
        assert!(placing.contains("refresh_token"), "{placing}");
        assert_eq!(shared.diagrams.live_state(&pr(1)), None);
    }

    #[tokio::test]
    async fn with_the_setting_off_no_keywords_entry_is_drawn() {
        let script = Script::turns(vec![plan_turn(&["flow"]), draw_turn("flow")]);
        let (_home, fake, shared) = lab(script, 1, |config| config.harness.keywords_3d = false);
        assert!(matches!(
            run(&shared, &pr(1)).await,
            Outcome::Ok(Reply::Ack)
        ));
        settled(&shared, 1).await;
        let version = latest(&shared, 1);
        assert!(version.entry(KEYWORDS_KEY).is_none());
        assert_eq!(FakeClaude::calls(fake.path()).len(), 2);
    }

    #[tokio::test]
    async fn the_agent_never_sees_the_keywords_entry_but_a_redraw_gets_the_previous_words() {
        let script = Script::turns(vec![
            plan_turn(&["flow"]),
            draw_turn("flow"),
            keyword_plan_turn(1),
            keyword_placement_turn(),
            plan_turn(&["flow"]),
            draw_turn("flow"),
            keyword_plan_turn(1),
            keyword_placement_turn(),
        ]);
        let (_home, fake, shared) = lab(script, 1, keywords_on);
        keyword_repo(&shared).await;
        for _ in 0..2 {
            assert!(matches!(
                run(&shared, &pr(1)).await,
                Outcome::Ok(Reply::Ack)
            ));
            settled(&shared, 1).await;
        }
        assert_eq!(latest(&shared, 1).number, 2);
        let prompts: Vec<String> = FakeClaude::calls(fake.path())
            .iter()
            .map(prompt_of)
            .collect();
        assert_eq!(prompts.len(), 8);
        for at in [1, 4, 5] {
            assert!(
                !prompts[at].contains(KEYWORDS_TITLE),
                "turn {at} offers the keywords entry: {}",
                prompts[at]
            );
        }
        assert!(
            prompts[6].contains("Token refresh"),
            "the second keyword turn carries the first map's plan: {}",
            prompts[6]
        );
    }

    #[tokio::test]
    async fn a_keyword_plan_that_stays_unreadable_fails_only_its_entry() {
        let script = Script::turns(vec![
            plan_turn(&["flow"]),
            draw_turn("flow"),
            Turn::answer("No words."),
            Turn::answer("Still no words."),
        ]);
        let (_home, fake, shared) = lab(script, 1, keywords_on);
        keyword_repo(&shared).await;
        let _ = run(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        let version = latest(&shared, 1);
        assert!(matches!(version.entries[0].state, EntryState::Ready { .. }));
        assert!(!reason_at(&version, KEYWORDS_KEY).is_empty());
        assert_eq!(
            FakeClaude::calls(fake.path()).len(),
            4,
            "one repair, no search turn"
        );
    }

    #[tokio::test]
    async fn a_seed_on_a_line_the_change_does_not_add_is_traced_nowhere() {
        // The last turn repeats, so the repair gives the same plan.
        let script = Script::turns(vec![
            plan_turn(&["flow"]),
            draw_turn("flow"),
            keyword_plan_turn(9),
        ]);
        let (_home, fake, shared) = lab(script, 1, keywords_on);
        keyword_repo(&shared).await;
        let _ = run(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        assert_eq!(
            reason_at(&latest(&shared, 1), KEYWORDS_KEY),
            "No word of this change could be traced."
        );
        assert_eq!(FakeClaude::calls(fake.path()).len(), 4);
    }

    #[tokio::test]
    async fn a_seed_on_an_added_line_that_lacks_its_word_is_traced_nowhere() {
        // Line 2 of `src/token.rs` is added (`fn other() {`) but does not hold `refresh_token`.
        let script = Script::turns(vec![
            plan_turn(&["flow"]),
            draw_turn("flow"),
            keyword_plan_turn(2),
            keyword_placement_turn(),
        ]);
        let (_home, fake, shared) = lab(script, 1, keywords_on);
        keyword_repo(&shared).await;
        let _ = run(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        assert_eq!(
            reason_at(&latest(&shared, 1), KEYWORDS_KEY),
            "No word of this change could be traced."
        );
        assert_eq!(
            FakeClaude::calls(fake.path()).len(),
            3,
            "no repair, no placement turn"
        );
    }

    #[tokio::test]
    async fn a_family_whose_seed_word_is_not_on_its_line_is_dropped_with_a_note() {
        let two_families = Turn::answer_blocks(
            "The words.",
            &[keyword_plan_block(json!({
                "title": "Where the words live", "caption": "c",
                "axes": [
                    {"name": "Layer", "low": "client", "high": "storage"},
                    {"name": "Time", "low": "login", "high": "expiry"},
                    {"name": "Risk", "low": "safe", "high": "risky"}
                ],
                "families": [
                    {"id": "refresh", "label": "refresh",
                     "seed": {"word": "refresh_token", "path": "src/token.rs", "line": 1},
                     "words": ["refresh_token"]},
                    {"id": "other", "label": "other",
                     "seed": {"word": "other", "path": "src/token.rs", "line": 3},
                     "words": ["other"]}
                ],
                "use_cases": [{"id": "refresh", "name": "Token refresh"}]
            }))],
        );
        let script = Script::turns(vec![
            plan_turn(&["flow"]),
            draw_turn("flow"),
            two_families,
            keyword_placement_turn(),
        ]);
        let (_home, _fake, shared) = lab(script, 1, keywords_on);
        keyword_repo(&shared).await;
        let _ = run(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        let version = latest(&shared, 1);
        let EntryState::ReadyMap { map, notes } = &version.entry(KEYWORDS_KEY).unwrap().state
        else {
            panic!("a map");
        };
        let ids: Vec<&str> = map.plan.families.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["refresh"]);
        assert!(
            map.words.iter().all(|w| w.text != "other"),
            "the dropped family's words are not searched"
        );
        assert!(
            notes.contains(&"1 family dropped: its seed word is not on its line".to_string()),
            "{notes:?}"
        );
    }

    #[test]
    fn the_seed_note_counts_the_dropped_families() {
        assert_eq!(seed_note(0), None);
        assert_eq!(
            seed_note(1).as_deref(),
            Some("1 family dropped: its seed word is not on its line")
        );
        assert_eq!(
            seed_note(3).as_deref(),
            Some("3 families dropped: their seed words are not on their lines")
        );
    }

    #[tokio::test]
    async fn a_checkout_that_moved_fails_the_entry_before_the_search() {
        let script = Script::turns(vec![
            plan_turn(&["flow"]),
            draw_turn("flow"),
            keyword_plan_turn(1),
            keyword_placement_turn(),
        ]);
        let (_home, fake, shared) = lab(script, 1, keywords_on);
        keyword_repo(&shared).await;
        let root = shared.paths.worktree_for(&pr(1));
        std::fs::write(root.join("src/http.rs"), "moved\n").unwrap();
        git_in(&root, &["commit", "-q", "-am", "two"]);
        let _ = run(&shared, &pr(1)).await;
        settled(&shared, 1).await;
        assert_eq!(
            reason_at(&latest(&shared, 1), KEYWORDS_KEY),
            "The checkout moved while drawing."
        );
        assert_eq!(FakeClaude::calls(fake.path()).len(), 3, "no placement turn");
    }

    #[tokio::test]
    async fn stopping_during_the_placement_turn_leaves_the_entry_not_drawn_and_no_process() {
        let script = Script::turns(vec![
            plan_turn(&["flow"]),
            draw_turn("flow"),
            keyword_plan_turn(1),
            Turn::hanging(),
        ]);
        let (_home, fake, shared) = lab(script, 1, keywords_on);
        keyword_repo(&shared).await;
        let _ = run(&shared, &pr(1)).await;
        let pid = calls(&fake, 4).await[3].pid;
        assert_eq!(
            shared.diagrams.live_state(&pr(1)),
            Some(DiagramsState::Drawing {
                done: 1,
                total: 2,
                key: KEYWORDS_KEY.into(),
                step: Some("Placing files on the axes…".into()),
            })
        );
        let _ = stop(&shared, &pr(1));
        settled(&shared, 1).await;
        let stored = latest(&shared, 1);
        assert!(matches!(
            stored.entry(KEYWORDS_KEY).unwrap().state,
            EntryState::NotDrawn
        ));
        assert_eq!(stored.phase, VersionPhase::Done);
        assert!(!alive(pid));
    }

    #[tokio::test]
    async fn work_between_turns_ends_when_the_run_is_stopped() {
        let stop = Stop::new();
        stop.fire(Why::User);
        assert_eq!(
            unless_stopped(&stop, async { 1 }).await,
            Err(Why::User),
            "a stop asked before is not lost"
        );
        let stop = Stop::new();
        let later = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            later.fire(Why::Shutdown);
        });
        let started = Instant::now();
        assert_eq!(
            unless_stopped(&stop, std::future::pending::<()>()).await,
            Err(Why::Shutdown)
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(unless_stopped(&Stop::new(), async { 7 }).await, Ok(7));
    }

    #[test]
    fn a_cut_search_says_so_after_the_other_notes() {
        assert_eq!(
            map_notes(vec!["a".into()], vec!["b".into()], true),
            [
                "a",
                "b",
                "Search stopped after 10 s; some places are missing."
            ]
        );
        assert_eq!(map_notes(Vec::new(), vec!["b".into()], false), ["b"]);
    }

    /// A map of one family and two places: `src/a.rs:2` (changed) and `src/other.rs:7`.
    fn small_map() -> KeywordMap {
        serde_json::from_value(json!({
            "plan": {
                "title": "Where the words live", "caption": "",
                "axes": [
                    {"name": "Layer", "low": "client", "high": "storage"},
                    {"name": "Time", "low": "login", "high": "expiry"},
                    {"name": "Risk", "low": "safe", "high": "risky"}
                ],
                "families": [{"id": "refresh", "label": "refresh",
                    "seed": {"word": "refresh_token", "path": "src/a.rs", "line": 2},
                    "words": ["refresh_token"]}],
                "use_cases": [{"id": "refresh", "name": "Token refresh"}]
            },
            "regions": [],
            "paths": ["src/a.rs", "src/other.rs"],
            "words": [{"text": "refresh_token", "family": 0}],
            "places": [[0, 2, 0, 65535, 2], [1, 7, 0, 65535, 0]],
            "overflow": [0],
            "search_cut": false
        }))
        .unwrap()
    }

    #[test]
    fn a_map_over_the_cap_is_too_large_to_keep() {
        assert!(!too_large(&small_map()));
        let mut big = small_map();
        big.paths = (0..2_000)
            .map(|i| format!("{i}{}", "x".repeat(2_000)))
            .collect();
        assert!(too_large(&big));
    }

    /// Version `number`, drawn from `commit`, whose only entry is `small_map`.
    fn seed_map_version(shared: &Shared, number: u32, commit: &str) {
        let version = DiagramVersion {
            number,
            commit: commit.into(),
            harness: "Claude Code".into(),
            drawn_at: 1,
            phase: VersionPhase::Done,
            entries: vec![DiagramEntry {
                plan: clusia_core::diagram::keywords_entry(),
                state: EntryState::ReadyMap {
                    map: Box::new(small_map()),
                    notes: Vec::new(),
                },
            }],
        };
        save_diagram_version(&shared.paths, &pr(1), &version).unwrap();
    }

    #[tokio::test]
    async fn a_changed_place_is_a_line_comment_and_any_other_place_names_its_line() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        seed_map_version(&shared, 1, "h");
        serve_files(&shared, "h").await;
        let changed = item_of(comment(&shared, KEYWORDS_KEY, "p0", 1).await);
        assert_eq!(
            (changed.kind, changed.origin),
            (DraftKind::LineComment, Origin::Diagram)
        );
        let anchor = changed.anchor.expect("an anchor");
        assert_eq!(
            (
                anchor.path.as_str(),
                anchor.start_line,
                anchor.line,
                anchor.side
            ),
            ("src/a.rs", None, 2, Side::Right)
        );
        let elsewhere = item_of(comment(&shared, KEYWORDS_KEY, "p1", 1).await);
        assert_eq!(
            (elsewhere.kind, elsewhere.anchor, elsewhere.body.as_str()),
            (DraftKind::General, None, "src/other.rs:7: Why this?")
        );
        for node in ["p2", "a", "p", "p-1"] {
            let Outcome::Err(refusal) = comment(&shared, KEYWORDS_KEY, node, 1).await else {
                panic!("{node} is no place");
            };
            assert_eq!(refusal.code, ErrorCode::NotFound, "{node}");
        }
    }

    #[tokio::test]
    async fn a_place_of_a_version_after_a_force_push_is_named_and_its_code_is_gone() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        seed_map_version(&shared, 1, "an-older-commit");
        serve_files(&shared, "h").await;
        let item = item_of(comment(&shared, KEYWORDS_KEY, "p0", 1).await);
        assert_eq!(
            (item.kind, item.body.as_str()),
            (DraftKind::General, "src/a.rs:2: Why this?")
        );
        let Outcome::Ok(Reply::DiagramCode(code)) =
            crate::diagram_code::read(&shared, &pr(1), 1, KEYWORDS_KEY, "p0").await
        else {
            panic!("the code");
        };
        assert_eq!(
            (code.path.as_str(), code.unavailable.as_deref()),
            (
                "src/a.rs",
                Some("The code at an-olde is no longer available.")
            )
        );
    }

    #[tokio::test]
    async fn the_code_of_a_place_starts_one_line_before_it() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        let root = shared.paths.worktree_for(&pr(1));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "one\ntwo\nthree\nfour\nfive\nsix\n").unwrap();
        seed_map_version(&shared, 1, "h");
        serve_files(&shared, "h").await;
        let Outcome::Ok(Reply::DiagramCode(code)) =
            crate::diagram_code::read(&shared, &pr(1), 1, KEYWORDS_KEY, "p0").await
        else {
            panic!("the code");
        };
        // Place p0 is line 2; every line of the file is added, but only the place's is tinted.
        let lines: Vec<(u32, &str, bool)> = code
            .lines
            .iter()
            .map(|l| (l.number, l.text.as_str(), l.changed))
            .collect();
        assert_eq!(code.start_line, 1);
        assert_eq!(
            lines,
            [
                (1, "one", false),
                (2, "two", true),
                (3, "three", false),
                (4, "four", false),
            ]
        );
    }

    #[tokio::test]
    async fn a_version_with_a_map_loads_and_lists_its_places() {
        let (_home, _fake, shared) = lab(Script::one(Turn::hanging()), 1, |_| {});
        seed_map_version(&shared, 1, "h");
        let Outcome::Ok(Reply::Diagrams {
            versions, latest, ..
        }) = get(&shared, &pr(1))
        else {
            panic!("the diagrams");
        };
        assert_eq!(versions[0].entries[0].places, 2);
        assert_eq!(map_of(&latest.expect("the latest")).places.len(), 2);
    }
}
