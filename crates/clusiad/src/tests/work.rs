//! The activities themselves: generating the recipe, walking the setup plan, running and
//! listing the tests, and where their output goes.

use std::collections::BTreeSet;
use std::future::Future;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clusia_core::tests::{
    Approval, Approve, EnvState, Listing, MESSAGE_MAX, PlanStep, RECIPE_CHANGED, RECIPE_FILE,
    RESULTS_MAX, RecipePromptContext, RecipeProposal, ResultsFormat, RunOutcome, RunnerKind,
    STEP_ENGINE, STEP_IMAGE, STEP_SERVICES, STEP_STACK, STEP_TESTS, SetupPlan, StepState, TAIL_MAX,
    TestCaseResult, TestRun, TestScope, TestStatus, TestsPhase, Totals, keep_runs,
};
use clusia_core::{PrRef, Review};
use clusia_protocol::{ErrorCode, Event, Outcome, ProtocolError, Reply, topics};
use clusia_testrun::container::{self, Marker, Stack};
use clusia_testrun::docker::{self, ContainerJob, ContainerNet, Docker, RunMounts};
use clusia_testrun::exec::{CommandSpec, Ended, StopFlag, Stream};
use clusia_testrun::image::{self, ImagePlan};
use clusia_testrun::log::{CappedLog, Tail};
use clusia_testrun::names::{self, LabelKind};
use clusia_testrun::recipe::{Recipe, commands_for, recipe_hash};
use clusia_testrun::results::{ResultParser, parse_list, parser_for};
use clusia_testrun::sandbox::{self, NetMode};
use clusia_testrun::{compose, engine, local};

use super::env::{self, Ctx, RecipeState, Snapshot};
use super::recipe_file::{self, Adopt};
use super::recipe_turn;
use super::{GENERATE_FIRST, Live, NO_LIST, NO_NEAR, NO_RECIPE, SET_UP_FIRST, WRITING};
use crate::reviews;
use crate::state::Shared;
use crate::sync::now_unix;
use crate::turns::Stop;

/// How many cores the machine has, for `tests.cpus = 0`; 1 when it cannot say.
fn cores() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX))
}

/// How often the engine is asked whether it is up after its start command.
const ENGINE_POLL: Duration = Duration::from_secs(2);
/// How long a started engine may take to answer.
const ENGINE_WAIT: Duration = Duration::from_secs(120);
/// The longest a list command may run.
const LIST_MAX_SECS: u32 = 300;
/// How much of a run's stdout is kept for the parsers that read it whole (jest).
const STDOUT_KEPT: usize = clusia_core::tests::LOG_TAIL;
/// A line longer than this is fed to the parser as it is.
const LINE_MAX: usize = 64 * 1024;
const OUTPUT_EVERY: Duration = Duration::from_millis(clusia_core::tests::OUTPUT_EVERY_MS);
/// The largest compose file read before `docker compose` sees it.
const COMPOSE_MAX: u64 = 256 * 1024;

/// Runs one activity to its end in a task of its own, and settles it whatever happens inside:
/// an error is the review's failure, a panic is said as one.
pub(super) async fn drive(
    shared: Arc<Shared>,
    pr: PrRef,
    id: u64,
    live: Live,
    job: impl Future<Output = Result<(), String>> + Send + 'static,
) {
    let failed = match tokio::spawn(job).await {
        Ok(Ok(())) => None,
        Ok(Err(message)) => Some(message),
        Err(_) => {
            tracing::error!(pr = %pr, "a test activity failed inside the daemon");
            Some("The test activity stopped unexpectedly".to_string())
        }
    };
    super::end(&shared, &pr, id, failed);
    drop(live);
    super::tell(&shared, &pr).await;
}

/// What a command's output feeds besides the log.
pub(super) enum To {
    /// `TestStep` Running events with the tail.
    Step(String),
    /// `TestOutput` and `TestResult` events of the run.
    Run(String),
    /// No events (a list).
    Quiet,
}

/// Where a running command's output goes: its log, the tail, the result parser and the events,
/// at most one tail every `OUTPUT_EVERY`.
pub(super) struct Sink<'a> {
    shared: &'a Shared,
    pr: &'a PrRef,
    to: To,
    log: Option<CappedLog>,
    tail: Tail,
    /// The unfinished line of each stream.
    lines: [Vec<u8>; 2],
    stdout: Vec<u8>,
    parser: Option<Box<dyn ResultParser>>,
    live: Vec<TestCaseResult>,
    sent: Option<Instant>,
    unsent: bool,
}

impl<'a> Sink<'a> {
    pub fn new(
        shared: &'a Shared,
        pr: &'a PrRef,
        to: To,
        log: Option<CappedLog>,
        parser: Option<Box<dyn ResultParser>>,
    ) -> Self {
        Self {
            shared,
            pr,
            to,
            log,
            tail: Tail::new(TAIL_MAX),
            lines: [Vec::new(), Vec::new()],
            stdout: Vec::new(),
            parser,
            live: Vec::new(),
            sent: None,
            unsent: false,
        }
    }

    pub fn chunk(&mut self, stream: Stream, bytes: &[u8]) {
        if let Some(log) = self.log.as_mut()
            && let Err(e) = log.write(bytes)
        {
            tracing::warn!(error = %e, "cannot write the test log");
            self.log = None;
        }
        self.tail.push(bytes);
        if stream == Stream::Out {
            let room = STDOUT_KEPT.saturating_sub(self.stdout.len());
            self.stdout
                .extend_from_slice(&bytes[..bytes.len().min(room)]);
        }
        let at = usize::from(stream == Stream::Err);
        self.lines[at].extend_from_slice(bytes);
        while let Some(end) = self.lines[at].iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.lines[at].drain(..=end).collect();
            self.feed(&line[..line.len() - 1]);
        }
        if self.lines[at].len() > LINE_MAX {
            let line = std::mem::take(&mut self.lines[at]);
            self.feed(&line);
        }
        self.unsent = true;
        if self.sent.is_none_or(|at| at.elapsed() >= OUTPUT_EVERY) {
            self.send();
        }
    }

    fn feed(&mut self, line: &[u8]) {
        let Some(parser) = self.parser.as_mut() else {
            return;
        };
        let text = String::from_utf8_lossy(line).into_owned();
        for result in parser.line(text.strip_suffix('\r').unwrap_or(&text)) {
            if self.live.len() >= RESULTS_MAX {
                break;
            }
            if let To::Run(run) = &self.to {
                self.shared.publish(
                    topics::TESTS,
                    Event::TestResult {
                        pr: self.pr.clone(),
                        run: run.clone(),
                        result: result.clone(),
                    },
                );
            }
            self.live.push(result);
        }
    }

    fn send(&mut self) {
        self.sent = Some(Instant::now());
        self.unsent = false;
        let tail = self.tail.text();
        let event = match &self.to {
            To::Step(step) => Event::TestStep {
                pr: self.pr.clone(),
                step: step.clone(),
                state: StepState::Running,
                tail: Some(tail),
            },
            To::Run(run) => Event::TestOutput {
                pr: self.pr.clone(),
                run: run.clone(),
                tail,
            },
            To::Quiet => return,
        };
        self.shared.publish(topics::TESTS, event);
    }

    /// Feeds the unfinished lines and sends the tail when something new is unsent.
    pub fn flush(&mut self) {
        for at in 0..2 {
            let rest = std::mem::take(&mut self.lines[at]);
            if !rest.is_empty() {
                self.feed(&rest);
            }
        }
        if self.unsent {
            self.send();
        }
    }

    pub fn tail(&self) -> String {
        self.tail.text()
    }

    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// The run's results (the parser's full list, else what the lines gave), its tail, and
    /// whether the log was written.
    pub fn finish(mut self, worktree: &Path) -> (Vec<TestCaseResult>, String, bool) {
        let stdout = self.stdout();
        let mut results = self
            .parser
            .as_mut()
            .map(|p| p.finish(worktree, &stdout))
            .unwrap_or_default();
        if results.is_empty() {
            results = std::mem::take(&mut self.live);
        }
        results.truncate(RESULTS_MAX);
        for result in &mut results {
            if let Some(message) = result.message.as_mut() {
                cut(message, MESSAGE_MAX);
            }
        }
        let logged = match self.log.take().map(CappedLog::finish) {
            Some(Ok(_)) => true,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "cannot finish the test log");
                false
            }
            None => false,
        };
        (results, self.tail.text(), logged)
    }
}

/// Cuts `text` to at most `max` bytes, at a character boundary.
fn cut(text: &mut String, max: usize) {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}

fn not_alive(message: String) -> Outcome {
    reviews::invalid_state(message)
}

/// What a request that runs recipe text needs, checked in the order the reviewer fixes them:
/// the pull request's own `.clusia/`, the probe, the recipe, a blocked plan.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
fn usable(snap: Snapshot) -> Result<Usable, Outcome> {
    if let Some(problem) = snap.shipped {
        return Err(not_alive(problem));
    }
    let Some(env) = snap.env else {
        return Err(not_alive(GENERATE_FIRST.into()));
    };
    let recipe = match snap.recipe {
        RecipeState::Missing => return Err(not_alive(NO_RECIPE.into())),
        RecipeState::Problem(message) => return Err(not_alive(message)),
        RecipeState::Valid { recipe, .. } => *recipe,
    };
    let (Some(ctx), Some(plan)) = (snap.ctx, snap.plan) else {
        return Err(not_alive(GENERATE_FIRST.into()));
    };
    if let Some(blocked) = &plan.blocked {
        return Err(not_alive(blocked.clone()));
    }
    let image = snap.cached.and_then(|c| c.image).and_then(Result::ok);
    Ok(Usable {
        head: snap.head,
        ctx,
        env,
        recipe,
        plan,
        image,
    })
}

struct Usable {
    head: String,
    ctx: Ctx,
    env: EnvState,
    recipe: Recipe,
    plan: SetupPlan,
    image: Option<ImagePlan>,
}

/// The recipe turn and its adoption.
pub(super) async fn generate(
    shared: Arc<Shared>,
    pr: PrRef,
    review: Review,
    id: u64,
    turn: Arc<Stop>,
) -> Result<(), String> {
    let ctx = Ctx::new(&shared, &pr)?;
    if let Some(problem) = env::refresh_shipped(&shared, &ctx).await {
        return Err(problem);
    }
    let env = env::probe(&shared, &ctx, &review.head_sha).await;
    let stored = env.clone();
    if !env::update_record(&shared, &pr, |record| record.env = Some(stored)).await {
        return Ok(());
    }
    shared.tests.with(&pr, |entry| entry.facts = None);
    super::set_phase(
        &shared,
        &pr,
        id,
        TestsPhase::Generating {
            activity: Some(WRITING.into()),
        },
    );
    super::tell(&shared, &pr).await;

    let files = shared
        .review_caches
        .get(&pr)
        .await
        .map(|cache| cache.files.iter().map(|f| f.path.clone()).collect())
        .unwrap_or_default();
    let current = recipe_file::read_recipe(&ctx.worktree)?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let prompt = RecipePromptContext {
        pr_title: review.title.clone(),
        files,
        forked: false,
        env: env.clone(),
        current,
        cases: false,
    };
    let answer = match recipe_turn::run_recipe_turn(&shared, &pr, prompt, turn.clone()).await {
        Ok(answer) => answer,
        Err(_) if turn.fired() => return Ok(()),
        Err(message) => return Err(message),
    };
    if answer.unreadable > 0 {
        tracing::warn!(pr = %pr, unreadable = answer.unreadable, "the recipe answer held blocks that could not be read");
    }
    let near = answer.near;
    let recipe = answer
        .recipe
        .map_err(|reasons| format!("The agent's recipe is not valid: {}", reasons.join("; ")))?;
    let text = recipe.to_toml();

    let _guard = reviews::lock(&shared, &pr).await;
    if turn.fired() || !env::alive(&shared, &pr) {
        return Ok(());
    }
    let mut record = env::load(&shared, &pr);
    let file = recipe_file::read_recipe(&ctx.worktree)?;
    match recipe_file::adopt(file.as_deref(), record.written_hash.as_deref()) {
        Adopt::Write => {
            let hash = recipe_file::write_recipe(&ctx.worktree, &text)
                .map_err(|e| format!("Cannot write {RECIPE_FILE}: {e}"))?;
            record.written_hash = Some(hash);
            record.proposal = None;
        }
        Adopt::Propose => {
            let hash = recipe_hash(text.as_bytes());
            let same = file.is_some_and(|file| recipe_hash(&file) == hash);
            record.proposal = (!same).then(|| RecipeProposal {
                text,
                hash,
                at: now_unix(),
            });
        }
    }
    record.agent_near = near;
    record.generated_inputs = Some(env.inputs_hash.clone());
    record.generated_at = Some(now_unix());
    env::save(&shared, &pr, &record);
    shared.tests.with(&pr, |entry| entry.facts = None);
    Ok(())
}

/// Checks a `PrepareTests` against the plan the reviewer saw, and records the approval.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
pub(super) async fn accept_setup(
    shared: &Arc<Shared>,
    pr: &PrRef,
    recipe_hash: &str,
    approve: &Approve,
) -> Result<(), Outcome> {
    let ctx = Ctx::new(shared, pr).map_err(not_alive)?;
    env::refresh_shipped(shared, &ctx).await;
    env::reprobe(shared, &ctx, pr).await;
    // A "try again" checks the mounts again.
    shared.tests.retry_mounts(&ctx.worktree);
    let snap = env::snapshot(shared, pr).await;
    let plan = usable(snap)?.plan;
    if recipe_hash != plan.recipe_hash {
        return Err(not_alive(RECIPE_CHANGED.into()));
    }
    let wanted: BTreeSet<String> = match approve {
        Approve::All => plan
            .steps
            .iter()
            .map(|s| s.id.clone())
            .chain([STEP_TESTS.to_string()])
            .collect(),
        Approve::Steps(ids) => {
            for id in ids {
                if id != STEP_TESTS && !plan.steps.iter().any(|s| s.id == *id) {
                    return Err(not_alive(format!(
                        "There is no step {id} in the setup plan"
                    )));
                }
            }
            ids.iter().cloned().collect()
        }
    };
    let hash = plan.recipe_hash.clone();
    env::update_record(shared, pr, |record| {
        let steps = match record.approval.take() {
            Some(old) if old.recipe_hash == hash => old.steps.into_iter().chain(wanted).collect(),
            _ => wanted,
        };
        record.approval = Some(Approval {
            recipe_hash: hash,
            steps,
            at: now_unix(),
        });
    })
    .await;
    shared.tests.with(pr, |entry| entry.facts = None);
    Ok(())
}

/// Walks the setup plan with a place: the mount check as soon as the image exists, then each
/// step that is missing or failed, as far as the reviewer approved.
pub(super) async fn prepare(
    shared: Arc<Shared>,
    pr: PrRef,
    id: u64,
    stop: Arc<StopFlag>,
) -> Result<(), String> {
    let Some(_place) = super::place(&shared, &pr, id, &stop).await else {
        return Ok(());
    };
    let mut tried: BTreeSet<String> = BTreeSet::new();
    while !stop.fired() {
        let snap = env::snapshot(&shared, &pr).await;
        let cached = snap.cached.clone();
        let (Some(plan), Some(ctx), Some(env), RecipeState::Valid { recipe, hash }) =
            (snap.plan, snap.ctx, snap.env, snap.recipe)
        else {
            return Err(
                "The recipe or the environment changed during the setup; generate the tests again"
                    .into(),
            );
        };
        if plan.blocked.is_some() {
            break;
        }
        // The mount check comes as soon as the image exists, before any work step, so a folder
        // the engine can't see is said as that and not as a step's exit code.
        if env.runner == RunnerKind::Container
            && let Some(cached) = cached.as_ref().filter(|c| c.image_present)
            && let Some(Ok(image)) = cached.image.clone()
            && shared.tests.mount(&image.tag, &ctx.worktree).is_none()
        {
            mount(&shared, &pr, id, &ctx, &env, &image.tag).await;
            continue;
        }
        let next = plan
            .steps
            .iter()
            .find(|s| matches!(s.state, StepState::Missing | StepState::Failed { .. }))
            .cloned();
        let Some(step) = next else {
            break;
        };
        if !step.approved {
            break;
        }
        if !tried.insert(step.id.clone()) {
            failed(
                &shared,
                &pr,
                &step.id,
                "The step finished, but what it leaves behind is not there".into(),
                None,
            );
            break;
        }
        super::set_phase(
            &shared,
            &pr,
            id,
            TestsPhase::Preparing {
                step: step.id.clone(),
            },
        );
        super::tell(&shared, &pr).await;
        say_step(&shared, &pr, &step.id, StepState::Running, None);
        let walk = Walk {
            shared: &shared,
            pr: &pr,
            id,
            ctx: &ctx,
            env: &env,
            recipe: &recipe,
            hash: &hash,
            stop: &stop,
        };
        match run_step(&walk, &step).await {
            StepEnd::Done => {
                shared.tests.with(&pr, |entry| {
                    entry.step_failures.remove(&step.id);
                    entry.facts = None;
                });
                say_step(&shared, &pr, &step.id, StepState::Done, None);
            }
            StepEnd::Failed(message, tail) => {
                failed(&shared, &pr, &step.id, message, Some(tail));
                break;
            }
            StepEnd::Stopped => {
                say_step(&shared, &pr, &step.id, StepState::Missing, None);
                break;
            }
        }
    }
    Ok(())
}

fn say_step(shared: &Shared, pr: &PrRef, step: &str, state: StepState, tail: Option<String>) {
    shared.publish(
        topics::TESTS,
        Event::TestStep {
            pr: pr.clone(),
            step: step.to_string(),
            state,
            tail,
        },
    );
}

/// Keeps why `step` failed until it runs again, and says so.
fn failed(shared: &Shared, pr: &PrRef, step: &str, message: String, tail: Option<String>) {
    shared.tests.with(pr, |entry| {
        entry
            .step_failures
            .insert(step.to_string(), message.clone());
        entry.facts = None;
    });
    say_step(shared, pr, step, StepState::Failed { message }, tail);
}

/// The mount check of `tag` for this worktree, kept for the daemon's session.
async fn mount(shared: &Shared, pr: &PrRef, id: u64, ctx: &Ctx, env: &EnvState, tag: &str) {
    super::set_phase(
        shared,
        pr,
        id,
        TestsPhase::Preparing {
            step: "mount".into(),
        },
    );
    super::tell(shared, pr).await;
    let result = match &env.engine {
        Some(engine) => {
            let docker = env::docker_for(engine, &ctx.host);
            let mounts = RunMounts {
                worktree: ctx.worktree.clone(),
                git_common: ctx.git_common.clone(),
                caches: Vec::new(),
            };
            let name = names::run_name(&ctx.slug, pr.number, shared.tests.seq());
            // Live before it starts, so no orphan pass removes the probe mid-check.
            super::set_container(shared, pr, id, Some(name.clone()));
            let checked = container::mount_check(
                shared.test_exec.as_ref(),
                &docker,
                engine,
                tag,
                &mounts,
                &ctx.labels(LabelKind::Probe),
                &name,
            )
            .await;
            super::set_container(shared, pr, id, None);
            checked
        }
        None => Err("No container engine found.".into()),
    };
    shared.tests.set_mount(tag, &ctx.worktree, result);
    shared.tests.with(pr, |entry| entry.facts = None);
}

/// What every step of one walk reads.
struct Walk<'a> {
    shared: &'a Arc<Shared>,
    pr: &'a PrRef,
    id: u64,
    ctx: &'a Ctx,
    env: &'a EnvState,
    recipe: &'a Recipe,
    hash: &'a str,
    stop: &'a StopFlag,
}

enum StepEnd {
    Done,
    /// Why, and the output's tail.
    Failed(String, String),
    Stopped,
}

fn step_end(ended: io::Result<Ended>, limit: Duration, tail: String) -> StepEnd {
    let message = match ended {
        Ok(Ended::Exited(Some(0))) => return StepEnd::Done,
        Ok(Ended::Stopped) => return StepEnd::Stopped,
        Ok(Ended::Exited(Some(code))) => format!("It exited with code {code}"),
        Ok(Ended::Exited(None)) => "It was ended by a signal".to_string(),
        Ok(Ended::TimedOut) => format!(
            "It ran past its limit of {}s and was stopped",
            limit.as_secs()
        ),
        Err(e) => format!("It could not start: {e}"),
    };
    StepEnd::Failed(message, tail)
}

fn docker_of(walk: &Walk<'_>) -> Result<Docker, StepEnd> {
    match &walk.env.engine {
        Some(engine) => Ok(env::docker_for(engine, &walk.ctx.host)),
        None => Err(StepEnd::Failed(
            "No container engine found.".into(),
            String::new(),
        )),
    }
}

fn image_of(walk: &Walk<'_>) -> Result<ImagePlan, String> {
    image::plan_image(
        walk.recipe,
        &walk.ctx.worktree,
        &walk.ctx.slug,
        &walk.ctx.labels(LabelKind::Image),
        &walk.env.inputs_hash,
    )
}

async fn run_step(walk: &Walk<'_>, step: &PlanStep) -> StepEnd {
    let shared = walk.shared;
    let exec = shared.test_exec.as_ref();
    let limit = Duration::from_secs(u64::from(shared.config.read().await.tests.run_timeout_secs));
    let mut sink = Sink::new(shared, walk.pr, To::Step(step.id.clone()), None, None);
    let id = step.id.as_str();
    if id == STEP_ENGINE {
        return engine_step(walk, &mut sink).await;
    }
    if id == STEP_IMAGE {
        let docker = match docker_of(walk) {
            Ok(found) => found,
            Err(end) => return end,
        };
        let image = match image_of(walk) {
            Ok(image) => image,
            Err(message) => return StepEnd::Failed(message, String::new()),
        };
        let build_dir = env::build_dir(shared, walk.pr);
        if let Ok(roots) = env::roots(shared, walk.ctx) {
            debug_assert!(!sandbox::writable_by_sandbox(&roots, &build_dir));
        }
        let ended = container::build_image(
            exec,
            &docker,
            &image,
            &build_dir,
            &walk.ctx.labels(LabelKind::Image),
            limit,
            walk.stop,
            &mut |stream, bytes: &[u8]| sink.chunk(stream, bytes),
        )
        .await;
        sink.flush();
        return step_end(ended, limit, sink.tail());
    }
    if id == STEP_SERVICES {
        let docker = match docker_of(walk) {
            Ok(found) => found,
            Err(end) => return end,
        };
        let stack = match compose_stack(walk, &docker).await {
            Ok(stack) => stack,
            Err(reasons) => {
                let hash = walk.hash.to_string();
                shared.tests.with(walk.pr, |entry| {
                    entry.compose_refused = Some((hash, reasons.clone()));
                });
                return StepEnd::Failed(reasons, String::new());
            }
        };
        let ended = container::stack_services(
            exec,
            &docker,
            &stack,
            limit,
            walk.stop,
            &mut |stream, bytes: &[u8]| sink.chunk(stream, bytes),
        )
        .await;
        sink.flush();
        if matches!(ended, Ok(Ended::Exited(Some(0)))) {
            let hash = walk.hash.to_string();
            shared.tests.with(walk.pr, |entry| {
                entry.services_ready = Some(hash.clone());
                entry.stack = Some((hash, stack));
            });
        }
        return step_end(ended, limit, sink.tail());
    }
    if id == STEP_STACK {
        let docker = match docker_of(walk) {
            Ok(found) => found,
            Err(end) => return end,
        };
        let kept = shared.tests.with(walk.pr, |entry| {
            entry
                .stack
                .clone()
                .filter(|(hash, _)| hash == walk.hash)
                .map(|(_, stack)| stack)
        });
        // After a restart the stack is built again from the compose file.
        let stack = match kept {
            Some(stack) => stack,
            None => match compose_stack(walk, &docker).await {
                Ok(stack) => stack,
                Err(reasons) => return StepEnd::Failed(reasons, String::new()),
            },
        };
        // The project is live while it comes up; the record names it once it is up.
        super::set_container(shared, walk.pr, walk.id, Some(stack.project.clone()));
        let ended = container::stack_up(
            exec,
            &docker,
            &stack,
            limit,
            walk.stop,
            &mut |stream, bytes: &[u8]| sink.chunk(stream, bytes),
        )
        .await;
        sink.flush();
        if matches!(ended, Ok(Ended::Exited(Some(0)))) {
            let project = stack.project.clone();
            env::update_record(shared, walk.pr, |record| record.stack = Some(project)).await;
        }
        super::set_container(shared, walk.pr, walk.id, None);
        return step_end(ended, limit, sink.tail());
    }
    if let Some(at) = id
        .strip_prefix("work:")
        .and_then(|n| n.parse::<usize>().ok())
        && let Some(setup) = walk.recipe.setup.get(at)
    {
        let ended = work_step(walk, step, setup, limit, &mut sink).await;
        sink.flush();
        return step_end(ended, limit, sink.tail());
    }
    StepEnd::Failed(
        format!("Clúsia does not know how to run the step {id}"),
        String::new(),
    )
}

/// Starts the engine with its start command, word by word, then asks until it answers.
async fn engine_step(walk: &Walk<'_>, sink: &mut Sink<'_>) -> StepEnd {
    let shared = walk.shared;
    let exec = shared.test_exec.as_ref();
    let Some(engine) = walk.env.engine.clone() else {
        return StepEnd::Failed("No container engine found.".into(), String::new());
    };
    let Some(start) = engine.start.clone() else {
        return StepEnd::Failed("Start your container engine.".into(), String::new());
    };
    let mut words = start.split_whitespace();
    let Some(first) = words.next() else {
        return StepEnd::Failed("Start your container engine.".into(), String::new());
    };
    // `open` is the system's own; any other program is found on the engine CLI's `PATH`.
    let program = if first == "open" {
        "/usr/bin/open"
    } else {
        first
    };
    let mut cmd = CommandSpec::new(program).args(words).clear_env();
    for (key, value) in engine::cli_env(Path::new(&engine.cli), &walk.ctx.host) {
        cmd = cmd.env(key, value);
    }
    let ended = exec
        .stream(&cmd, ENGINE_WAIT, walk.stop, &mut |stream, bytes: &[u8]| {
            sink.chunk(stream, bytes)
        })
        .await;
    sink.flush();
    if !matches!(ended, Ok(Ended::Exited(Some(0)))) {
        return step_end(ended, ENGINE_WAIT, sink.tail());
    }
    // A `tokio` deadline, so a paused test clock drives the polls.
    let deadline = tokio::time::Instant::now() + ENGINE_WAIT;
    loop {
        if walk.stop.fired() {
            return StepEnd::Stopped;
        }
        if engine::is_running(exec, &engine, &walk.ctx.host).await {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return StepEnd::Failed(
                format!(
                    "{} did not start within {} s",
                    container::engine_name(engine.kind),
                    ENGINE_WAIT.as_secs()
                ),
                sink.tail(),
            );
        }
        tokio::select! {
            _ = tokio::time::sleep(ENGINE_POLL) => {}
            _ = walk.stop.notified() => return StepEnd::Stopped,
        }
    }
    env::rewrite_record(shared, walk.pr, |record| {
        if let Some(env) = record.env.as_mut() {
            if let Some(engine) = env.engine.as_mut() {
                engine.running = true;
            }
            env.note = None;
        }
    })
    .await;
    shared.tests.with(walk.pr, |entry| entry.facts = None);
    StepEnd::Done
}

/// A `work` step, in a container of the recipe's image or in the local sandbox; its marker is
/// written by Clúsia after an exit 0.
async fn work_step(
    walk: &Walk<'_>,
    step: &PlanStep,
    setup: &clusia_testrun::recipe::SetupStep,
    limit: Duration,
    sink: &mut Sink<'_>,
) -> io::Result<Ended> {
    let shared = walk.shared;
    let exec = shared.test_exec.as_ref();
    let (ctx, env) = (walk.ctx, walk.env);
    let mut chunk = |stream, bytes: &[u8]| sink.chunk(stream, bytes);
    if env.runner == RunnerKind::Container {
        let Some(engine) = &env.engine else {
            return Err(io::Error::other("no container engine found"));
        };
        let docker = env::docker_for(engine, &ctx.host);
        let image = image_of(walk).map_err(io::Error::other)?;
        let marker = clusia_testrun::plan::work_marker(setup, Some(&image.tag), &env.inputs_hash);
        let config = shared.config.read().await.tests.clone();
        let name = names::run_name(&ctx.slug, walk.pr.number, shared.tests.seq());
        let job = ContainerJob {
            name: name.clone(),
            image: image.tag.clone(),
            command: setup.run.clone(),
            env: docker::job_env(&env.signals.ecosystems, &walk.recipe.tests.env),
            net: if setup.network {
                ContainerNet::Bridge
            } else {
                ContainerNet::None
            },
            cpus: Some(config.cpus_for(cores())),
            memory_mb: Some(config.memory_mb),
            labels: ctx.labels(LabelKind::Run),
        };
        super::set_container(shared, walk.pr, walk.id, Some(name));
        let ended = container::run_work_step(
            exec,
            &docker,
            &job,
            &mounts(ctx, env),
            Marker {
                worktree: &ctx.worktree,
                step: &step.id,
                text: &marker,
            },
            limit,
            walk.stop,
            &mut chunk,
        )
        .await;
        super::set_container(shared, walk.pr, walk.id, None);
        return ended;
    }
    let mode = if setup.network {
        NetMode::Online
    } else {
        NetMode::Offline
    };
    let roots = env::roots(shared, ctx)?;
    let profile = env::profile_file(shared, ctx, mode)?;
    let cmd = sandbox::local_command(
        &shared.sandbox_exec_program,
        &profile,
        &setup.run,
        sandbox::base_env(
            &roots,
            &env::local_path(shared),
            &env.signals.ecosystems,
            &walk.recipe.tests.env,
        ),
        &ctx.worktree,
    );
    let ended = local::run_local(exec, &cmd, limit, walk.stop, &mut chunk).await?;
    if ended == Ended::Exited(Some(0)) {
        let marker = clusia_testrun::plan::work_marker(setup, None, &env.inputs_hash);
        container::write_marker(&ctx.worktree, &step.id, &marker)?;
    }
    Ok(ended)
}

fn mounts(ctx: &Ctx, env: &EnvState) -> RunMounts {
    RunMounts {
        worktree: ctx.worktree.clone(),
        git_common: ctx.git_common.clone(),
        caches: docker::cache_mounts(&ctx.slug, &env.signals.ecosystems),
    }
}

/// The compose file, read as text before `docker compose` ever sees it: the folder walk and the
/// open follow no link, the size is capped, and `include` and `extends` are refused because
/// `docker compose config` would fetch or read what they name.
fn compose_text(worktree: &Path, file: &str) -> Result<String, String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let unreadable = |why: &str| format!("Cannot read the compose file {file}: {why}");
    let mut at = worktree.to_path_buf();
    for part in Path::new(file).components() {
        at.push(part);
        let meta = std::fs::symlink_metadata(&at).map_err(|e| unreadable(&e.to_string()))?;
        if meta.file_type().is_symlink() {
            return Err(unreadable("it goes through a link"));
        }
    }
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&at)
        .map_err(|e| unreadable(&e.to_string()))?;
    if !opened
        .metadata()
        .map_err(|e| unreadable(&e.to_string()))?
        .is_file()
    {
        return Err(unreadable("it is not a file"));
    }
    let mut text = String::new();
    opened
        .take(COMPOSE_MAX + 1)
        .read_to_string(&mut text)
        .map_err(|e| unreadable(&e.to_string()))?;
    if text.len() as u64 > COMPOSE_MAX {
        return Err(unreadable("it is larger than 256 KiB"));
    }
    let follows = text
        .lines()
        .any(|line| names_key(line, "include") || names_key(line, "extends"));
    if follows {
        return Err(
            "The compose file uses include or extends, which Clúsia does not follow".into(),
        );
    }
    Ok(text)
}

/// Whether `line` holds `name` as a mapping key anywhere: after the start, a space, `{` or `,`,
/// quoted or not, then `:`. That covers block keys, flow mappings and a file written as JSON on
/// one line; a value that merely reads like a key is refused too, which only blocks the plan.
fn names_key(line: &str, name: &str) -> bool {
    line.match_indices(name).any(|(at, _)| {
        let before = line[..at].strip_suffix(['"', '\'']).unwrap_or(&line[..at]);
        let after = &line[at + name.len()..];
        let after = after.strip_prefix(['"', '\'']).unwrap_or(after);
        before
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || c == '{' || c == ',')
            && after.trim_start().starts_with(':')
    })
}

/// The recipe's compose stack: the file read as text, then `docker compose config`, its
/// refusals and the override Clúsia writes for it.
async fn compose_stack(walk: &Walk<'_>, docker: &Docker) -> Result<Stack, String> {
    let shared = walk.shared;
    let ctx = walk.ctx;
    let Some(services) = &walk.recipe.services else {
        return Err("The recipe names no services".into());
    };
    compose_text(&ctx.worktree, &services.compose)?;
    let project = names::compose_project(&ctx.slug, walk.pr.number);
    let merged = container::compose_config(
        shared.test_exec.as_ref(),
        docker,
        &ctx.worktree,
        &services.compose,
        &project,
    )
    .await?;
    let problems = compose::compose_problems(&merged, &ctx.worktree, &services.names);
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    let override_text =
        compose::compose_override(&merged, &project, &ctx.labels(LabelKind::Stack))?;
    let stack = Stack {
        project,
        worktree: ctx.worktree.clone(),
        compose_file: services.compose.clone(),
        override_file: env::override_file(shared, walk.pr),
        override_text,
        services: services.names.clone(),
    };
    if let Ok(roots) = env::roots(shared, ctx) {
        debug_assert!(!sandbox::writable_by_sandbox(&roots, &stack.override_file));
    }
    Ok(stack)
}

/// A run the request accepted.
pub(super) struct RunJob {
    pub id: String,
    pub scope: TestScope,
    pub commands: Vec<String>,
    pub head: String,
    pub ctx: Ctx,
    pub env: EnvState,
    pub recipe: Recipe,
    /// The recipe's image, for the container runner.
    pub image: Option<String>,
    pub network: ContainerNet,
}

/// What a run or a list needs: everything `usable` checks, a plan that is ready, and an engine
/// asked again.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
async fn ready_to_run(shared: &Arc<Shared>, pr: &PrRef) -> Result<Usable, Outcome> {
    let ctx = Ctx::new(shared, pr).map_err(not_alive)?;
    env::refresh_shipped(shared, &ctx).await;
    env::reprobe(shared, &ctx, pr).await;
    // "Ready" is asked of docker again, so an image removed since is not run.
    shared.tests.with(pr, |entry| entry.facts = None);
    let usable = usable(env::snapshot(shared, pr).await)?;
    if !usable.plan.ready {
        return Err(not_alive(SET_UP_FIRST.into()));
    }
    Ok(usable)
}

fn job_of(usable: Usable, id: String, scope: TestScope, commands: Vec<String>) -> RunJob {
    let network = match &usable.recipe.services {
        Some(_) => ContainerNet::Internal(compose::internal_network(&names::compose_project(
            &usable.ctx.slug,
            usable.ctx.pr.number,
        ))),
        None => ContainerNet::None,
    };
    RunJob {
        id,
        scope,
        commands,
        head: usable.head,
        image: usable.image.map(|image| image.tag),
        ctx: usable.ctx,
        env: usable.env,
        recipe: usable.recipe,
        network,
    }
}

/// Checks a `RunTests` and gives the run it will be.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
pub(super) async fn accept_run(
    shared: &Arc<Shared>,
    pr: &PrRef,
    scope: TestScope,
) -> Result<RunJob, Outcome> {
    let usable = ready_to_run(shared, pr).await?;
    let near: Vec<String> = if scope == TestScope::NearChange {
        let record = env::load(shared, pr);
        let near: Vec<String> = env::near_list(shared, pr, &record)
            .await
            .into_iter()
            .map(|test| test.name)
            .collect();
        if near.is_empty() {
            return Err(not_alive(NO_NEAR.into()));
        }
        near
    } else {
        Vec::new()
    };
    let commands = commands_for(&usable.recipe.tests, &scope, &near).map_err(not_alive)?;
    let seq = shared.tests.seq();
    Ok(job_of(
        usable,
        format!("{}-{seq}", now_unix()),
        scope,
        commands,
    ))
}

/// Runs each command of `job` in turn within one deadline, and stores the run.
pub(super) async fn run(
    shared: Arc<Shared>,
    pr: PrRef,
    act: u64,
    stop: Arc<StopFlag>,
    job: RunJob,
) -> Result<(), String> {
    let limit = Duration::from_secs(u64::from(shared.config.read().await.tests.run_timeout_secs));
    let mut run = TestRun {
        id: job.id.clone(),
        head: job.head.clone(),
        scope: job.scope.clone(),
        runner: job.env.runner,
        command: job.commands.join("\n"),
        started_at: now_unix(),
        duration_ms: 0,
        exit_code: None,
        outcome: RunOutcome::Stopped,
        totals: Totals::default(),
        results: Vec::new(),
        log: None,
        tail: String::new(),
    };
    let Some(_place) = super::place(&shared, &pr, act, &stop).await else {
        done(&shared, &pr, run).await;
        return Ok(());
    };
    super::set_phase(
        &shared,
        &pr,
        act,
        TestsPhase::Running {
            run: job.id.clone(),
        },
    );
    super::tell(&shared, &pr).await;
    let log_path = shared.paths.test_log(&pr, &job.id);
    let log = log_path
        .parent()
        .map(std::fs::create_dir_all)
        .transpose()
        .ok()
        .and_then(|_| CappedLog::create(&log_path).ok());
    let mut sink = Sink::new(
        &shared,
        &pr,
        To::Run(job.id.clone()),
        log,
        Some(parser_for(&job.recipe.tests.results)),
    );
    let began = Instant::now();
    let mut ended_by: Option<RunOutcome> = None;
    let mut first_failure: Option<Option<i32>> = None;
    for command in &job.commands {
        let left = limit.saturating_sub(began.elapsed());
        if left.is_zero() {
            ended_by = Some(RunOutcome::TimedOut);
            break;
        }
        match run_command(&shared, &pr, act, &job, command, left, &stop, &mut sink).await {
            Ok(Ended::Exited(Some(0))) => {}
            Ok(Ended::Exited(code)) => {
                first_failure.get_or_insert(code);
            }
            Ok(Ended::TimedOut) => {
                ended_by = Some(RunOutcome::TimedOut);
                break;
            }
            Ok(Ended::Stopped) => {
                ended_by = Some(RunOutcome::Stopped);
                break;
            }
            Err(e) => {
                ended_by = Some(RunOutcome::Error {
                    message: format!("The run could not start: {e}"),
                });
                break;
            }
        }
    }
    sink.flush();
    let (results, tail, logged) = sink.finish(&job.ctx.worktree);
    run.totals = totals_of(&results);
    run.outcome = match ended_by {
        Some(outcome) => outcome,
        None if first_failure.is_some() || run.totals.failed > 0 => RunOutcome::Failed,
        None => RunOutcome::Passed,
    };
    if results.is_empty() && job.recipe.tests.results == ResultsFormat::Plain {
        match run.outcome {
            RunOutcome::Passed => run.totals.passed = 1,
            RunOutcome::Failed => run.totals.failed = 1,
            _ => {}
        }
    }
    // A run that failed only by its results keeps no code: every command exited 0, and a 0
    // next to Failed reads as a pass.
    run.exit_code = match (&run.outcome, first_failure) {
        (RunOutcome::Stopped | RunOutcome::TimedOut | RunOutcome::Error { .. }, _) => None,
        (RunOutcome::Failed, None) => None,
        (_, failure) => failure.unwrap_or(Some(0)),
    };
    run.results = results;
    run.tail = tail;
    run.log = logged.then(|| log_path.display().to_string());
    run.duration_ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
    done(&shared, &pr, run).await;
    Ok(())
}

/// Stores a finished run and says it is done, even when the review ended (then it is not
/// stored).
async fn done(shared: &Shared, pr: &PrRef, run: TestRun) {
    store_run(shared, pr, run.clone()).await;
    shared.publish(
        topics::TESTS,
        Event::TestRunDone {
            pr: pr.clone(),
            run: Box::new(run),
        },
    );
}

/// Stores `run` first among the review's runs, keeping as many as `keep_runs` allows.
pub(super) async fn store_run(shared: &Shared, pr: &PrRef, run: TestRun) -> bool {
    env::update_record(shared, pr, |record| {
        record.runs.insert(0, run);
        keep_runs(&mut record.runs);
    })
    .await
}

fn totals_of(results: &[TestCaseResult]) -> Totals {
    let mut totals = Totals::default();
    for result in results {
        match result.status {
            TestStatus::Passed => totals.passed += 1,
            TestStatus::Failed => totals.failed += 1,
            TestStatus::Skipped => totals.skipped += 1,
        }
    }
    totals
}

/// One command of a run or a list, in a container of the recipe's image or in the local
/// sandbox with no network.
#[allow(clippy::too_many_arguments)]
async fn run_command(
    shared: &Shared,
    pr: &PrRef,
    act: u64,
    job: &RunJob,
    command: &str,
    limit: Duration,
    stop: &StopFlag,
    sink: &mut Sink<'_>,
) -> io::Result<Ended> {
    let exec = shared.test_exec.as_ref();
    let (ctx, env) = (&job.ctx, &job.env);
    let mut chunk = |stream, bytes: &[u8]| sink.chunk(stream, bytes);
    if env.runner == RunnerKind::Container {
        let (Some(engine), Some(image)) = (&env.engine, &job.image) else {
            return Err(io::Error::other("no container engine or image"));
        };
        let docker = env::docker_for(engine, &ctx.host);
        let config = shared.config.read().await.tests.clone();
        let name = names::run_name(&ctx.slug, pr.number, shared.tests.seq());
        let container_job = ContainerJob {
            name: name.clone(),
            image: image.clone(),
            command: command.to_string(),
            env: docker::job_env(&env.signals.ecosystems, &job.recipe.tests.env),
            net: job.network.clone(),
            cpus: Some(config.cpus_for(cores())),
            memory_mb: Some(config.memory_mb),
            labels: ctx.labels(LabelKind::Run),
        };
        super::set_container(shared, pr, act, Some(name));
        let ended = container::run_container(
            exec,
            &docker,
            &container_job,
            &mounts(ctx, env),
            limit,
            stop,
            &mut chunk,
        )
        .await;
        super::set_container(shared, pr, act, None);
        return ended;
    }
    let roots = env::roots(shared, ctx)?;
    let profile = env::profile_file(shared, ctx, NetMode::Offline)?;
    let cmd = sandbox::local_command(
        &shared.sandbox_exec_program,
        &profile,
        command,
        sandbox::base_env(
            &roots,
            &env::local_path(shared),
            &env.signals.ecosystems,
            &job.recipe.tests.env,
        ),
        &ctx.worktree,
    );
    local::run_local(exec, &cmd, limit, stop, &mut chunk).await
}

/// The list command, with a place, answered as the tests it found.
pub(super) async fn list(shared: &Arc<Shared>, pr: &PrRef, id: u64, stop: &StopFlag) -> Outcome {
    let usable = match ready_to_run(shared, pr).await {
        Ok(usable) => usable,
        Err(out) => return out,
    };
    let Some(command) = usable.recipe.tests.list.clone() else {
        return not_alive(NO_LIST.into());
    };
    let job = job_of(usable, "list".into(), TestScope::All, vec![command.clone()]);
    let Some(_place) = super::place(shared, pr, id, stop).await else {
        return not_alive("Listing the tests was stopped".into());
    };
    super::set_phase(shared, pr, id, TestsPhase::Running { run: "list".into() });
    super::tell(shared, pr).await;
    let secs = shared
        .config
        .read()
        .await
        .tests
        .run_timeout_secs
        .min(LIST_MAX_SECS);
    let limit = Duration::from_secs(u64::from(secs));
    let mut sink = Sink::new(shared, pr, To::Quiet, None, None);
    let ended = run_command(shared, pr, id, &job, &command, limit, stop, &mut sink).await;
    sink.flush();
    let failure = match ended {
        Ok(Ended::Exited(Some(0))) => {
            let tests = parse_list(&job.recipe.tests.results, &sink.stdout());
            let listing = Listing {
                head: job.head.clone(),
                tests: tests.clone(),
            };
            env::update_record(shared, pr, |record| record.listing = Some(listing)).await;
            return Outcome::Ok(Reply::TestList(tests));
        }
        Ok(Ended::Exited(Some(code))) => format!(
            "Listing the tests failed (exit code {code}):\n{}",
            sink.tail()
        ),
        Ok(Ended::Exited(None)) => format!(
            "Listing the tests failed (ended by a signal):\n{}",
            sink.tail()
        ),
        Ok(Ended::TimedOut) => format!("Listing the tests ran past its limit of {secs}s"),
        Ok(Ended::Stopped) => "Listing the tests was stopped".to_string(),
        Err(e) => format!("Listing the tests could not start: {e}"),
    };
    Outcome::Err(ProtocolError::new(ErrorCode::InvalidState, failure))
}
