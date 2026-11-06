//! Single runs and comparisons: each item's rounds in the review's runner with no network,
//! alternating this pull request and main after a warm-up on each, one sampler per round, the
//! approved scenario bytes from a read-only folder, the results by side, and the lookup the view
//! reads them back with.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clusia_core::PrRef;
use clusia_core::cockpit::{
    BENCH_MAX_BYTES, COCKPIT_CHANGED, COMPARE_MAX_SECS, CockpitActivity, CockpitScope,
    CompareOutcome, CompareRecord, ComparisonView, Freshness, Input, ItemKind, ItemRef,
    ItemResults, ItemSupport, LOAD_LATER, MAIN_FIRST, Metric, NEW_FUNCTION, NO_SCENARIOS,
    NOT_APPROVED, RoundEnd, RoundResult, RunSide, SERVICES_ON_MAIN, Sample, Series, SideRecord,
    SideView, WARMUP_MAX_SECS, approval_hash, comparison_view, freshness, script_sha, side_view,
    valid_scenario_slug,
};
use clusia_core::functions::{FunctionChange, FunctionLang, valid_slug};
use clusia_core::tests::{
    OUTPUT_EVERY_MS, RunnerKind, TAIL_MAX, TestScope, TestsPhase, TestsRecord,
};
use clusia_protocol::{Event, Outcome, Reply, topics};
use clusia_testrun::bench::{
    BenchReport, NO_BENCH_RESULT, bench_env, bench_metrics, bench_target, read_bench_line,
};
use clusia_testrun::docker::{ContainerNet, SCENARIO_DIR, WORK_DIR};
use clusia_testrun::exec::{Ended, StopFlag, Stream};
use clusia_testrun::functions::{
    NO_PACKAGE, function_command, package_for, place_driver, remove_driver,
};
use clusia_testrun::log::Tail;
use clusia_testrun::sample::summarize;
use clusia_testrun::scenario::{
    ScenarioReport, read_scenario_report, remove_run_folder, scenario_env, write_run_folder,
};

use super::cases::{self, Refusal};
use super::work::{self, RunJob, Usable};
use super::{Doing, bench, case_run, env, main_tree, scenarios};
use crate::reviews;
use crate::state::Shared;
use crate::sync::now_unix;

pub(super) const NO_PACKAGE_ON_MAIN: &str = "This function's package doesn't exist on main";
/// The most points one `CockpitSamples` carries.
const BATCH_MAX: usize = 16;
const REPORT_LINE_MAX: usize = 64 * 1024;
const OUTPUT_EVERY: Duration = Duration::from_millis(OUTPUT_EVERY_MS);
/// How long removing a panicked run's container may take.
const DOCKER_RM_LIMIT: Duration = Duration::from_secs(30);
const BENCH_CHANGED: &str = "The benchmark changed since this run started";
const MAIN_NOT_PLAIN: &str =
    "Clúsia won't write the benchmark into main: its folder is not a plain folder";

/// The newest results of `item`: the last run of this pull request alone and the last comparison.
pub(super) fn results_of(record: &TestsRecord, item: &ItemRef) -> ItemResults {
    record
        .cockpit_results
        .get(&item.key())
        .cloned()
        .unwrap_or_default()
}

/// What an item's results are held against to tell whether they are still about what is under
/// review.
pub(super) struct Now<'a> {
    pub head: &'a str,
    pub base: Option<&'a str>,
    pub runner: RunnerKind,
}

/// The last single run and the last comparison of `item`, shown as `metrics` say.
pub(super) fn shown(
    record: &TestsRecord,
    item: &ItemRef,
    input_hash: &str,
    now: &Now<'_>,
    metrics: &[Metric],
    scenario: bool,
) -> (Option<SideView>, Option<ComparisonView>) {
    let results = results_of(record, item);
    let fresh = |side| freshness(side, now.head, now.base, input_hash, now.runner);
    let last = results
        .single
        .as_ref()
        .map(|side| side_view(side, fresh(side)));
    let comparison = results.compare.as_ref().map(|c: &CompareRecord| {
        let sides: (Freshness, Freshness) = (fresh(&c.pr), fresh(&c.main));
        comparison_view(c, metrics, sides, scenario)
    });
    (last, comparison)
}

pub(super) enum Reader {
    Scenario(ScenarioReport),
    Bench(BenchReport),
}

/// A round's stdout and stderr as they arrive: marked stdout lines read at once (with the time
/// since the round began), everything else into the output's tail. Bounded whatever is printed.
pub(super) struct RoundSink {
    mark: String,
    prefix: Vec<u8>,
    /// The unfinished line of stdout and of stderr.
    lines: [Vec<u8>; 2],
    /// Whether the unfinished line of each stream already went to the tail for its length; the
    /// rest of it follows it there.
    overflowed: [bool; 2],
    reader: Reader,
    tail: Tail,
    began: Instant,
}

impl RoundSink {
    fn new(mark: &str, reader: Reader) -> Self {
        Self {
            mark: mark.to_string(),
            prefix: format!("{mark} ").into_bytes(),
            lines: [Vec::new(), Vec::new()],
            overflowed: [false, false],
            reader,
            tail: Tail::new(TAIL_MAX),
            began: Instant::now(),
        }
    }

    pub(super) fn scenario(mark: &str) -> Self {
        Self::new(mark, Reader::Scenario(ScenarioReport::default()))
    }

    pub(super) fn bench(mark: &str) -> Self {
        Self::new(mark, Reader::Bench(BenchReport::default()))
    }

    pub(super) fn chunk(&mut self, stream: Stream, bytes: &[u8]) {
        let at = match stream {
            Stream::Out => 0,
            Stream::Err => 1,
        };
        let mut rest = bytes;
        while !rest.is_empty() {
            let (piece, ended) = match rest.iter().position(|&b| b == b'\n') {
                Some(end) => (&rest[..=end], true),
                None => (rest, false),
            };
            rest = &rest[piece.len()..];
            if self.overflowed[at] {
                self.tail.push(piece);
            } else {
                self.lines[at].extend_from_slice(piece);
                if ended {
                    let line = std::mem::take(&mut self.lines[at]);
                    self.line(stream, &line);
                } else if self.lines[at].len() > REPORT_LINE_MAX {
                    let line = std::mem::take(&mut self.lines[at]);
                    self.tail.push(&line);
                    self.overflowed[at] = true;
                }
            }
            if ended {
                self.overflowed[at] = false;
            }
        }
    }

    /// A whole line, its `\n` included: a marked stdout line is read and kept out of the tail.
    fn line(&mut self, stream: Stream, line: &[u8]) {
        if stream != Stream::Out || !line.starts_with(&self.prefix) {
            self.tail.push(line);
            return;
        }
        let text = String::from_utf8_lossy(line);
        let at_ms = u64::try_from(self.began.elapsed().as_millis()).unwrap_or(u64::MAX);
        match &mut self.reader {
            Reader::Scenario(report) => read_scenario_report(&self.mark, &text, at_ms, report),
            Reader::Bench(report) => read_bench_line(&self.mark, &text, at_ms, report),
        }
    }

    /// What the marked lines said, and the output; an unfinished line is output, stdout's first.
    pub(super) fn finish(mut self) -> (Reader, String) {
        for at in 0..2 {
            let line = std::mem::take(&mut self.lines[at]);
            self.tail.push(&line);
        }
        (self.reader, self.tail.text())
    }
}

/// What one item runs, chosen and checked by the request.
enum Work {
    Scenario {
        command: String,
        inputs: Vec<Input>,
        values: BTreeMap<String, String>,
        files: Vec<(String, Vec<u8>)>,
        timeout_secs: u32,
    },
    Bench {
        slug: String,
        lang: FunctionLang,
        file: String,
        template: String,
        loop_file: Vec<u8>,
        /// The Cargo package's folder and name in the pull request's worktree, for Rust.
        package: Option<(String, String)>,
        /// The same in main's worktree, for a Rust comparison.
        main_package: Option<(String, String)>,
        values: BTreeMap<String, String>,
        warmup: u32,
        secs: u32,
    },
}

struct Planned {
    item: ItemRef,
    input_hash: String,
    rounds: u32,
    work: Work,
}

/// The jobs of both sides; `main` only for a comparison, with main's base.
struct Sides {
    pr: RunJob,
    main: Option<(RunJob, String)>,
}

/// Publishes a run's points in batches, never more often than `OUTPUT_EVERY_MS`, and only when
/// a point arrives.
struct SampleBatch<'a> {
    shared: &'a Shared,
    pr: &'a PrRef,
    run: &'a str,
    item: &'a ItemRef,
    side: RunSide,
    round: u32,
    pending: Vec<Sample>,
    last: Option<Instant>,
}

impl SampleBatch<'_> {
    fn point(&mut self, point: Sample) {
        if self.pending.len() >= BATCH_MAX {
            self.pending.remove(0);
        }
        self.pending.push(point);
        if self.last.is_none_or(|at| at.elapsed() >= OUTPUT_EVERY) {
            self.send();
        }
    }

    fn send(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        self.last = Some(Instant::now());
        self.shared.publish(
            topics::TESTS,
            Event::CockpitSamples {
                pr: self.pr.clone(),
                run: self.run.to_string(),
                item: self.item.clone(),
                side: self.side,
                round: self.round,
                points: std::mem::take(&mut self.pending),
            },
        );
    }
}

/// Why an item can't run, or `None` when it is `Ready`.
fn support_reason(support: &ItemSupport) -> Option<String> {
    match support {
        ItemSupport::Ready => None,
        ItemSupport::NeedsApproval => Some(NOT_APPROVED.to_string()),
        ItemSupport::NoCommand { reason }
        | ItemSupport::Stale { reason }
        | ItemSupport::Unsupported { reason } => Some(reason.clone()),
    }
}

enum Chosen {
    Scenario(usize),
    Bench(usize),
}

/// The items `scope` runs, in order, and the scenarios it skips.
fn choose(
    read: &scenarios::Read,
    scope: &CockpitScope,
    on_main: bool,
) -> Result<(Vec<Chosen>, Vec<String>), String> {
    match scope {
        CockpitScope::Item { item } if item.kind == ItemKind::Bench => {
            let Some(at) = read.view.benches.iter().position(|b| b.slug == item.id) else {
                return Err(COCKPIT_CHANGED.to_string());
            };
            if on_main && read.benches[at].change == FunctionChange::New {
                return Err(NEW_FUNCTION.to_string());
            }
            if let Some(reason) = support_reason(&read.view.benches[at].support) {
                return Err(reason);
            }
            Ok((vec![Chosen::Bench(at)], Vec::new()))
        }
        CockpitScope::Item { item } => {
            let Some(at) = read.view.scenarios.iter().position(|s| s.slug == item.id) else {
                return Err(COCKPIT_CHANGED.to_string());
            };
            if !read.scenarios[at].approved {
                return Err(NOT_APPROVED.to_string());
            }
            if let Some(reason) = support_reason(&read.view.scenarios[at].support) {
                return Err(reason);
            }
            Ok((vec![Chosen::Scenario(at)], Vec::new()))
        }
        CockpitScope::Scenarios => {
            let (mut chosen, mut skipped) = (Vec::new(), Vec::new());
            for (at, view) in read.view.scenarios.iter().enumerate() {
                if read.scenarios[at].approved && view.support == ItemSupport::Ready {
                    chosen.push(Chosen::Scenario(at));
                } else {
                    skipped.push(view.slug.clone());
                }
            }
            if chosen.is_empty() {
                return Err(NO_SCENARIOS.to_string());
            }
            Ok((chosen, skipped))
        }
    }
}

/// A scenario as it will run: the bytes read with the view, which must be the approved ones.
fn plan_scenario(read: &scenarios::Read, at: usize) -> Result<Planned, String> {
    let item = &read.scenarios[at];
    let Some(command) = item.command.clone() else {
        return Err(COCKPIT_CHANGED.to_string());
    };
    let shas: BTreeMap<String, String> = item
        .files
        .iter()
        .map(|(name, bytes)| (name.clone(), script_sha(bytes)))
        .collect();
    if read.record.scenario_approvals.get(&item.slug) != Some(&approval_hash(&command, &shas)) {
        return Err(COCKPIT_CHANGED.to_string());
    }
    Ok(Planned {
        item: ItemRef {
            kind: ItemKind::Scenario,
            id: item.slug.clone(),
        },
        input_hash: item.input_hash.clone(),
        rounds: item.rounds,
        work: Work::Scenario {
            command,
            inputs: item.inputs.clone(),
            values: item.values.clone(),
            files: item.files.clone(),
            timeout_secs: item.timeout_secs,
        },
    })
}

/// A benchmark as it will run: the approved recipe's command and the loop file whose hash
/// `bench.json` holds.
fn plan_bench(read: &scenarios::Read, usable: &Usable, at: usize) -> Result<Planned, String> {
    let item = &read.benches[at];
    let template = usable
        .recipe
        .functions
        .as_ref()
        .and_then(|spec| spec.command(item.lang));
    let Some(template) = template.filter(|t| Some(*t) == item.template.as_deref()) else {
        return Err(COCKPIT_CHANGED.to_string());
    };
    let (Some(loop_file), Some(stored)) = (item.loop_file.clone(), item.stored.as_ref()) else {
        return Err(COCKPIT_CHANGED.to_string());
    };
    if script_sha(&loop_file) != stored.bench_sha {
        return Err(COCKPIT_CHANGED.to_string());
    }
    let package = match (item.lang, &item.package) {
        (FunctionLang::Rust, Some(Ok(package))) => Some(package.clone()),
        (FunctionLang::Rust, Some(Err(message))) => return Err(message.clone()),
        (FunctionLang::Rust, None) => return Err(NO_PACKAGE.to_string()),
        _ => None,
    };
    Ok(Planned {
        item: ItemRef {
            kind: ItemKind::Bench,
            id: item.slug.clone(),
        },
        input_hash: item.input_hash.clone(),
        rounds: item.rounds,
        work: Work::Bench {
            slug: item.slug.clone(),
            lang: item.lang,
            file: item.file.clone(),
            template: template.to_string(),
            loop_file,
            package,
            main_package: None,
            values: item.values.clone(),
            warmup: item.warmup,
            secs: item.secs,
        },
    })
}

/// Finds each Rust benchmark's package in main's worktree.
fn main_packages(worktree: &Path, planned: &mut [Planned]) -> Result<(), String> {
    for item in planned {
        if let Work::Bench {
            lang: FunctionLang::Rust,
            file,
            main_package,
            ..
        } = &mut item.work
        {
            let found = package_for(worktree, file).map_err(|_| NO_PACKAGE_ON_MAIN.to_string())?;
            *main_package = Some(found);
        }
    }
    Ok(())
}

/// Main's state, refused unless a comparison can run there now.
async fn main_ready(
    shared: &Shared,
    pr: &PrRef,
    runner: RunnerKind,
    planned: &mut [Planned],
) -> Result<main_tree::MainState, String> {
    let state = main_tree::main_state(shared, pr).await;
    let Some(record) = &state.record else {
        return Err(MAIN_FIRST.to_string());
    };
    // A record of another runner is never ready, so this is said first.
    if record.runner != runner {
        return Err(main_tree::MAIN_OTHER_RUNNER.to_string());
    }
    if !state.ready {
        return Err(MAIN_FIRST.to_string());
    }
    let Some(ctx) = &state.ctx else {
        return Err(MAIN_FIRST.to_string());
    };
    main_packages(&ctx.worktree, planned)?;
    Ok(state)
}

/// `RunCockpit`: checks that what would run is what the reviewer saw and approved, on both
/// sides, then runs it in the background. Nothing is written or run before the last check.
pub(crate) async fn run_cockpit(
    shared: &Arc<Shared>,
    pr: &PrRef,
    scope: CockpitScope,
    on_main: bool,
    seen: String,
) -> Outcome {
    if let CockpitScope::Item { item } = &scope {
        match item.kind {
            ItemKind::Load => return reviews::invalid_state(LOAD_LATER),
            ItemKind::Scenario if !valid_scenario_slug(&item.id) => {
                return reviews::bad_request("invalid scenario id");
            }
            ItemKind::Bench if !valid_slug(&item.id) => {
                return reviews::bad_request("invalid function id");
            }
            _ => {}
        }
    }
    if let Err(out) = super::active(shared, pr) {
        return out;
    }
    let usable = match work::ready_to_run(shared, pr).await {
        Ok(usable) => usable,
        Err(out) => return out,
    };
    let read = scenarios::read(shared, pr).await;
    if let Some(problem) = &read.shipped {
        return reviews::invalid_state(problem.clone());
    }
    if let Err(out) = scenarios::check_seen(&read.view, pr, &scope, &seen) {
        return out;
    }
    let (chosen, skipped) = match choose(&read, &scope, on_main) {
        Ok(chosen) => chosen,
        Err(message) => return reviews::invalid_state(message),
    };
    let mut planned = Vec::new();
    for at in chosen {
        let item = match at {
            Chosen::Scenario(at) => plan_scenario(&read, at),
            Chosen::Bench(at) => plan_bench(&read, &usable, at),
        };
        match item {
            Ok(item) => planned.push(item),
            Err(message) => return reviews::invalid_state(message),
        }
    }
    let runner = usable.env.runner;
    if on_main {
        if usable.recipe.services.is_some() {
            return reviews::invalid_state(SERVICES_ON_MAIN);
        }
        if let Err(message) = main_ready(shared, pr, runner, &mut planned).await {
            return reviews::invalid_state(message);
        }
    }
    let started = match super::begin(shared, pr, Doing::Cockpit, TestsPhase::Waiting) {
        Ok(started) => started,
        Err(refused) => return refused.outcome(pr),
    };
    let (id, live, stop) = (started.id, started.live, started.stop);
    let run_id = format!("{}-{}", now_unix(), shared.tests.seq());
    let mut main = None;
    if on_main {
        // Inside the slot: moving main's worktree never races another activity of the review.
        let state = match main_tree::ensure_main_at_base(shared, pr).await {
            Ok(_) => main_ready(shared, pr, runner, &mut planned).await,
            Err(message) => Err(message),
        };
        let state = match state {
            Ok(state) => state,
            Err(message) => {
                scenarios::end(shared, pr, id, None);
                drop(live);
                scenarios::tell_cockpit(shared, pr).await;
                return reviews::invalid_state(message);
            }
        };
        let (Some(record), Some(ctx), Some(env)) = (state.record, state.ctx, state.env) else {
            scenarios::end(shared, pr, id, None);
            drop(live);
            scenarios::tell_cockpit(shared, pr).await;
            return reviews::invalid_state(MAIN_FIRST);
        };
        let job = RunJob {
            id: run_id.clone(),
            scope: TestScope::All,
            commands: Vec::new(),
            head: usable.head.clone(),
            ctx,
            env,
            recipe: usable.recipe.clone(),
            image: state.image,
            network: ContainerNet::None,
            scenario: None,
            side: RunSide::Main,
        };
        main = Some((job, record.base));
    }
    let sides = Sides {
        pr: work::job_of(usable, run_id.clone(), TestScope::All, Vec::new()),
        main,
    };
    let total = planned.len();
    let first = &planned[0];
    let activity = CockpitActivity::Running {
        run: run_id.clone(),
        item: first.item.clone(),
        side: RunSide::Pr,
        round: 0,
        rounds: shown_rounds(first, on_main),
        done_items: 0,
        total_items: u32::try_from(total).unwrap_or(u32::MAX),
    };
    set_activity(shared, pr, id, activity);
    scenarios::tell_cockpit(shared, pr).await;
    let whole = match scope {
        CockpitScope::Scenarios => {
            let secs = shared.config.read().await.tests.run_timeout_secs;
            Some(Duration::from_secs(u64::from(secs) * 4))
        }
        CockpitScope::Item { .. } => None,
    };
    let task = run(
        shared.clone(),
        pr.clone(),
        id,
        stop,
        run_id.clone(),
        planned,
        sides,
        whole,
    );
    let (shared, pr) = (shared.clone(), pr.clone());
    tokio::spawn(async move {
        if tokio::spawn(task).await.is_err() {
            tracing::error!(pr = %pr, "a cockpit run failed inside the daemon");
            remove_container(&shared, &pr, id).await;
        }
        scenarios::end(&shared, &pr, id, None);
        drop(live);
        scenarios::tell_cockpit(&shared, &pr).await;
    });
    Outcome::Ok(Reply::CockpitStarted {
        run: run_id,
        skipped,
    })
}

/// Removes the container activity `id` recorded, so neither a run that panicked nor the sampler
/// inside its container outlives it. Any failure, "No such container" included, is ignored.
async fn remove_container(shared: &Shared, pr: &PrRef, id: u64) {
    let name = shared.tests.with(pr, |entry| {
        entry
            .activity
            .as_ref()
            .filter(|a| a.id == id)
            .and_then(|a| a.container.clone())
    });
    let Some(name) = name else {
        return;
    };
    if let Some(docker) = env::docker_now(shared).await {
        let _ = shared
            .test_exec
            .output(
                &docker.command(["rm", "-f", name.as_str()]),
                DOCKER_RM_LIMIT,
            )
            .await;
    }
}

/// How many rounds an item runs on each side.
fn shown_rounds(planned: &Planned, compare: bool) -> u32 {
    if compare { planned.rounds } else { 1 }
}

fn set_activity(shared: &Shared, pr: &PrRef, act: u64, activity: CockpitActivity) {
    shared.tests.with(pr, |entry| {
        if let Some(running) = entry.activity.as_mut().filter(|a| a.id == act) {
            running.cockpit = Some(activity);
        }
    });
}

/// What every item and round of one run shares.
struct RunCtx<'a> {
    shared: &'a Arc<Shared>,
    pr: &'a PrRef,
    act: u64,
    stop: &'a StopFlag,
    run_id: &'a str,
    total: usize,
    /// When the run last published a sample batch.
    last_batch: Option<Instant>,
}

/// Runs the planned items in order with a place, within each item's budget and the run's.
#[allow(clippy::too_many_arguments)]
async fn run(
    shared: Arc<Shared>,
    pr: PrRef,
    act: u64,
    stop: Arc<StopFlag>,
    run_id: String,
    planned: Vec<Planned>,
    mut sides: Sides,
    whole: Option<Duration>,
) {
    let Some(_place) = super::place(&shared, &pr, act, &stop).await else {
        return;
    };
    remove_leftovers(&shared, &pr, &sides).await;
    let whole_end = whole.map(|budget| tokio::time::Instant::now() + budget);
    let mut cx = RunCtx {
        shared: &shared,
        pr: &pr,
        act,
        stop: &stop,
        run_id: &run_id,
        total: planned.len(),
        last_batch: None,
    };
    for (n, item) in planned.iter().enumerate() {
        let now = tokio::time::Instant::now();
        if stop.fired() || whole_end.is_some_and(|end| now >= end) {
            break;
        }
        let item_end = now + Duration::from_secs(COMPARE_MAX_SECS);
        let deadline = whole_end.map_or(item_end, |end| end.min(item_end));
        if run_item(&mut cx, n, item, &mut sides, deadline).await == CompareOutcome::Stopped {
            break;
        }
    }
}

/// What an earlier run left: its run folders and the bench files it placed in either worktree.
async fn remove_leftovers(shared: &Shared, pr: &PrRef, sides: &Sides) {
    let runs = shared.paths.test_runs_for(pr);
    for entry in std::fs::read_dir(&runs).into_iter().flatten().flatten() {
        let path = entry.path();
        if let Err(e) = remove_run_folder(&path) {
            tracing::warn!(error = %e, folder = %path.display(), "cannot remove a scenario run folder");
        }
    }
    let record = env::load(shared, pr);
    for placed in &record.placed {
        case_run::unplace(shared, pr, &sides.pr.ctx.worktree, placed).await;
    }
    if !record.main_placed.is_empty() {
        let main = match &sides.main {
            Some((job, _)) => Some(job.ctx.worktree.clone()),
            None => std::fs::canonicalize(shared.paths.main_worktree_for(pr)).ok(),
        };
        for placed in &record.main_placed {
            unplace_main(shared, pr, main.as_deref(), placed).await;
        }
    }
}

/// Removes a bench file placed in main's worktree and forgets it, or keeps it in the record for
/// the next run. With main's worktree gone, or a path not of the placed form, it is forgotten.
async fn unplace_main(shared: &Shared, pr: &PrRef, worktree: Option<&Path>, placed: &str) {
    let removed = worktree.map(|worktree| match copy_parts(placed) {
        Some((slug, name)) => remove_main_copy(worktree, slug, name),
        None => remove_driver(worktree, placed),
    });
    let forget = match removed {
        None | Some(Ok(())) => true,
        Some(Err(e)) if e.kind() == std::io::ErrorKind::InvalidInput => true,
        Some(Err(e)) => {
            tracing::warn!(pr = %pr, path = placed, error = %e, "cannot remove a bench file from main");
            false
        }
    };
    if forget {
        env::update_record(shared, pr, |record| {
            record.main_placed.retain(|p| p != placed);
        })
        .await;
    }
}

/// The slug and file name of a non-Rust loop file's copy in main's worktree,
/// `.clusia/functions/<slug>/bench.<ext>`; `None` for any other path.
fn copy_parts(placed: &str) -> Option<(&str, &str)> {
    let (slug, name) = placed.strip_prefix(".clusia/functions/")?.split_once('/')?;
    let named = FunctionLang::ALL
        .iter()
        .any(|lang| name == format!("bench.{}", lang.ext()));
    (valid_slug(slug) && named).then_some((slug, name))
}

/// The path of main's copy of `slug`'s loop file, as `main_placed` records it.
fn copy_path(slug: &str, name: &str) -> String {
    format!(".clusia/functions/{slug}/{name}")
}

/// Removes main's copy of `slug`'s loop file without following a link, then the folders above
/// it that are empty now. A link on the way is refused as not a placed path.
fn remove_main_copy(worktree: &Path, slug: &str, name: &str) -> std::io::Result<()> {
    match cases::dir_at(worktree, &[".clusia", "functions", slug], false) {
        Ok(Some(dir)) => cases::remove_at(&dir, name)?,
        Ok(None) => {}
        Err(Refusal::NotPlain) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                MAIN_NOT_PLAIN,
            ));
        }
        Err(Refusal::Io(message)) => return Err(std::io::Error::other(message)),
    }
    prune_copy_folders(worktree, slug);
    Ok(())
}

/// Removes `.clusia/functions/<slug>`, then `.clusia/functions`, then `.clusia` from main's
/// worktree, each only while it is empty: the copy's write made them when they were missing.
fn prune_copy_folders(worktree: &Path, slug: &str) {
    let levels: [(&[&str], &str); 3] = [
        (&[".clusia", "functions"], slug),
        (&[".clusia"], "functions"),
        (&[], ".clusia"),
    ];
    for (parts, name) in levels {
        let Ok(Some(parent)) = cases::dir_at(worktree, parts, false) else {
            return;
        };
        if cases::remove_dir_at(&parent, name).is_err() {
            return;
        }
    }
}

/// Adds `path` to `record.main_placed`, or takes it out.
async fn note_main(shared: &Shared, pr: &PrRef, path: &str, placed: bool) {
    env::update_record(shared, pr, |record| {
        record.main_placed.retain(|p| p != path);
        if placed {
            record.main_placed.push(path.to_string());
        }
    })
    .await;
}

/// Removes a scenario's run folder when dropped, however its item ends.
struct RunFolder(PathBuf);

impl Drop for RunFolder {
    fn drop(&mut self) {
        if let Err(e) = remove_run_folder(&self.0) {
            tracing::warn!(error = %e, folder = %self.0.display(), "cannot remove a scenario run folder");
        }
    }
}

/// What an item put in place for its rounds, all of it undone when the item ends.
#[derive(Default)]
struct Placed {
    folder: Option<RunFolder>,
    /// The Rust loop file in the pull request's worktree, relative to it.
    pr_driver: Option<String>,
    /// The Rust loop file in main's worktree, relative to it.
    main_driver: Option<String>,
    /// Main's copy of a non-Rust loop file: its folder, the slug and its name there.
    main_copy: Option<(OwnedFd, String, String)>,
}

impl Placed {
    async fn undo(self, shared: &Shared, pr: &PrRef, sides: &mut Sides) {
        drop(self.folder);
        sides.pr.scenario = None;
        if let Some((job, _)) = sides.main.as_mut() {
            job.scenario = None;
        }
        if let Some(placed) = &self.pr_driver {
            case_run::unplace(shared, pr, &sides.pr.ctx.worktree, placed).await;
        }
        if let Some(placed) = &self.main_driver {
            let main = sides.main.as_ref().map(|(job, _)| job.ctx.worktree.clone());
            unplace_main(shared, pr, main.as_deref(), placed).await;
        }
        if let Some((dir, slug, name)) = &self.main_copy {
            match cases::remove_at(dir, name) {
                Ok(()) => {
                    if let Some((job, _)) = &sides.main {
                        prune_copy_folders(&job.ctx.worktree, slug);
                    }
                    note_main(shared, pr, &copy_path(slug, name), false).await;
                }
                Err(e) => {
                    tracing::warn!(pr = %pr, error = %e, "cannot remove the benchmark copy from main");
                }
            }
        }
    }
}

/// Places a Rust loop file in `worktree`, recorded in `record.placed` (the pull request's) or
/// `record.main_placed` (main's) before it exists, and answers its command.
#[allow(clippy::too_many_arguments)]
async fn place_rust(
    shared: &Shared,
    pr: &PrRef,
    worktree: &Path,
    side: RunSide,
    slug: &str,
    template: &str,
    package: &(String, String),
    loop_file: &[u8],
) -> Result<(String, String), String> {
    let (dir, name) = package;
    let target = bench_target(slug);
    // Known before anything is placed, so a refused command leaves nothing behind.
    let command = function_command(template, &target, Some(name))?;
    let path = case_run::placed_path(dir, &target);
    let mut leftover = false;
    env::update_record(shared, pr, |record| {
        let placed = match side {
            RunSide::Pr => &mut record.placed,
            RunSide::Main => &mut record.main_placed,
        };
        leftover = placed.contains(&path);
        if !leftover {
            placed.push(path.clone());
        }
    })
    .await;
    match place_driver(worktree, dir, &target, loop_file, leftover) {
        Ok(at) => Ok((at, command)),
        Err(message) => {
            env::update_record(shared, pr, |record| {
                let placed = match side {
                    RunSide::Pr => &mut record.placed,
                    RunSide::Main => &mut record.main_placed,
                };
                placed.retain(|p| *p != path);
            })
            .await;
            Err(message)
        }
    }
}

/// Puts an item's files in place on both sides and answers each side's command.
async fn prepare(
    cx: &RunCtx<'_>,
    n: usize,
    planned: &Planned,
    sides: &mut Sides,
    placed: &mut Placed,
) -> Result<(String, Option<String>), String> {
    let (shared, pr) = (cx.shared, cx.pr);
    match &planned.work {
        Work::Scenario { command, files, .. } => {
            let runs = shared.paths.test_runs_for(pr);
            let folder = runs.join(format!("{}-{n}", cx.run_id));
            std::fs::create_dir_all(&runs)
                .and_then(|()| write_run_folder(&folder, files))
                .map_err(|e| format!("Clúsia couldn't prepare the scenario's files: {e}"))?;
            placed.folder = Some(RunFolder(folder.clone()));
            let real = std::fs::canonicalize(&folder).unwrap_or(folder);
            sides.pr.scenario = Some(real.clone());
            let main_command = sides.main.as_mut().map(|(job, _)| {
                job.scenario = Some(real);
                command.clone()
            });
            Ok((command.clone(), main_command))
        }
        Work::Bench {
            slug,
            lang,
            template,
            loop_file,
            package,
            main_package,
            ..
        } => {
            let script = bench::loop_path(slug, *lang);
            let pr_command = match (lang, package) {
                (FunctionLang::Rust, Some(package)) => {
                    let worktree = sides.pr.ctx.worktree.clone();
                    let (at, command) = place_rust(
                        shared,
                        pr,
                        &worktree,
                        RunSide::Pr,
                        slug,
                        template,
                        package,
                        loop_file,
                    )
                    .await?;
                    placed.pr_driver = Some(at);
                    command
                }
                (FunctionLang::Rust, None) => return Err(NO_PACKAGE.to_string()),
                _ => function_command(template, &script, None)?,
            };
            let Some((main, _)) = &sides.main else {
                return Ok((pr_command, None));
            };
            let worktree = main.ctx.worktree.clone();
            let main_command = match (lang, main_package) {
                (FunctionLang::Rust, Some(package)) => {
                    let (at, command) = place_rust(
                        shared,
                        pr,
                        &worktree,
                        RunSide::Main,
                        slug,
                        template,
                        package,
                        loop_file,
                    )
                    .await?;
                    placed.main_driver = Some(at);
                    command
                }
                (FunctionLang::Rust, None) => return Err(NO_PACKAGE_ON_MAIN.to_string()),
                _ => {
                    let command = function_command(template, &script, None)?;
                    let name = format!("bench.{}", lang.ext());
                    let path = copy_path(slug, &name);
                    // Recorded before it exists, so a run that dies after writing it still
                    // lets the next run remove it.
                    note_main(shared, pr, &path, true).await;
                    let parts = [".clusia", "functions", slug.as_str()];
                    let written = match cases::dir_at(&worktree, &parts, true) {
                        Ok(Some(dir)) => cases::write_whole(&dir, &name, loop_file)
                            .map(|()| dir)
                            .map_err(|e| e.to_string()),
                        Ok(None) => Err(format!("cannot make the folder of {slug} in main")),
                        Err(Refusal::NotPlain) => Err(MAIN_NOT_PLAIN.to_string()),
                        Err(Refusal::Io(message)) => Err(message),
                    };
                    match written {
                        Ok(dir) => placed.main_copy = Some((dir, slug.clone(), name)),
                        Err(message) => {
                            prune_copy_folders(&worktree, slug);
                            note_main(shared, pr, &path, false).await;
                            return Err(message);
                        }
                    }
                    command
                }
            };
            Ok((pr_command, Some(main_command)))
        }
    }
}

/// Writes `change` into the results of `item`.
async fn store(shared: &Shared, pr: &PrRef, item: &ItemRef, change: impl FnOnce(&mut ItemResults)) {
    env::update_record(shared, pr, |record| {
        change(record.cockpit_results.entry(item.key()).or_default());
    })
    .await;
}

/// One side's record before its first round.
fn side_record(side: RunSide, sha: &str, input_hash: &str, runner: RunnerKind) -> SideRecord {
    SideRecord {
        side,
        sha: sha.to_string(),
        input_hash: input_hash.to_string(),
        runner,
        rounds: Vec::new(),
        series: Series::default(),
        errors: Vec::new(),
        stopped: false,
    }
}

/// One item: its files put in place, its rounds (a warm-up on each side first for a
/// comparison), its results stored after every measured round and at the end, and everything
/// it placed removed.
async fn run_item(
    cx: &mut RunCtx<'_>,
    n: usize,
    planned: &Planned,
    sides: &mut Sides,
    deadline: tokio::time::Instant,
) -> CompareOutcome {
    let (shared, pr) = (cx.shared, cx.pr);
    let item = &planned.item;
    let mut pr_side = side_record(
        RunSide::Pr,
        &sides.pr.head,
        &planned.input_hash,
        sides.pr.env.runner,
    );
    let mut main_side = sides
        .main
        .as_ref()
        .map(|(job, base)| side_record(RunSide::Main, base, &planned.input_hash, job.env.runner));
    let mut placed = Placed::default();
    let commands = prepare(cx, n, planned, sides, &mut placed).await;
    let outcome = match commands {
        Err(message) => {
            failed(cx, planned, &mut pr_side, main_side.as_ref(), &message).await;
            CompareOutcome::Failed { message }
        }
        Ok((pr_command, main_command)) => {
            let rounds = Rounds {
                n,
                planned,
                pr_command: &pr_command,
                main_command: main_command.as_deref(),
                deadline,
            };
            rounds
                .run(cx, sides, &mut pr_side, main_side.as_mut())
                .await
        }
    };
    if let Some(main) = main_side {
        let record = CompareRecord {
            run: cx.run_id.to_string(),
            pr: pr_side,
            main,
            outcome: outcome.clone(),
            at: now_unix(),
        };
        store(shared, pr, item, |results| results.compare = Some(record)).await;
    } else if !pr_side.rounds.is_empty() {
        store(shared, pr, item, |results| results.single = Some(pr_side)).await;
    }
    placed.undo(shared, pr, sides).await;
    outcome
}

/// An item whose files could not be put in place: one `Error` round on a single run, a
/// `Failed` comparison otherwise.
async fn failed(
    cx: &RunCtx<'_>,
    planned: &Planned,
    pr_side: &mut SideRecord,
    main_side: Option<&SideRecord>,
    message: &str,
) {
    if main_side.is_some() {
        return;
    }
    let result = error_round(planned, RunSide::Pr, 1, &pr_side.sha, message);
    pr_side.rounds.push(result.clone());
    cx.shared.publish(
        topics::TESTS,
        Event::CockpitRound {
            pr: cx.pr.clone(),
            run: cx.run_id.to_string(),
            result: Box::new(result),
        },
    );
}

/// A round that ended before its command ran, with `message` as why.
fn error_round(
    planned: &Planned,
    side: RunSide,
    round: u32,
    sha: &str,
    message: &str,
) -> RoundResult {
    RoundResult {
        item: planned.item.clone(),
        side,
        round,
        sha: sha.to_string(),
        passed: None,
        end: RoundEnd::Error,
        exit_code: None,
        metrics: BTreeMap::new(),
        outputs: BTreeMap::new(),
        errors: Vec::new(),
        message: Some(message.to_string()),
        tail: String::new(),
        took_ms: 0,
        at: now_unix(),
    }
}

/// Whether the pull request's own loop file of `slug` still holds the bytes the request checked.
fn same_loop(worktree: &Path, slug: &str, lang: FunctionLang, checked: &[u8]) -> bool {
    let Ok(Some(dir)) = cases::folder(worktree, slug, false) else {
        return false;
    };
    let name = format!("bench.{}", lang.ext());
    matches!(
        cases::read_capped(&dir, &name, BENCH_MAX_BYTES),
        Ok(Some(bytes)) if script_sha(&bytes) == script_sha(checked)
    )
}

/// The rounds of one item, once its files are in place.
struct Rounds<'a> {
    n: usize,
    planned: &'a Planned,
    pr_command: &'a str,
    main_command: Option<&'a str>,
    deadline: tokio::time::Instant,
}

impl Rounds<'_> {
    async fn run(
        &self,
        cx: &mut RunCtx<'_>,
        sides: &Sides,
        pr_side: &mut SideRecord,
        mut main_side: Option<&mut SideRecord>,
    ) -> CompareOutcome {
        let (shared, pr) = (cx.shared, cx.pr);
        let planned = self.planned;
        let run_timeout =
            Duration::from_secs(u64::from(shared.config.read().await.tests.run_timeout_secs));
        let own = match &planned.work {
            Work::Scenario { timeout_secs, .. } => {
                Duration::from_secs(u64::from(*timeout_secs)).min(run_timeout)
            }
            Work::Bench { secs, .. } => Duration::from_secs(u64::from(*secs) + 60),
        };
        let warm = run_timeout.min(Duration::from_secs(WARMUP_MAX_SECS));
        let mut schedule = Vec::new();
        match (&sides.main, self.main_command) {
            (Some(_), Some(_)) => {
                schedule.extend([(RunSide::Pr, 0), (RunSide::Main, 0)]);
                for round in 1..=planned.rounds {
                    schedule.extend([(RunSide::Pr, round), (RunSide::Main, round)]);
                }
            }
            _ => schedule.push((RunSide::Pr, 1)),
        }
        let compare = schedule.len() > 1;
        let mut main_built = false;
        for (side, round) in schedule {
            if cx.stop.fired() {
                return CompareOutcome::Stopped;
            }
            let left = self
                .deadline
                .saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return CompareOutcome::TimedOut;
            }
            let limit = if round == 0 { warm } else { own }.min(left);
            let (job, command, sha) = match (side, &sides.main, self.main_command) {
                (RunSide::Main, Some((job, base)), Some(command)) => (job, command, base.as_str()),
                _ => (&sides.pr, self.pr_command, sides.pr.head.as_str()),
            };
            let at = RoundAt {
                n: self.n,
                side,
                round,
                rounds: shown_rounds(planned, compare),
                limit,
            };
            // A non-Rust loop runs from the pull request's worktree, which its own runs may
            // write: each round runs only the bytes the request checked.
            let changed = match &planned.work {
                Work::Bench {
                    slug,
                    lang,
                    loop_file,
                    ..
                } if side == RunSide::Pr && *lang != FunctionLang::Rust => {
                    !same_loop(&sides.pr.ctx.worktree, slug, *lang, loop_file)
                }
                _ => false,
            };
            let Ran {
                result,
                points,
                exited,
            } = if changed {
                Ran {
                    result: error_round(planned, side, round, sha, BENCH_CHANGED),
                    points: Vec::new(),
                    exited: false,
                }
            } else {
                run_round(cx, planned, job, command, &at, sha).await
            };
            let past_budget = result.end == RoundEnd::TimedOut && limit == left;
            let stopped = result.end == RoundEnd::Stopped;
            if round > 0 {
                let record = match side {
                    RunSide::Pr => Some(&mut *pr_side),
                    RunSide::Main => main_side.as_deref_mut(),
                };
                if let Some(record) = record {
                    record.rounds.push(result.clone());
                    if result.end == RoundEnd::Finished {
                        let mut series = Series::default();
                        for point in points {
                            series.push(point);
                        }
                        record.series = series;
                        record.errors = result.errors.clone();
                    }
                    if stopped {
                        record.stopped = true;
                    }
                }
                let built = side == RunSide::Main && exited;
                let stored = (pr_side.clone(), main_side.as_deref().cloned());
                let run = cx.run_id.to_string();
                env::update_record(shared, pr, |record| {
                    if built
                        && !main_built
                        && let Some(main) = record.main.as_mut()
                    {
                        main.built = true;
                    }
                    let results = record
                        .cockpit_results
                        .entry(planned.item.key())
                        .or_default();
                    match stored {
                        (pr_record, Some(main)) => {
                            results.compare = Some(CompareRecord {
                                run,
                                pr: pr_record,
                                main,
                                outcome: CompareOutcome::Done,
                                at: now_unix(),
                            });
                        }
                        (pr_record, None) => results.single = Some(pr_record),
                    }
                })
                .await;
                main_built |= built;
                shared.publish(
                    topics::TESTS,
                    Event::CockpitRound {
                        pr: pr.clone(),
                        run: cx.run_id.to_string(),
                        result: Box::new(result),
                    },
                );
                scenarios::tell_cockpit(shared, pr).await;
            }
            if changed {
                return CompareOutcome::Failed {
                    message: BENCH_CHANGED.to_string(),
                };
            }
            if stopped {
                return CompareOutcome::Stopped;
            }
            if past_budget {
                return CompareOutcome::TimedOut;
            }
        }
        CompareOutcome::Done
    }
}

/// What one round gave: its result, every point its sampler read, and whether its command ran
/// to its end.
struct Ran {
    result: RoundResult,
    points: Vec<Sample>,
    exited: bool,
}

/// Where one round stands in its run.
struct RoundAt {
    /// The item's index in the run.
    n: usize,
    side: RunSide,
    /// 0 for a warm-up.
    round: u32,
    rounds: u32,
    limit: Duration,
}

/// Adds the peak memory and the average CPU of `points` inside `window` to `metrics`.
fn add_usage(metrics: &mut BTreeMap<Metric, u64>, points: &[Sample], window: Option<(u64, u64)>) {
    let (peak, average) = summarize(points, window);
    if let Some(peak) = peak {
        metrics.insert(Metric::PeakMemory, peak);
    }
    if let Some(average) = average {
        metrics.insert(Metric::AvgCpu, u64::from(average));
    }
}

/// One round on one side: the activity says it, the command runs measured with its own mark
/// and environment, its points go out in batches (none for a warm-up), and its result is read.
async fn run_round(
    cx: &mut RunCtx<'_>,
    planned: &Planned,
    job: &RunJob,
    command: &str,
    at: &RoundAt,
    sha: &str,
) -> Ran {
    let (shared, pr) = (cx.shared, cx.pr);
    let warmup = at.round == 0;
    set_activity(
        shared,
        pr,
        cx.act,
        CockpitActivity::Running {
            run: cx.run_id.to_string(),
            item: planned.item.clone(),
            side: at.side,
            round: at.round,
            rounds: at.rounds,
            done_items: u32::try_from(at.n).unwrap_or(u32::MAX),
            total_items: u32::try_from(cx.total).unwrap_or(u32::MAX),
        },
    );
    scenarios::tell_cockpit(shared, pr).await;
    let mark: String = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
    let container = job.env.runner == RunnerKind::Container;
    let root = if container {
        WORK_DIR.to_string()
    } else {
        job.ctx.worktree.display().to_string()
    };
    let (extra, mut sink) = match &planned.work {
        Work::Scenario { inputs, values, .. } => {
            let dir = match (&job.scenario, container) {
                (_, true) => SCENARIO_DIR.to_string(),
                (Some(folder), false) => folder.display().to_string(),
                (None, false) => String::new(),
            };
            (
                scenario_env(&mark, &root, &dir, inputs, values),
                RoundSink::scenario(&mark),
            )
        }
        Work::Bench {
            values,
            warmup: calls,
            secs,
            ..
        } => {
            let secs = if warmup { 1 } else { *secs };
            (
                bench_env(&mark, &root, values, *calls, secs),
                RoundSink::bench(&mark),
            )
        }
    };
    let mut batch = SampleBatch {
        shared,
        pr,
        run: cx.run_id,
        item: &planned.item,
        side: at.side,
        round: at.round,
        pending: Vec::new(),
        last: cx.last_batch,
    };
    let measured = {
        let mut chunk = |stream: Stream, bytes: &[u8]| sink.chunk(stream, bytes);
        // A warm-up is sampled like any round, but keeps and publishes nothing.
        let mut push = |point: Sample| {
            if !warmup {
                batch.point(point);
            }
        };
        work::run_measured(
            shared, pr, cx.act, job, command, at.limit, cx.stop, &mut chunk, &extra, &mut push,
        )
        .await
    };
    batch.send();
    cx.last_batch = batch.last;
    let (reader, tail) = sink.finish();
    let mut result = RoundResult {
        item: planned.item.clone(),
        side: at.side,
        round: at.round,
        sha: sha.to_string(),
        passed: None,
        end: RoundEnd::Error,
        exit_code: None,
        metrics: BTreeMap::new(),
        outputs: BTreeMap::new(),
        errors: Vec::new(),
        message: None,
        tail,
        took_ms: 0,
        at: now_unix(),
    };
    let measured = match measured {
        Ok(measured) => measured,
        Err(e) => {
            result.message = Some(format!("The run could not start: {e}"));
            return Ran {
                result,
                points: Vec::new(),
                exited: false,
            };
        }
    };
    result.took_ms = measured.took_ms;
    // Main's command ran to its end, so it built, whatever its result says.
    let exited = matches!(measured.ended, Ended::Exited(_));
    let code = match measured.ended {
        Ended::Exited(code) => code,
        _ => None,
    };
    result.exit_code = code;
    result.end = match measured.ended {
        Ended::Exited(_) => RoundEnd::Finished,
        Ended::TimedOut => RoundEnd::TimedOut,
        Ended::Stopped => RoundEnd::Stopped,
    };
    let finished = result.end == RoundEnd::Finished;
    match reader {
        Reader::Scenario(report) => {
            result.outputs = report.outputs;
            result.errors = report.errors;
            if finished {
                result.passed = Some(code == Some(0));
                result.metrics.insert(Metric::Duration, measured.took_ms);
                add_usage(&mut result.metrics, &measured.points, None);
            }
        }
        Reader::Bench(report) => {
            result.errors = report.errors;
            if finished {
                match &report.bench {
                    None => {
                        result.end = RoundEnd::Error;
                        result.message = Some(match code {
                            Some(code) => format!("{NO_BENCH_RESULT} (exit {code})"),
                            None => format!("{NO_BENCH_RESULT} (no exit code)"),
                        });
                    }
                    Some(line) => {
                        result.metrics = bench_metrics(line);
                        let window = report.started_ms.zip(report.bench_ms);
                        add_usage(&mut result.metrics, &measured.points, window);
                    }
                }
            }
        }
    }
    Ran {
        result,
        points: measured.points,
        exited,
    }
}

#[cfg(test)]
mod tests {
    use clusia_core::cockpit::{ERRORS_MAX, OUTPUTS_MAX};
    use clusia_core::tests::TAIL_MAX;
    use clusia_testrun::exec::Stream;

    use super::*;

    const MARK: &str = "0123456789abcdef";

    #[test]
    fn huge_output_keeps_a_tail_and_bounded_errors() {
        let mut sink = RoundSink::scenario(MARK);
        sink.chunk(
            Stream::Err,
            format!("{MARK} output from_stderr 1\n").as_bytes(),
        );
        let block = vec![b'y'; 1024 * 1024];
        for i in 0..50 {
            sink.chunk(Stream::Out, &block);
            sink.chunk(Stream::Out, b"\n");
            sink.chunk(
                Stream::Out,
                format!("{MARK} error e{i}\n{MARK} output o{i} {i}\n").as_bytes(),
            );
        }
        sink.chunk(
            Stream::Out,
            format!("{MARK} output o0 last\ntail line\n").as_bytes(),
        );
        let (Reader::Scenario(report), output) = sink.finish() else {
            panic!("a scenario sink reads a scenario report")
        };
        assert!(output.len() <= TAIL_MAX, "{}", output.len());
        assert!(output.ends_with("tail line\n"));
        assert!(!output.contains(&format!("{MARK} output")));
        assert_eq!(report.outputs.len(), OUTPUTS_MAX);
        assert_eq!(report.outputs["o0"], "last");
        assert!(
            !report.outputs.contains_key("from_stderr"),
            "stderr never reports"
        );
        assert_eq!(report.errors.len(), ERRORS_MAX);
        assert_eq!(report.errors[0].text, "e0");

        let mut long = RoundSink::bench(MARK);
        long.chunk(Stream::Out, format!("{MARK} bench 1 1 1 1 1").as_bytes());
        long.chunk(Stream::Out, &vec![b'9'; 70 * 1024]);
        long.chunk(Stream::Out, format!("\n{MARK} done").as_bytes());
        let (Reader::Bench(report), output) = long.finish() else {
            panic!("a bench sink reads a bench report")
        };
        assert_eq!(
            report.bench, None,
            "a line past 64 KiB is output, never read"
        );
        assert!(!report.done, "an unfinished last line is output");
        assert!(output.len() <= TAIL_MAX);
    }
}
