//! The tests of each review: the environment probe, the recipe, the setup plan, the runs and
//! their storage. A review does one thing at a time (generate, set up, list or run), and at most
//! `tests.parallel_runs` setups, lists and runs go at once across all reviews.

mod bench;
mod case_run;
pub(crate) mod case_turn;
pub(super) mod cases;
#[cfg(test)]
mod cockpit_lab;
mod cockpit_turn;
mod compare;
#[cfg(test)]
mod compare_lab;
mod endpoints;
mod env;
#[cfg(test)]
mod functions_lab;
#[cfg(test)]
mod lab;
#[cfg(test)]
mod main_lab;
mod main_tree;
pub(crate) mod recipe_file;
pub(crate) mod recipe_turn;
mod scenarios;
mod system;
mod system_http;
#[cfg(test)]
mod system_lab;
mod work;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clusia_core::PrRef;
use clusia_core::cockpit::CockpitActivity;
use clusia_core::functions::FunctionsActivity;
use clusia_core::tests::{Approve, TestEngineInfo, TestScope, TestsPhase, TestsView};
use clusia_protocol::{ErrorCode, Event, Outcome, ProtocolError, Reply, topics};
use clusia_testrun::exec::StopFlag;
use clusia_testrun::{engine, sandbox};
use tokio::sync::{Notify, watch};

use crate::reviews;
use crate::state::Shared;
use crate::turns::{Stop, Why};

pub(crate) use bench::{reset_bench, save_bench};
pub(crate) use case_run::run_cases;
pub(crate) use case_turn::write_cases;
pub(crate) use cases::{driver, functions, remove_case, reset_case, save_case};
pub(crate) use cockpit_turn::{ask_copilot, write_bench, write_scenarios};
pub(crate) use compare::run_cockpit;
pub(crate) use endpoints::{fix_system, write_endpoints};
pub(crate) use main_tree::{open_main, prepare_main, remove_main, sweep_main};
pub(crate) use scenarios::{
    approve_scenario, cockpit, remove_scenario, reset_scenario, save_scenario, scenario_script,
};
pub(crate) use system::{
    restart_system, set_system_network, start_system, stop_system, system, system_logs,
};

/// How long `stop_all` waits for the activity of a review to end; a container's stop may take
/// two `docker kill`s and the client's grace between them.
const STOP_WAIT: Duration = Duration::from_secs(45);
/// How long a shutdown waits for every activity to end.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(45);

pub(super) const PROBING: &str = clusia_core::tests::ACTIVITY_PROBING;
pub(super) const WRITING: &str = clusia_core::tests::ACTIVITY_WRITING;
pub(super) const GENERATE_FIRST: &str = "Generate the tests first";
pub(super) const NO_RECIPE: &str = "There is no recipe yet; generate the tests first";
pub(super) const SET_UP_FIRST: &str = "Set up the tests first: the setup plan has steps or test commands that are not done or not approved";
pub(super) const NO_NEAR: &str = "No tests near this change were found";
pub(super) const NO_LIST: &str = "The recipe has no list command";
pub(super) const STOPPING: &str = "clusiad is stopping";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Doing {
    Generate,
    Prepare,
    List,
    Run,
    Cases,
    RunCases,
    Scenarios,
    Bench,
    Main,
    Cockpit,
    System,
    Endpoints,
}

impl Doing {
    fn label(self) -> &'static str {
        match self {
            Doing::Generate => "generating the recipe",
            Doing::Prepare => "setting up",
            Doing::List => "listing the tests",
            Doing::Run => "running the tests",
            Doing::Cases => "writing cases",
            Doing::RunCases => "running cases",
            Doing::Scenarios => "writing scenarios",
            Doing::Bench => "writing a benchmark",
            Doing::Main => "preparing main",
            Doing::Cockpit => "running the QA cockpit",
            Doing::System => "starting the system",
            Doing::Endpoints => "reading the endpoints",
        }
    }

    /// Whether the activity is about the review's system, which leaves the tests' phase and
    /// their failure alone, and takes no place among `tests.parallel_runs`.
    pub(super) fn is_system(self) -> bool {
        matches!(self, Doing::System | Doing::Endpoints)
    }

    /// Whether the activity is about the function cases, which leave the tests' phase alone.
    pub(super) fn is_functions(self) -> bool {
        matches!(self, Doing::Cases | Doing::RunCases)
    }

    /// Whether the activity is about the QA cockpit, which leaves the tests' phase alone too.
    pub(super) fn is_cockpit(self) -> bool {
        matches!(
            self,
            Doing::Scenarios | Doing::Bench | Doing::Main | Doing::Cockpit
        )
    }
}

/// What a review is doing now.
pub(super) struct Activity {
    pub id: u64,
    pub doing: Doing,
    pub phase: TestsPhase,
    /// Stops the programs the activity started.
    pub stop: Arc<StopFlag>,
    /// Stops the recipe turn.
    pub turn: Arc<Stop>,
    /// The container of the command that runs now, or the compose project coming up, which
    /// the sweep must leave alone.
    pub container: Option<String>,
    /// What a functions activity is doing, for the functions view.
    pub functions: Option<FunctionsActivity>,
    /// What a cockpit activity is doing, for the cockpit view.
    pub cockpit: Option<CockpitActivity>,
}

impl Activity {
    fn fire(&self, why: Why) {
        self.turn.fire(why);
        self.stop.fire();
    }
}

/// What the daemon remembers about a review between requests; nothing here is stored.
#[derive(Default)]
pub(super) struct Entry {
    pub activity: Option<Activity>,
    /// Why the last activity failed, until the next one begins.
    pub failed: Option<String>,
    /// Why each step failed in the last setup, by step id.
    pub step_failures: BTreeMap<String, String>,
    /// The recipe hash whose compose services were pulled and built this session.
    pub services_ready: Option<String>,
    /// The stack the `services` step built from `docker compose config`, by recipe hash.
    pub stack: Option<(String, clusia_testrun::container::Stack)>,
    /// Why the `services` step refused the compose file, by recipe hash; blocks the plan until
    /// the recipe changes.
    pub compose_refused: Option<(String, String)>,
    /// Whether the PR ships its own .clusia/ (the problem, if so); `None` until asked.
    pub shipped: Option<Option<String>>,
    /// What docker said about the recipe's image and services; dropped whenever it may change.
    pub facts: Option<env::Cached>,
    /// Why the last case turn failed, until the next functions activity begins.
    pub cases_failed: Option<String>,
    /// A task waits for the call index to tell the functions view.
    pub functions_waiting: bool,
    /// Waits for the call index that ended with it still not ready, since it was last ready or
    /// a client last asked.
    pub functions_rounds: u32,
    /// Why the last scenario or benchmark turn failed, until the next one begins.
    pub cockpit_failed: Option<String>,
    /// The copilot turn that answers now; it does not hold the review's slot.
    pub copilot: Option<Arc<Stop>>,
    /// Why the last copilot turn failed, until the next ask.
    pub copilot_failed: Option<String>,
    /// Main's worktree is being made or moved.
    pub main_opening: bool,
    /// Why opening or preparing main failed, until the next such activity begins.
    pub main_failed: Option<String>,
    /// Why each of main's setup steps failed in the last walk, by step id.
    pub main_step_failures: BTreeMap<String, String>,
    /// The review's system: the live one, or the last one after it stopped or failed.
    pub system: Option<system::SystemRun>,
    /// Why the last endpoints turn failed, until the next one begins.
    pub endpoints_failed: Option<String>,
}

/// The tests of every review.
pub(crate) struct Bench {
    table: Mutex<HashMap<PrRef, Entry>>,
    /// Places taken by setups, lists and runs across all reviews.
    taken: Mutex<usize>,
    freed: Notify,
    /// Activities and background teardowns still running; shutdown waits for zero.
    live: Arc<watch::Sender<usize>>,
    closing: AtomicBool,
    /// Activity ids and container sequence numbers.
    next: AtomicU32,
    /// The mount checks of this session, by (image tag, worktree).
    mounts: Mutex<HashMap<(String, PathBuf), Result<(), String>>>,
    /// How long one wait for the call index lasts, in milliseconds.
    index_wait_ms: AtomicU64,
    /// How long a system's health loop sleeps between checks, in milliseconds.
    health_every: AtomicU64,
    /// How long a Send waits for its answer, in milliseconds.
    send_timeout: AtomicU64,
}

impl Default for Bench {
    fn default() -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
            taken: Mutex::new(0),
            freed: Notify::new(),
            live: Arc::new(watch::channel(0).0),
            closing: AtomicBool::new(false),
            next: AtomicU32::new(1),
            mounts: Mutex::new(HashMap::new()),
            index_wait_ms: AtomicU64::new(
                u64::try_from(cases::INDEX_WAIT.as_millis()).unwrap_or(u64::MAX),
            ),
            health_every: AtomicU64::new(clusia_core::system::HEALTH_EVERY_MS),
            send_timeout: AtomicU64::new(clusia_core::system::SEND_TIMEOUT_SECS * 1000),
        }
    }
}

impl Bench {
    pub(super) fn with<R>(&self, pr: &PrRef, change: impl FnOnce(&mut Entry) -> R) -> R {
        let mut table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        change(table.entry(pr.clone()).or_default())
    }

    pub(super) fn index_wait(&self) -> Duration {
        Duration::from_millis(self.index_wait_ms.load(Ordering::SeqCst))
    }

    #[cfg(test)]
    pub(super) fn set_index_wait(&self, wait: Duration) {
        let ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        self.index_wait_ms.store(ms, Ordering::SeqCst);
    }

    pub(super) fn health_every(&self) -> Duration {
        Duration::from_millis(self.health_every.load(Ordering::SeqCst))
    }

    #[cfg(test)]
    pub(super) fn set_health_every(&self, every: Duration) {
        let ms = u64::try_from(every.as_millis()).unwrap_or(u64::MAX);
        self.health_every.store(ms, Ordering::SeqCst);
    }

    #[allow(dead_code)]
    pub(super) fn send_timeout(&self) -> Duration {
        Duration::from_millis(self.send_timeout.load(Ordering::SeqCst))
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn set_send_timeout(&self, wait: Duration) {
        let ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        self.send_timeout.store(ms, Ordering::SeqCst);
    }

    pub(super) fn seq(&self) -> u32 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }

    pub(super) fn mount(&self, tag: &str, worktree: &Path) -> Option<Result<(), String>> {
        let mounts = self.mounts.lock().unwrap_or_else(|p| p.into_inner());
        mounts
            .get(&(tag.to_string(), worktree.to_path_buf()))
            .cloned()
    }

    pub(super) fn set_mount(&self, tag: &str, worktree: &Path, result: Result<(), String>) {
        let mut mounts = self.mounts.lock().unwrap_or_else(|p| p.into_inner());
        mounts.insert((tag.to_string(), worktree.to_path_buf()), result);
    }

    /// Forgets the failed mount checks of `worktree`, so the next setup checks again.
    pub(super) fn retry_mounts(&self, worktree: &Path) {
        let mut mounts = self.mounts.lock().unwrap_or_else(|p| p.into_inner());
        mounts.retain(|(_, w), result| !(w == worktree && result.is_err()));
    }

    /// The containers and coming-up compose projects of the running activities.
    pub(super) fn containers(&self) -> Vec<String> {
        let table = self.table.lock().unwrap_or_else(|p| p.into_inner());
        table
            .values()
            .filter_map(|entry| entry.activity.as_ref()?.container.clone())
            .collect()
    }
}

/// Counts an activity or a teardown in `Bench::live` while it lives.
pub(super) struct Live(Arc<watch::Sender<usize>>);

impl Live {
    pub(super) fn new(bench: &Bench) -> Self {
        bench.live.send_modify(|n| *n += 1);
        Self(bench.live.clone())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n -= 1);
    }
}

/// A place among `tests.parallel_runs`, given back when dropped.
pub(super) struct Place(Arc<Shared>);

impl Drop for Place {
    fn drop(&mut self) {
        *self.0.tests.taken.lock().unwrap_or_else(|p| p.into_inner()) -= 1;
        self.0.tests.freed.notify_waiters();
    }
}

/// Waits for a place for activity `id` of `pr`, `Waiting` meanwhile; `None` when stopped first.
pub(super) async fn place(
    shared: &Arc<Shared>,
    pr: &PrRef,
    id: u64,
    stop: &StopFlag,
) -> Option<Place> {
    let mut told = false;
    loop {
        // Registered before the count is read, so a place freed in between is not missed.
        let freed = shared.tests.freed.notified();
        tokio::pin!(freed);
        freed.as_mut().enable();
        if stop.fired() {
            return None;
        }
        let limit = shared.config.read().await.tests.parallel_runs.max(1) as usize;
        {
            let mut taken = shared.tests.taken.lock().unwrap_or_else(|p| p.into_inner());
            if *taken < limit {
                *taken += 1;
                return Some(Place(shared.clone()));
            }
        }
        if !told {
            told = true;
            set_phase(shared, pr, id, TestsPhase::Waiting);
            tell(shared, pr).await;
        }
        tokio::select! {
            _ = &mut freed => {}
            _ = stop.notified() => return None,
        }
    }
}

/// What `begin` gives the activity it accepted.
pub(super) struct Started {
    pub id: u64,
    pub stop: Arc<StopFlag>,
    pub turn: Arc<Stop>,
    pub live: Live,
}

pub(super) enum Refused {
    Busy(Doing),
    Closing,
}

impl Refused {
    fn outcome(self, pr: &PrRef) -> Outcome {
        match self {
            Refused::Busy(doing) => reviews::invalid_state(format!(
                "The tests of {pr} are busy {}; wait for it or stop it",
                doing.label()
            )),
            Refused::Closing => reviews::invalid_state(STOPPING),
        }
    }
}

/// Takes the review's slot for `doing`, or says why not.
pub(super) fn begin(
    shared: &Shared,
    pr: &PrRef,
    doing: Doing,
    phase: TestsPhase,
) -> Result<Started, Refused> {
    shared.tests.with(pr, |entry| {
        if shared.tests.closing.load(Ordering::SeqCst) {
            return Err(Refused::Closing);
        }
        if let Some(activity) = &entry.activity {
            return Err(Refused::Busy(activity.doing));
        }
        let id = u64::from(shared.tests.seq());
        let (stop, turn) = (StopFlag::new(), Stop::new());
        // A functions activity leaves the tests' own failure for the tests' view to show.
        if doing.is_functions() {
            entry.cases_failed = None;
        } else if doing == Doing::Main {
            entry.main_failed = None;
            entry.main_step_failures.clear();
        } else if doing.is_cockpit() {
            entry.cockpit_failed = None;
        } else if doing.is_system() {
            if doing == Doing::Endpoints {
                entry.endpoints_failed = None;
            }
        } else {
            entry.failed = None;
        }
        entry.activity = Some(Activity {
            id,
            doing,
            phase,
            stop: stop.clone(),
            turn: turn.clone(),
            container: None,
            functions: None,
            cockpit: None,
        });
        Ok(Started {
            id,
            stop,
            turn,
            live: Live::new(&shared.tests),
        })
    })
}

pub(super) fn set_phase(shared: &Shared, pr: &PrRef, id: u64, phase: TestsPhase) {
    shared.tests.with(pr, |entry| {
        if let Some(activity) = entry.activity.as_mut().filter(|a| a.id == id) {
            activity.phase = phase;
        }
    });
}

pub(super) fn set_container(shared: &Shared, pr: &PrRef, id: u64, name: Option<String>) {
    shared.tests.with(pr, |entry| {
        if let Some(activity) = entry.activity.as_mut().filter(|a| a.id == id) {
            activity.container = name;
        }
    });
}

/// Ends activity `id`, leaving `failed` as the review's failure when given.
pub(super) fn end(shared: &Shared, pr: &PrRef, id: u64, failed: Option<String>) {
    shared.tests.with(pr, |entry| {
        if entry.activity.as_ref().is_some_and(|a| a.id == id) {
            entry.activity = None;
            entry.failed = failed;
        }
    });
}

/// Tells every window where the tests of `pr` stand.
pub(super) async fn tell(shared: &Shared, pr: &PrRef) {
    let view = env::view(shared, pr).await;
    shared.publish(
        topics::TESTS,
        Event::TestEnv {
            pr: pr.clone(),
            view: Box::new(view),
        },
    );
}

/// `CheckTestEngine`: detects the engine and asks the sandbox's health, as the probe does.
/// For an open review with a stored environment the engine is also read into that
/// environment, so its view moves on without an agent turn.
pub(crate) async fn check_engine(shared: &Arc<Shared>, pr: Option<PrRef>) -> Outcome {
    if shared.tests.closing.load(Ordering::SeqCst) {
        return reviews::invalid_state(STOPPING);
    }
    let exec = shared.test_exec.as_ref();
    let engine = engine::detect(exec, &env::host(shared)).await;
    let health = sandbox::health(exec, &shared.sandbox_exec_program).await;
    if let Some(pr) = pr
        && active(shared, &pr).is_ok()
        && env::load(shared, &pr).env.is_some()
        && let Ok(ctx) = env::Ctx::new(shared, &pr)
    {
        env::reprobe(shared, &ctx, &pr).await;
        tell(shared, &pr).await;
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    Outcome::Ok(Reply::TestEngine(TestEngineInfo {
        sandbox_ok: health.is_ok(),
        note: env::note_for(engine.as_ref(), &health),
        engine,
        cores: cores.max(1),
    }))
}

/// The review of `pr` when it is open with a worktree.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
pub(super) fn active(shared: &Shared, pr: &PrRef) -> Result<clusia_core::Review, Outcome> {
    match reviews::load_stored(shared, pr) {
        Ok(Some(review))
            if review.state == clusia_core::ReviewState::Active
                && shared.paths.worktree_for(pr).is_dir() =>
        {
            Ok(review)
        }
        Ok(_) => Err(reviews::invalid_state(format!(
            "no review for {pr}; open it first"
        ))),
        Err(out) => Err(out),
    }
}

/// `GetTests`.
pub(crate) async fn get(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    Outcome::Ok(Reply::Tests(Box::new(env::view(shared, pr).await)))
}

/// `GenerateTests`: probes the machine and has the agent write the recipe, in the background.
pub(crate) async fn generate(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    generate_with(shared, pr, None).await
}

/// `generate`, with what made the system fail given to the agent as data when set.
pub(super) async fn generate_with(
    shared: &Arc<Shared>,
    pr: &PrRef,
    system_failure: Option<String>,
) -> Outcome {
    let review = match active(shared, pr) {
        Ok(review) => review,
        Err(out) => return out,
    };
    let phase = TestsPhase::Generating {
        activity: Some(PROBING.into()),
    };
    let started = match begin(shared, pr, Doing::Generate, phase) {
        Ok(started) => started,
        Err(Refused::Busy(Doing::Generate)) => return Outcome::Ok(Reply::Ack),
        Err(refused) => return refused.outcome(pr),
    };
    let guard = Ending::new(shared, pr, started.id, started.live);
    // The `Generating` view goes out before the `Ack`, so a client always sees it begin.
    tell(shared, pr).await;
    cases::tell_functions(shared, pr).await;
    let id = started.id;
    let job = work::generate(
        shared.clone(),
        pr.clone(),
        review,
        id,
        started.turn.clone(),
        system_failure,
    );
    let live = guard.hand_off();
    let (shared, pr) = (shared.clone(), pr.clone());
    tokio::spawn(async move {
        work::drive(shared.clone(), pr.clone(), id, live, job).await;
        cases::tell_functions(&shared, &pr).await;
    });
    Outcome::Ok(Reply::Ack)
}

/// `PrepareTests`: records the approval, then walks the setup plan in the background.
pub(crate) async fn prepare(
    shared: &Arc<Shared>,
    pr: &PrRef,
    recipe_hash: &str,
    approve: Approve,
) -> Outcome {
    if let Err(out) = active(shared, pr) {
        return out;
    }
    let started = match begin(shared, pr, Doing::Prepare, TestsPhase::Waiting) {
        Ok(started) => started,
        Err(refused) => return refused.outcome(pr),
    };
    let guard = Ending::new(shared, pr, started.id, started.live);
    if let Err(out) = work::accept_setup(shared, pr, recipe_hash, &approve).await {
        guard.quietly();
        return out;
    }
    // The `Waiting` view goes out before the `Ack`, so a client always sees the setup begin.
    tell(shared, pr).await;
    let job = work::prepare(shared.clone(), pr.clone(), started.id, started.stop.clone());
    tokio::spawn(work::drive(
        shared.clone(),
        pr.clone(),
        started.id,
        guard.hand_off(),
        job,
    ));
    Outcome::Ok(Reply::Ack)
}

/// `RunTests`: checks everything the run needs, then runs it in the background.
pub(crate) async fn run(shared: &Arc<Shared>, pr: &PrRef, scope: TestScope) -> Outcome {
    if let Err(out) = active(shared, pr) {
        return out;
    }
    let started = match begin(shared, pr, Doing::Run, TestsPhase::Waiting) {
        Ok(started) => started,
        Err(refused) => return refused.outcome(pr),
    };
    let guard = Ending::new(shared, pr, started.id, started.live);
    let job = match work::accept_run(shared, pr, scope).await {
        Ok(job) => job,
        Err(out) => {
            guard.quietly();
            return out;
        }
    };
    let run_id = job.id.clone();
    tell(shared, pr).await;
    let task = work::run(
        shared.clone(),
        pr.clone(),
        started.id,
        started.stop.clone(),
        job,
    );
    tokio::spawn(work::drive(
        shared.clone(),
        pr.clone(),
        started.id,
        guard.hand_off(),
        task,
    ));
    Outcome::Ok(Reply::TestRunStarted { run: run_id })
}

/// `StopTests`: stops what the review is doing, if anything.
pub(crate) fn stop(shared: &Shared, pr: &PrRef) -> Outcome {
    shared.tests.with(pr, |entry| {
        if let Some(activity) = &entry.activity {
            activity.fire(Why::User);
        }
        if let Some(copilot) = &entry.copilot {
            copilot.fire(Why::User);
        }
    });
    Outcome::Ok(Reply::Ack)
}

/// `ListTests`: runs the recipe's list command and answers what it found. The list runs in a
/// task of its own, which ends the activity however it stops, even when the request is dropped.
pub(crate) async fn list(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    if let Err(out) = active(shared, pr) {
        return out;
    }
    let started = match begin(shared, pr, Doing::List, TestsPhase::Waiting) {
        Ok(started) => started,
        Err(refused) => return refused.outcome(pr),
    };
    let (shared, pr) = (shared.clone(), pr.clone());
    let task = tokio::spawn(async move {
        let _end = Ending::new(&shared, &pr, started.id, started.live);
        work::list(&shared, &pr, started.id, &started.stop).await
    });
    task.await.unwrap_or_else(|_| {
        Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            "Listing the tests stopped unexpectedly",
        ))
    })
}

/// Ends activity `id` of `pr` when dropped, and tells the windows, so that a panic or an early
/// return never leaves the review busy. `hand_off` gives the activity to `work::drive`, which
/// ends it itself.
struct Ending {
    shared: Arc<Shared>,
    pr: PrRef,
    id: u64,
    live: Option<Live>,
}

impl Ending {
    fn new(shared: &Arc<Shared>, pr: &PrRef, id: u64, live: Live) -> Self {
        Self {
            shared: shared.clone(),
            pr: pr.clone(),
            id,
            live: Some(live),
        }
    }

    fn hand_off(mut self) -> Live {
        self.live
            .take()
            .expect("an armed guard holds its live count")
    }

    /// Ends the activity now, without telling the windows, which never saw it begin.
    fn quietly(self) {
        end(&self.shared, &self.pr, self.id, None);
        drop(self.hand_off());
    }
}

impl Drop for Ending {
    fn drop(&mut self) {
        let Some(live) = self.live.take() else {
            return;
        };
        end(&self.shared, &self.pr, self.id, None);
        let (shared, pr) = (self.shared.clone(), self.pr.clone());
        tokio::spawn(async move {
            let _live = live;
            tell(&shared, &pr).await;
        });
    }
}

/// `AcceptRecipe`: the waiting proposal becomes the recipe file, Clúsia's own again.
pub(crate) async fn accept_recipe(shared: &Arc<Shared>, pr: &PrRef, recipe_hash: &str) -> Outcome {
    if let Err(out) = active(shared, pr) {
        return out;
    }
    let busy = shared.tests.with(pr, |entry| {
        if shared.tests.closing.load(Ordering::SeqCst) {
            Some(Refused::Closing)
        } else {
            entry.activity.as_ref().map(|a| Refused::Busy(a.doing))
        }
    });
    if let Some(refused) = busy {
        return refused.outcome(pr);
    }
    let worktree = match env::Ctx::new(shared, pr) {
        Ok(ctx) => ctx.worktree,
        Err(message) => return reviews::invalid_state(message),
    };
    {
        let _guard = reviews::lock(shared, pr).await;
        let mut record = env::load(shared, pr);
        let proposal = match record.proposal.take() {
            None => return reviews::invalid_state("There is no recipe proposal to accept"),
            Some(p) if p.hash != recipe_hash => {
                return reviews::invalid_state(clusia_core::tests::RECIPE_CHANGED);
            }
            Some(p) => p,
        };
        match recipe_file::write_recipe(&worktree, &proposal.text) {
            Ok(hash) => record.written_hash = Some(hash),
            Err(e) => {
                return Outcome::Err(ProtocolError::new(
                    ErrorCode::Internal,
                    format!("Cannot write {}: {e}", clusia_core::tests::RECIPE_FILE),
                ));
            }
        }
        env::save(shared, pr, &record);
    }
    shared.tests.with(pr, |entry| entry.facts = None);
    let view = env::view(shared, pr).await;
    shared.publish(
        topics::TESTS,
        Event::TestEnv {
            pr: pr.clone(),
            view: Box::new(view.clone()),
        },
    );
    Outcome::Ok(Reply::Tests(Box::new(view)))
}

/// `GetTestDisk`: what Clúsia keeps for tests, on the engine and on disk.
pub(crate) async fn disk(shared: &Shared) -> Outcome {
    if shared.tests.closing.load(Ordering::SeqCst) {
        return reviews::invalid_state(STOPPING);
    }
    let docker = env::docker_now(shared).await;
    // A pull request can commit `node_modules` as a link to `/`: links are never followed.
    let is_dir = |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    let folders = |dir: PathBuf| -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| is_dir(path))
            .collect();
        found.sort();
        found
    };
    let caches = folders(shared.paths.tests_dir().join("cache"));
    // Main's worktrees sit one level down, in a folder of their own.
    let worktree_dirs: Vec<PathBuf> = folders(shared.paths.worktrees_dir())
        .into_iter()
        .chain(folders(shared.paths.worktrees_dir().join(".main")))
        .flat_map(|worktree| [worktree.join("node_modules"), worktree.join(".venv")])
        .filter(|path| is_dir(path))
        .collect();
    let report = clusia_testrun::cleanup::disk_report(
        shared.test_exec.as_ref(),
        docker.as_ref(),
        &caches,
        &worktree_dirs,
    )
    .await;
    Outcome::Ok(Reply::TestDisk(report))
}

/// `owner/repo` with the characters a pull request's owner and repo may hold; never a path.
fn repo_name(repo: &str) -> Option<(String, String)> {
    let (owner, name) = repo.split_once('/')?;
    PrRef::new(owner, name, 1)
        .ok()
        .map(|pr| (pr.owner, pr.repo))
}

/// `CleanUpTests`: removes the images, volumes and caches of one repository, or of every one
/// with a cache folder, and the orphan containers; answers the new disk report.
pub(crate) async fn clean_up(shared: &Shared, repo: Option<String>) -> Outcome {
    if shared.tests.closing.load(Ordering::SeqCst) {
        return reviews::invalid_state(STOPPING);
    }
    let one = repo.is_some();
    let repos: Vec<(String, String)> = match repo {
        Some(repo) => match repo_name(&repo) {
            Some(name) => vec![name],
            None => {
                return reviews::invalid_state(format!("\"{repo}\" is not an owner/repo name"));
            }
        },
        None => std::fs::read_dir(shared.paths.tests_dir().join("cache"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let (owner, repo) = name.split_once('~')?;
                repo_name(&format!("{owner}/{repo}"))
            })
            .collect(),
    };
    for (owner, name) in &repos {
        let busy = {
            let table = shared.tests.table.lock().unwrap_or_else(|p| p.into_inner());
            table.iter().any(|(pr, entry)| {
                pr.owner == *owner && pr.repo == *name && entry.activity.is_some()
            })
        };
        if busy {
            return reviews::invalid_state(format!(
                "The tests of {owner}/{name} are busy; stop them first"
            ));
        }
    }
    let docker = env::docker_now(shared).await;
    for (owner, name) in &repos {
        let slug = format!("{owner}/{name}");
        if let Some(docker) = &docker
            && let Err(e) =
                clusia_testrun::cleanup::remove_repo(shared.test_exec.as_ref(), docker, &slug).await
        {
            tracing::warn!(error = %e, repo = %slug, "cannot remove the repository's test images");
        }
        remove_cache(shared, owner, name);
        forget_repo_facts(shared, owner, name);
    }
    // Only the repositories checked for busy activities above: another one's probe or stack
    // may be on its way up.
    let only = one.then(|| repos.first().map(|(owner, name)| format!("{owner}/{name}")));
    if let Some(docker) = &docker
        && let Err(e) = clusia_testrun::cleanup::remove_orphans(
            shared.test_exec.as_ref(),
            docker,
            &env::live_names(shared),
            only.flatten().as_deref(),
        )
        .await
    {
        tracing::warn!(error = %e, "cannot remove the orphan test containers");
    }
    disk(shared).await
}

fn remove_cache(shared: &Shared, owner: &str, repo: &str) {
    let cache = shared.paths.test_cache_for(owner, repo);
    if std::fs::symlink_metadata(&cache).is_ok()
        && let Err(e) = std::fs::remove_dir_all(&cache)
    {
        tracing::warn!(error = %e, "cannot remove the test cache");
    }
}

/// Drops the cached docker facts of every review of `owner/repo`: its images may be gone.
pub(super) fn forget_repo_facts(shared: &Shared, owner: &str, repo: &str) {
    let mut table = shared.tests.table.lock().unwrap_or_else(|p| p.into_inner());
    for (pr, entry) in table.iter_mut() {
        if pr.owner == owner && pr.repo == repo {
            entry.facts = None;
        }
    }
}

/// The review ended or was left: what runs is stopped for `why`, and its compose stack goes down.
/// The record stays (`forget` deletes it). Waits until the activity is gone, so the caller may
/// take the review lock after.
pub(crate) async fn stop_all(shared: &Shared, pr: &PrRef, why: Why) {
    shared.tests.with(pr, |entry| {
        if let Some(activity) = &entry.activity {
            activity.fire(why);
        }
        if let Some(copilot) = &entry.copilot {
            copilot.fire(why);
        }
    });
    let give_up = Instant::now() + STOP_WAIT;
    while !shared.tests.with(pr, |entry| entry.activity.is_none()) && Instant::now() < give_up {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    env::take_stack_down(shared, pr).await;
    shared.tests.with(pr, |entry| {
        entry.services_ready = None;
        entry.stack = None;
        entry.facts = None;
    });
}

/// The review was published or discarded: its entry, record, logs and temp folder go, and a
/// stack the record still names is taken down in the background. The caller holds the review
/// lock.
pub(crate) fn forget(shared: &Shared, pr: &PrRef) {
    let removed = shared
        .tests
        .table
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(pr);
    if let Some(activity) = removed.as_ref().and_then(|entry| entry.activity.as_ref()) {
        activity.fire(Why::Ended);
    }
    if let Some(copilot) = removed.as_ref().and_then(|entry| entry.copilot.as_ref()) {
        copilot.fire(Why::Ended);
    }
    let record = clusia_store::load_tests(&shared.paths, pr).unwrap_or_default();
    if let Err(e) = clusia_store::delete_tests(&shared.paths, pr) {
        tracing::warn!(error = %e, pr = %pr, "cannot delete the test record");
    }
    if let Ok(worktree) = std::fs::canonicalize(shared.paths.worktree_for(pr)) {
        let mut mounts = shared
            .tests
            .mounts
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        mounts.retain(|(_, w), _| *w != worktree);
    }
    remove_runs(&shared.paths.test_runs_for(pr));
    if let Some(project) = record.stack {
        env::take_down_later(shared, project);
    }
    let head = record.env.map(|env| env.head).unwrap_or_default();
    shared.publish(
        topics::TESTS,
        Event::TestEnv {
            pr: pr.clone(),
            view: Box::new(TestsView::new(head)),
        },
    );
}

/// The worktree of `pr` is gone: when it was the last of its repository, the repository's
/// images, volumes and cache folder go too. Called with `worktree_lock` held.
pub(crate) async fn after_checkout_removed(shared: &Shared, pr: &PrRef) {
    let prefix = format!("{}~{}~", pr.owner, pr.repo);
    let own = pr.file_key();
    let others = std::fs::read_dir(shared.paths.worktrees_dir())
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with(&prefix) && name != own && entry.path().is_dir()
        });
    if others {
        return;
    }
    if let Some(docker) = env::docker_now(shared).await
        && let Err(e) =
            clusia_testrun::cleanup::remove_repo(shared.test_exec.as_ref(), &docker, &pr.slug())
                .await
    {
        tracing::warn!(error = %e, repo = %pr.slug(), "cannot remove the repository's test images");
    }
    forget_repo_facts(shared, &pr.owner, &pr.repo);
    remove_cache(shared, &pr.owner, &pr.repo);
}

/// Removes the scenario run folders under `dir`, then `dir` itself.
fn remove_runs(dir: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if let Err(e) = clusia_testrun::scenario::remove_run_folder(&path) {
            tracing::warn!(error = %e, folder = %path.display(), "cannot remove a scenario run folder");
        }
    }
    match std::fs::remove_dir(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            tracing::warn!(error = %e, folder = %dir.display(), "cannot remove the scenario run folders");
        }
        _ => {}
    }
}

/// The pull requests with a stored test record, and the record's file.
fn recorded(shared: &Shared) -> Vec<(PrRef, PathBuf)> {
    std::fs::read_dir(shared.paths.tests_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let pr = name.strip_suffix(".json").and_then(PrRef::from_file_key)?;
            Some((pr, entry.path()))
        })
        .collect()
}

/// The retention sweep: records with no review older than `cutoff` go (the rule of the stored
/// check results), then the `clusia-` containers no live run or stack owns.
pub(crate) async fn sweep(shared: &Shared, cutoff: i64) {
    for (pr, file) in recorded(shared) {
        let old = || {
            !shared.is_touched(&pr.file_key())
                && !crate::retention::recently_modified(&file, cutoff)
        };
        if !old() {
            continue;
        }
        // The review lock: an open may be writing this record right now, so what was true
        // before waiting for it is checked again.
        let _guard = reviews::lock(shared, &pr).await;
        if !old() || shared.paths.review_file(&pr).exists() {
            continue;
        }
        if let Err(e) = clusia_store::delete_tests(&shared.paths, &pr) {
            tracing::warn!(error = %e, pr = %pr, "cannot delete the test record");
        }
    }
    sweep_runs(shared);
    sweep_main(shared).await;
    if let Some(docker) = env::docker_now(shared).await
        && let Err(e) = clusia_testrun::cleanup::remove_orphans(
            shared.test_exec.as_ref(),
            &docker,
            &env::live_names(shared),
            None,
        )
        .await
    {
        tracing::warn!(error = %e, "cannot remove the orphan test containers");
    }
}

/// Removes the scenario run folders of every review whose test record is gone.
fn sweep_runs(shared: &Shared) {
    let runs = shared.paths.tests_dir().join("runs");
    for entry in std::fs::read_dir(&runs).into_iter().flatten().flatten() {
        let key = entry.file_name().to_string_lossy().into_owned();
        if !shared
            .paths
            .tests_dir()
            .join(format!("{key}.json"))
            .exists()
        {
            remove_runs(&entry.path());
        }
    }
}

/// The daemon is stopping: no new activity, every running one stopped, then every stored
/// compose stack taken down.
pub(crate) async fn shutdown(shared: &Shared) {
    shared.tests.closing.store(true, Ordering::SeqCst);
    {
        let table = shared.tests.table.lock().unwrap_or_else(|p| p.into_inner());
        for entry in table.values() {
            if let Some(activity) = &entry.activity {
                activity.fire(Why::Shutdown);
            }
            if let Some(copilot) = &entry.copilot {
                copilot.fire(Why::Shutdown);
            }
        }
    }
    let mut live = shared.tests.live.subscribe();
    if tokio::time::timeout(SHUTDOWN_WAIT, live.wait_for(|n| *n == 0))
        .await
        .is_err()
    {
        tracing::warn!("stopping with test activities still running after {SHUTDOWN_WAIT:?}");
    }
    for (pr, _) in recorded(shared) {
        env::take_stack_down(shared, &pr).await;
    }
}
