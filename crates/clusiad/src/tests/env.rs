//! What the tests of a review are read from: the machine, the worktree, the recipe file and the
//! stored record; the plan and the view the clients get.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use clusia_core::PrRef;
use clusia_core::tests::{
    Engine, EnvState, NearTest, RECIPE_FILE, RunnerChoice, RunnerKind, SetupPlan, StepState,
    TestsPhase, TestsRecord, TestsView,
};
use clusia_testrun::container;
use clusia_testrun::docker::Docker;
use clusia_testrun::engine::{self, HostEnv};
use clusia_testrun::image::{self, ImagePlan};
use clusia_testrun::names::{self, LabelKind, Labels};
use clusia_testrun::plan::{Facts, effective_runner, setup_plan, work_marker};
use clusia_testrun::recipe::{Recipe, StepInto, recipe_hash};
use clusia_testrun::sandbox::{self, NetMode, SandboxRoots};
use clusia_testrun::{compose, near, scan};

use super::{Live, recipe_file};
use crate::reviews;
use crate::state::Shared;
use crate::sync::now_unix;

/// Where a review's commands run from and what they may see.
#[derive(Clone)]
pub(super) struct Ctx {
    pub pr: PrRef,
    /// The worktree, through `realpath`.
    pub worktree: PathBuf,
    pub clone: PathBuf,
    /// This worktree's own git folder (`<common>/worktrees/<name>`), checked by `git_dirs_of`.
    pub gitdir: PathBuf,
    pub git_common: PathBuf,
    pub slug: String,
    pub host: HostEnv,
}

impl Ctx {
    pub fn new(shared: &Shared, pr: &PrRef) -> Result<Ctx, String> {
        let shown = shared.paths.worktree_for(pr);
        let unreadable = || format!("Cannot read the git folder of {}", shown.display());
        let worktree = std::fs::canonicalize(&shown).map_err(|_| unreadable())?;
        let writable = [worktree.clone(), shared.paths.tests_dir()];
        let GitDirs {
            gitdir,
            common: git_common,
        } = git_dirs_of(&worktree, &writable).ok_or_else(unreadable)?;
        let clone = match git_common.file_name() {
            Some(name) if name == ".git" => git_common
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| git_common.clone()),
            _ => git_common.clone(),
        };
        Ok(Ctx {
            pr: pr.clone(),
            slug: names::repo_slug(&pr.owner, &pr.repo),
            host: host(shared),
            worktree,
            clone,
            gitdir,
            git_common,
        })
    }

    pub fn labels(&self, kind: LabelKind) -> Labels {
        Labels {
            review: self.pr.file_key(),
            repo: self.pr.slug(),
            kind,
        }
    }
}

/// The machine as the daemon sees it: its `PATH`, its home, and the docker of the options.
pub(super) fn host(shared: &Shared) -> HostEnv {
    HostEnv {
        path: std::env::var_os("PATH"),
        home: std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
        docker_override: shared.docker_program.clone(),
        docker_host: std::env::var("DOCKER_HOST").ok().filter(|v| !v.is_empty()),
    }
}

pub(super) fn docker_for(engine: &Engine, host: &HostEnv) -> Docker {
    let cli = PathBuf::from(&engine.cli);
    Docker {
        env: engine::cli_env(&cli, host),
        cli,
    }
}

/// The docker of a running engine, found now; `None` when there is none.
pub(super) async fn docker_now(shared: &Shared) -> Option<Docker> {
    let host = host(shared);
    let engine = engine::detect(shared.test_exec.as_ref(), &host).await?;
    engine.running.then(|| docker_for(&engine, &host))
}

/// A worktree's git folders, each through `realpath`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GitDirs {
    pub gitdir: PathBuf,
    pub common: PathBuf,
}

/// The git folders of `worktree`, read from its `.git` file. That file sits where a run may
/// write, so it is trusted only when the folder it names points back at this worktree, sits in
/// the `worktrees/` of its common folder, and neither lies inside a `writable` root.
pub(super) fn git_dirs_of(worktree: &Path, writable: &[PathBuf]) -> Option<GitDirs> {
    let writable: Vec<PathBuf> = writable
        .iter()
        .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| root.clone()))
        .collect();
    let dot_git = worktree.join(".git");
    if !std::fs::symlink_metadata(&dot_git).ok()?.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let named = text.trim().strip_prefix("gitdir:")?.trim();
    let gitdir = std::fs::canonicalize(worktree.join(named)).ok()?;
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(common) => std::fs::canonicalize(gitdir.join(common.trim())).ok()?,
        Err(_) => gitdir.clone(),
    };
    if gitdir.parent() != Some(common.join("worktrees").as_path()) {
        return None;
    }
    let back = std::fs::read_to_string(gitdir.join("gitdir")).ok()?;
    if std::fs::canonicalize(back.trim()).ok()? != std::fs::canonicalize(&dot_git).ok()? {
        return None;
    }
    let outside = |dir: &Path| writable.iter().all(|root| !dir.starts_with(root));
    (outside(&common) && outside(&gitdir)).then_some(GitDirs { gitdir, common })
}

/// The recipe file as it stands.
pub(super) enum RecipeState {
    Missing,
    Problem(String),
    Valid { recipe: Box<Recipe>, hash: String },
}

pub(super) fn recipe_state(worktree: &Path) -> RecipeState {
    let bytes = match recipe_file::read_recipe(worktree) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return RecipeState::Missing,
        Err(message) => return RecipeState::Problem(message),
    };
    let problems = |reasons: Vec<String>| {
        RecipeState::Problem(format!(
            "The recipe {RECIPE_FILE} has problems: {}",
            reasons.join("; ")
        ))
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return problems(vec!["it is not UTF-8".into()]);
    };
    match Recipe::parse_toml(text) {
        Ok(recipe) => RecipeState::Valid {
            recipe: Box::new(recipe),
            hash: recipe_hash(&bytes),
        },
        Err(reasons) => problems(reasons),
    }
}

/// What docker said about a recipe's image and services, for one recipe hash and runner.
#[derive(Clone)]
pub(crate) struct Cached {
    pub hash: String,
    pub runner: RunnerKind,
    pub running: bool,
    pub image: Option<Result<ImagePlan, String>>,
    pub image_present: bool,
    /// The Compose version check, for a recipe with services.
    pub compose: Option<Result<(), String>>,
}

/// Everything a decision about `pr` reads, read once.
pub(super) struct Snapshot {
    pub record: TestsRecord,
    pub head: String,
    pub ctx: Option<Ctx>,
    pub shipped: Option<String>,
    pub recipe: RecipeState,
    /// The stored environment, with the runner the recipe and the config make effective.
    pub env: Option<EnvState>,
    pub cached: Option<Cached>,
    pub plan: Option<SetupPlan>,
}

pub(super) async fn snapshot(shared: &Shared, pr: &PrRef) -> Snapshot {
    let record = load(shared, pr);
    let head = reviews::load_stored(shared, pr)
        .ok()
        .flatten()
        .map(|review| review.head_sha)
        .or_else(|| record.env.as_ref().map(|env| env.head.clone()))
        .unwrap_or_default();
    let ctx = Ctx::new(shared, pr).ok();
    let shipped = match &ctx {
        Some(ctx) => match shared.tests.with(pr, |entry| entry.shipped.clone()) {
            Some(known) => known,
            None => refresh_shipped(shared, ctx).await,
        },
        // A pull request with no worktree has nothing to read yet.
        None if !shared.paths.worktree_for(pr).exists() => None,
        None => Ctx::new(shared, pr).err(),
    };
    let recipe = match (&shipped, &ctx) {
        (None, Some(ctx)) => recipe_state(&ctx.worktree),
        _ => RecipeState::Missing,
    };
    let config_runner = shared.config.read().await.tests.runner;
    let env = record.env.clone().map(|mut env| {
        let wanted = match &recipe {
            RecipeState::Valid { recipe, .. } => recipe.runner,
            _ => RunnerChoice::Auto,
        };
        env.runner = effective_runner(config_runner, wanted, env.engine.as_ref());
        env
    });
    let (mut cached, mut plan) = (None, None);
    if let (RecipeState::Valid { recipe, hash }, Some(env), Some(ctx)) = (&recipe, &env, &ctx) {
        let running = env.engine.as_ref().is_some_and(|e| e.running);
        let kept = shared.tests.with(pr, |entry| {
            entry
                .facts
                .clone()
                .filter(|c| c.hash == *hash && c.runner == env.runner && c.running == running)
        });
        let facts = match kept {
            Some(facts) => facts,
            None => {
                let facts = compute_cached(shared, ctx, env, recipe, hash).await;
                shared
                    .tests
                    .with(pr, |entry| entry.facts = Some(facts.clone()));
                facts
            }
        };
        plan = Some(plan_of(shared, ctx, &record, env, recipe, hash, &facts));
        cached = Some(facts);
    }
    Snapshot {
        record,
        head,
        ctx,
        shipped,
        recipe,
        env,
        cached,
        plan,
    }
}

async fn compute_cached(
    shared: &Shared,
    ctx: &Ctx,
    env: &EnvState,
    recipe: &Recipe,
    hash: &str,
) -> Cached {
    let running = env.engine.as_ref().is_some_and(|e| e.running);
    let container = env.runner == RunnerKind::Container;
    let image = container.then(|| {
        image::plan_image(
            recipe,
            &ctx.worktree,
            &ctx.slug,
            &ctx.labels(LabelKind::Image),
            &env.inputs_hash,
        )
    });
    let mut image_present = false;
    if let (Some(Ok(plan)), Some(engine)) = (&image, env.engine.as_ref().filter(|e| e.running)) {
        let docker = docker_for(engine, &ctx.host);
        image_present =
            container::image_present(shared.test_exec.as_ref(), &docker, &plan.tag).await;
    }
    // Only the version Clúsia already knows: `docker compose config` reads the PR's compose
    // file, which belongs to the approved `services` step.
    let compose = (container && recipe.services.is_some()).then(|| {
        let version = env.engine.as_ref().and_then(|e| e.compose.clone());
        match &version {
            Some(v) if compose::compose_version_ok(v) => Ok(()),
            _ => Err(format!(
                "Docker Compose 2.24.4 or newer is needed; found {}",
                version.as_deref().unwrap_or("none")
            )),
        }
    });
    Cached {
        hash: hash.to_string(),
        runner: env.runner,
        running,
        image,
        image_present,
        compose,
    }
}

fn plan_of(
    shared: &Shared,
    ctx: &Ctx,
    record: &TestsRecord,
    env: &EnvState,
    recipe: &Recipe,
    hash: &str,
    cached: &Cached,
) -> SetupPlan {
    let tag = cached
        .image
        .as_ref()
        .and_then(|image| image.as_ref().ok())
        .map(|image| image.tag.clone());
    let markers = recipe
        .setup
        .iter()
        .enumerate()
        .filter(|(_, step)| step.into == StepInto::Work)
        .map(|(at, step)| {
            let id = format!("work:{at}");
            let text = work_marker(step, tag.as_deref(), &env.inputs_hash);
            let done = container::marker_matches(&ctx.worktree, &id, &text);
            (id, done)
        })
        .collect();
    let (failures, services_ready, refused) = shared.tests.with(&ctx.pr, |entry| {
        (
            entry.step_failures.clone(),
            entry.services_ready.as_deref() == Some(hash),
            entry
                .compose_refused
                .clone()
                .filter(|(h, _)| h == hash)
                .map(|(_, m)| m),
        )
    });
    let mount = tag
        .as_deref()
        .and_then(|tag| shared.tests.mount(tag, &ctx.worktree));
    let facts = Facts {
        engine_running: env.engine.as_ref().is_some_and(|e| e.running),
        image_present: cached.image_present,
        markers,
        mount_ok: mount.as_ref().map(Result::is_ok),
        services_ready,
        stack_up: record.stack.is_some(),
        clusia_problem: None,
        compose_problem: cached
            .compose
            .as_ref()
            .and_then(|c| c.as_ref().err().cloned())
            .or(refused),
    };
    let mut plan = setup_plan(recipe, hash, env, &facts, record.approval.as_ref());
    for step in &mut plan.steps {
        if step.state == StepState::Missing
            && let Some(message) = failures.get(&step.id)
        {
            step.state = StepState::Failed {
                message: message.clone(),
            };
        }
    }
    if plan.blocked.is_none() {
        plan.blocked = match (&cached.image, mount) {
            (Some(Err(message)), _) => Some(message.clone()),
            (_, Some(Err(message))) => Some(message),
            _ => None,
        };
    }
    if plan.blocked.is_some() {
        plan.ready = false;
    }
    plan
}

/// What the clients see of the tests of `pr`.
pub(super) async fn view(shared: &Shared, pr: &PrRef) -> TestsView {
    let snap = snapshot(shared, pr).await;
    let (live, failed) = shared.tests.with(pr, |entry| {
        (
            entry.activity.as_ref().map(|a| a.phase.clone()),
            entry.failed.clone(),
        )
    });
    let phase = if let Some(phase) = live {
        phase
    } else if let Some(message) = failed.or_else(|| snap.shipped.clone()) {
        TestsPhase::Failed { message }
    } else if snap.env.is_none() {
        TestsPhase::NotGenerated
    } else {
        match &snap.recipe {
            RecipeState::Problem(message) => TestsPhase::Failed {
                message: message.clone(),
            },
            RecipeState::Missing => TestsPhase::NotGenerated,
            RecipeState::Valid { .. } if snap.plan.as_ref().is_some_and(|p| p.ready) => {
                TestsPhase::Ready
            }
            RecipeState::Valid { .. } => TestsPhase::NeedsSetup,
        }
    };
    let recipe = match &snap.recipe {
        RecipeState::Valid { recipe, hash } => Some(recipe.summary(
            hash,
            snap.record.written_hash.as_deref() != Some(hash.as_str()),
        )),
        _ => None,
    };
    let github_checks = shared
        .review_caches
        .get(pr)
        .await
        .map(|cache| cache.check_runs.clone())
        .unwrap_or_default();
    let refresh_available = match (&snap.record.generated_inputs, &snap.ctx) {
        (Some(generated), Some(ctx)) => {
            *generated != scan::inputs_hash(&ctx.worktree, &scan::repo_signals(&ctx.worktree))
        }
        _ => false,
    };
    let near = near_list(shared, pr, &snap.record).await;
    TestsView {
        head: snap.head,
        phase,
        env: snap.env,
        recipe,
        proposal: snap.record.proposal,
        plan: snap.plan,
        runs: snap.record.runs,
        near,
        github_checks,
        refresh_available,
    }
}

/// The tests near the change: the stored listing, the files the pull request touches, its
/// changed functions (when the call graph of the head is built), and the agent's names.
pub(super) async fn near_list(shared: &Shared, pr: &PrRef, record: &TestsRecord) -> Vec<NearTest> {
    let known = record
        .listing
        .as_ref()
        .map(|l| l.tests.clone())
        .unwrap_or_default();
    let (touched, changed) = match shared.review_caches.get(pr).await {
        Some(cache) => {
            let touched: Vec<String> = cache.files.iter().map(|f| f.path.clone()).collect();
            let changed = shared
                .indexes
                .ready(pr, &cache.pr.head_sha)
                .map(|built| {
                    built
                        .changed
                        .iter()
                        .filter(|c| c.kind != clusia_calls::ChangeKind::Removed)
                        .filter_map(|c| built.locate(&c.id.0))
                        .map(|(f, _)| near::ChangedUnit {
                            path: f.path.clone(),
                            qualified: f.qualified.clone(),
                            lang: f.lang.clone(),
                        })
                        .collect::<Vec<near::ChangedUnit>>()
                })
                .unwrap_or_default();
            (touched, changed)
        }
        None => (Vec::new(), Vec::new()),
    };
    near::near_tests(&known, &touched, &changed, &record.agent_near)
}

/// What the machine and the worktree offer now.
pub(super) async fn probe(shared: &Shared, ctx: &Ctx, head: &str) -> EnvState {
    let exec = shared.test_exec.as_ref();
    let engine = engine::detect(exec, &ctx.host).await;
    let signals = scan::repo_signals(&ctx.worktree);
    let inputs_hash = scan::inputs_hash(&ctx.worktree, &signals);
    let existing = scan::existing(
        exec,
        engine.as_ref(),
        &ctx.host,
        &ctx.worktree,
        &ctx.clone,
        &ctx.pr.slug(),
    )
    .await;
    let health = sandbox::health(exec, &shared.sandbox_exec_program).await;
    let config = shared.config.read().await.tests.runner;
    EnvState {
        head: head.to_string(),
        probed_at: now_unix(),
        runner: effective_runner(config, RunnerChoice::Auto, engine.as_ref()),
        note: note_for(engine.as_ref(), &health),
        sandbox_ok: health.is_ok(),
        engine,
        signals,
        existing,
        inputs_hash,
    }
}

/// What the reviewer should know about the engine, or about the sandbox when there is none.
pub(super) fn note_for(engine: Option<&Engine>, health: &Result<(), String>) -> Option<String> {
    match engine {
        None => health.as_ref().err().cloned(),
        Some(engine) if engine.running => None,
        Some(engine) => Some(match &engine.start {
            Some(start) => format!(
                "{} is stopped. Start it with: {start}",
                container::engine_name(engine.kind)
            ),
            None => "Start your container engine.".to_string(),
        }),
    }
}

/// Asks for the engine again before a setup or a run: one that stopped or started since the
/// probe changes the plan.
pub(super) async fn reprobe(shared: &Shared, ctx: &Ctx, pr: &PrRef) {
    let Some(stored) = load(shared, pr).env else {
        return;
    };
    let engine = engine::detect(shared.test_exec.as_ref(), &ctx.host).await;
    if engine == stored.engine {
        return;
    }
    let health = if engine.is_none() {
        sandbox::health(shared.test_exec.as_ref(), &shared.sandbox_exec_program).await
    } else {
        Ok(())
    };
    let note = note_for(engine.as_ref(), &health);
    rewrite_record(shared, pr, |record| {
        if let Some(env) = record.env.as_mut() {
            env.engine = engine;
            env.note = note;
            env.probed_at = now_unix();
            // The sandbox was asked again only for want of an engine.
            if env.engine.is_none() {
                env.sandbox_ok = health.is_ok();
            }
        }
    })
    .await;
    shared.tests.with(pr, |entry| entry.facts = None);
}

/// Asks again whether the pull request ships its own `.clusia/`, and keeps the answer.
pub(super) async fn refresh_shipped(shared: &Shared, ctx: &Ctx) -> Option<String> {
    let problem =
        scan::clusia_dir_problem(shared.test_exec.as_ref(), &ctx.worktree, Some(&ctx.gitdir)).await;
    shared
        .tests
        .with(&ctx.pr, |entry| entry.shipped = Some(problem.clone()));
    problem
}

/// The containers and compose projects that belong to something live, which the orphan sweep
/// leaves alone.
pub(super) fn live_names(shared: &Shared) -> Vec<String> {
    let mut names = shared.tests.containers();
    for (pr, _) in super::recorded(shared) {
        if let Some(stack) = load(shared, &pr).stack {
            names.push(stack);
        }
    }
    names
}

/// Takes down the compose stack the record of `pr` names. A failure keeps the name, for the
/// sweep to find.
pub(super) async fn take_stack_down(shared: &Shared, pr: &PrRef) {
    let Some(project) = load(shared, pr).stack else {
        return;
    };
    let Some(docker) = docker_now(shared).await else {
        tracing::warn!(pr = %pr, "no running engine to take the compose stack down");
        return;
    };
    match container::stack_down(shared.test_exec.as_ref(), &docker, &project).await {
        Ok(()) => {
            rewrite_record(shared, pr, |record| record.stack = None).await;
        }
        Err(e) => tracing::warn!(error = %e, pr = %pr, "cannot take the compose stack down"),
    }
}

/// Takes `project` down in the background; shutdown waits for it.
pub(super) fn take_down_later(shared: &Shared, project: String) {
    let live = Live::new(&shared.tests);
    let exec = shared.test_exec.clone();
    let host = host(shared);
    tokio::spawn(async move {
        let _live = live;
        let Some(engine) = engine::detect(exec.as_ref(), &host).await else {
            return;
        };
        if !engine.running {
            return;
        }
        let docker = docker_for(&engine, &host);
        if let Err(e) = container::stack_down(exec.as_ref(), &docker, &project).await {
            tracing::warn!(error = %e, project = %project, "cannot take the compose stack down");
        }
    });
}

/// Changes the stored record of `pr` under the review lock, when there is one.
pub(super) async fn rewrite_record(
    shared: &Shared,
    pr: &PrRef,
    change: impl FnOnce(&mut TestsRecord),
) -> bool {
    let _guard = reviews::lock(shared, pr).await;
    if !shared.paths.tests_file(pr).exists() {
        return false;
    }
    let mut record = load(shared, pr);
    change(&mut record);
    save(shared, pr, &record);
    true
}

/// Changes the stored record of `pr` under the review lock, while the review lives.
pub(super) async fn update_record(
    shared: &Shared,
    pr: &PrRef,
    change: impl FnOnce(&mut TestsRecord),
) -> bool {
    let _guard = reviews::lock(shared, pr).await;
    if !alive(shared, pr) {
        return false;
    }
    let mut record = load(shared, pr);
    change(&mut record);
    save(shared, pr, &record);
    true
}

/// Whether the review of `pr` exists and has not ended.
pub(super) fn alive(shared: &Shared, pr: &PrRef) -> bool {
    matches!(reviews::load_stored(shared, pr), Ok(Some(review)) if !review.state.is_terminal())
}

pub(super) fn load(shared: &Shared, pr: &PrRef) -> TestsRecord {
    clusia_store::load_tests(&shared.paths, pr).unwrap_or_else(|e| {
        tracing::warn!(error = %e, pr = %pr, "cannot read the test record");
        TestsRecord::default()
    })
}

pub(super) fn save(shared: &Shared, pr: &PrRef, record: &TestsRecord) {
    if let Err(e) = clusia_store::save_tests(&shared.paths, pr, record) {
        tracing::warn!(error = %e, pr = %pr, "cannot save the test record");
    }
}

/// The name of the review's temp folder, which also names its files in `tests/`.
fn tmp_name(shared: &Shared, pr: &PrRef) -> OsString {
    shared
        .paths
        .test_tmp_for(pr)
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| pr.file_key().into())
}

fn named(shared: &Shared, pr: &PrRef, suffix: &str) -> PathBuf {
    let mut name = tmp_name(shared, pr);
    name.push(suffix);
    shared.paths.tests_dir().join("profiles").join(name)
}

/// The compose override of `pr`, outside every folder a run may write.
pub(super) fn override_file(shared: &Shared, pr: &PrRef) -> PathBuf {
    named(shared, pr, "-compose.yml")
}

/// The image build context of `pr`, outside every folder a run may write.
pub(super) fn build_dir(shared: &Shared, pr: &PrRef) -> PathBuf {
    shared
        .paths
        .tests_dir()
        .join("build")
        .join(tmp_name(shared, pr))
}

/// Writes the sandbox profile of `mode` for `ctx` atomically, right before its use, and answers
/// its path.
pub(super) fn profile_file(shared: &Shared, ctx: &Ctx, mode: NetMode) -> io::Result<PathBuf> {
    let roots = roots(shared, ctx)?;
    let file = named(
        shared,
        &ctx.pr,
        match mode {
            NetMode::Offline => "-offline.sb",
            NetMode::Online => "-online.sb",
        },
    );
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    debug_assert!(!sandbox::writable_by_sandbox(&roots, &file));
    let mut tmp = file.clone().into_os_string();
    tmp.push(".tmp");
    std::fs::write(&tmp, sandbox::profile(&roots, mode))?;
    std::fs::rename(&tmp, &file)?;
    Ok(file)
}

/// The folders a local run of `ctx` may write and read, at their real paths.
pub(super) fn roots(shared: &Shared, ctx: &Ctx) -> io::Result<SandboxRoots> {
    sandbox::resolve_roots(SandboxRoots {
        worktree: ctx.worktree.clone(),
        git_common: ctx.git_common.clone(),
        review_tmp: shared.paths.test_tmp_for(&ctx.pr),
        repo_cache: shared.paths.test_cache_for(&ctx.pr.owner, &ctx.pr.repo),
        data_root: shared.paths.root().to_path_buf(),
        home: ctx.host.home.clone(),
        clone_env: sandbox::clone_env_files(&ctx.clone),
    })
}

/// The `PATH` of a local run: the daemon's own, then the folders agent tools live in, then
/// `~/.cargo/bin`.
pub(super) fn local_path(shared: &Shared) -> OsString {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(shared.harness_search_paths.iter().cloned());
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".cargo/bin"));
    }
    let mut seen = Vec::new();
    dirs.retain(|dir| {
        let new = !seen.contains(dir);
        seen.push(dir.clone());
        new
    });
    std::env::join_paths(dirs).unwrap_or_default()
}
