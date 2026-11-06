//! Main's worktree: a second checkout at the commit the pull request branched from, with its
//! own temp, caches, volumes and setup approval. It runs in the pull request's image, so only
//! the code differs between the two sides.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use clusia_core::PrRef;
use clusia_core::cockpit::{
    CockpitActivity, MainPhase, MainRecord, MainView, SERVICES_ON_MAIN, main_recipe_hash,
};
use clusia_core::tests::{
    Approval, Approve, Ecosystem, EnvState, RECIPE_CHANGED, RunnerKind, STEP_TESTS, SetupPlan,
    StepState, TestsPhase,
};
use clusia_git::{ensure_worktree, merge_base, remove_worktree, repo_of_worktree};
use clusia_protocol::{Outcome, Reply};
use clusia_testrun::docker::{self, RunMounts};
use clusia_testrun::exec::StopFlag;
use clusia_testrun::names;
use clusia_testrun::plan::{Facts, main_plan, work_marker};
use clusia_testrun::recipe::StepInto;
use clusia_testrun::{container, scan};

use super::env::{self, Ctx, RecipeState, Snapshot};
use super::work::{self, StepEnd, Walk};
use super::{Doing, GENERATE_FIRST, Live, NO_RECIPE, STOPPING, scenarios};
use crate::reviews;
use crate::state::Shared;
use crate::sync::now_unix;

pub(super) const OPEN_MAIN_FIRST: &str = "Open main first: Run on main too creates its worktree";
pub(super) const MAIN_OTHER_RUNNER: &str = "Main was prepared for another runner; open it again";
const STEP_LEFT_NOTHING: &str = "The step finished, but what it leaves behind is not there";
const VOLUME_RM_LIMIT: Duration = Duration::from_secs(30);
const ECOSYSTEMS: [Ecosystem; 6] = [
    Ecosystem::Cargo,
    Ecosystem::Node,
    Ecosystem::Python,
    Ecosystem::Go,
    Ecosystem::Jvm,
    Ecosystem::Ruby,
];

/// What a run in main's container mounts: main's worktree and its own cache volumes, never the
/// pull request's.
pub(super) fn mounts_for_main(ctx: &Ctx, env: &EnvState) -> RunMounts {
    RunMounts {
        worktree: ctx.worktree.clone(),
        git_common: ctx.git_common.clone(),
        scenario: None,
        caches: docker::main_cache_mounts(&ctx.slug, ctx.pr.number, &env.signals.ecosystems),
    }
}

/// Brings main's worktree to the commit the pull request's diff is against, and stores what was
/// probed there. It needs no network (the review's open fetched the base) and shows no
/// activity.
pub(super) async fn ensure_main_at_base(
    shared: &Arc<Shared>,
    pr: &PrRef,
) -> Result<MainRecord, String> {
    let review = super::active(shared, pr).map_err(|_| "The review ended".to_string())?;
    let repo = repo_of_worktree(&shared.paths.worktree_for(pr))
        .await
        .map_err(|e| format!("Cannot find the repository of this review: {e}"))?;
    // The merge base is the commit the diff is against; the base's tip also holds what was
    // merged after the branch point.
    let base = merge_base(&repo, &review.base_sha, &review.head_sha)
        .await
        .map_err(|e| format!("Cannot find the commit this pull request branched from: {e}"))?;
    let main = shared.paths.main_worktree_for(pr);
    let created = {
        let _serialized = shared.worktree_lock.lock().await;
        let created = !main.join(".git").exists();
        ensure_worktree(&repo, &main, &base)
            .await
            .map_err(|e| format!("Cannot create main's worktree: {e}"))?;
        created
    };
    let ctx = Ctx::for_main(shared, pr)?;
    if let Some(problem) =
        scan::clusia_dir_problem(shared.test_exec.as_ref(), &ctx.worktree, Some(&ctx.gitdir)).await
    {
        return Err(problem.replacen("This pull request", "Main", 1));
    }
    let signals = scan::repo_signals(&ctx.worktree);
    let inputs_hash = scan::inputs_hash(&ctx.worktree, &signals);
    let runner = env::snapshot(shared, pr)
        .await
        .env
        .ok_or_else(|| GENERATE_FIRST.to_string())?
        .runner;
    let mut stored = None;
    let alive = env::update_record(shared, pr, |record| {
        let old = record.main.take();
        let main = MainRecord {
            base,
            created_at: old.as_ref().map_or_else(now_unix, |o| o.created_at),
            runner,
            inputs_hash,
            approval: old.as_ref().and_then(|o| o.approval.clone()),
            // A worktree made again has none of the caches and volumes of the old one.
            built: !created && old.as_ref().is_some_and(|o| o.built),
        };
        record.main = Some(main.clone());
        stored = Some(main);
    })
    .await;
    match stored {
        Some(main) if alive => Ok(main),
        _ => Err("The review ended".to_string()),
    }
}

/// Main as the view and a job read it.
#[allow(dead_code)]
pub(super) struct MainState {
    pub view: MainView,
    /// The stored record, ignored when main's worktree is gone.
    pub record: Option<MainRecord>,
    pub ctx: Option<Ctx>,
    /// The review's environment with main's `inputs_hash`.
    pub env: Option<EnvState>,
    /// The tag main runs in: the pull request's image.
    pub image: Option<String>,
    pub ready: bool,
}

pub(super) async fn main_state(shared: &Shared, pr: &PrRef) -> MainState {
    let snap = env::snapshot(shared, pr).await;
    main_state_of(shared, pr, &snap).await
}

/// The commit in `<gitdir>/HEAD`, read as a file.
fn head_of(ctx: &Ctx) -> Option<String> {
    let text = std::fs::read_to_string(ctx.gitdir.join("HEAD")).ok()?;
    let sha = text.trim();
    (sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())).then(|| sha.to_string())
}

pub(super) async fn main_state_of(shared: &Shared, pr: &PrRef, snap: &Snapshot) -> MainState {
    let (opening, failed, step_failures, preparing) = shared.tests.with(pr, |entry| {
        (
            entry.main_opening,
            entry.main_failed.clone(),
            entry.main_step_failures.clone(),
            entry
                .activity
                .as_ref()
                .filter(|a| a.doing == Doing::Main)
                .map(|a| match &a.cockpit {
                    Some(CockpitActivity::PreparingMain { step }) => step.clone(),
                    _ => None,
                }),
        )
    });
    let record = snap
        .record
        .main
        .clone()
        .filter(|_| shared.paths.main_worktree_for(pr).is_dir());
    let image = snap
        .cached
        .as_ref()
        .and_then(|cached| cached.image.as_ref())
        .and_then(|image| image.as_ref().ok())
        .map(|image| image.tag.clone());
    let ctx = record.as_ref().and_then(|_| Ctx::for_main(shared, pr).ok());
    let env = match (&snap.env, &record) {
        (Some(env), Some(record)) => {
            let mut env = env.clone();
            env.inputs_hash = record.inputs_hash.clone();
            env.runner = record.runner;
            env.head = record.base.clone();
            Some(env)
        }
        _ => None,
    };
    let mut plan: Option<SetupPlan> = None;
    if let (RecipeState::Valid { recipe, hash }, Some(env), Some(ctx), Some(record), Some(cached)) =
        (&snap.recipe, &env, &ctx, &record, &snap.cached)
    {
        let tag = image
            .clone()
            .filter(|_| env.runner == RunnerKind::Container);
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
        let mount = tag
            .as_deref()
            .and_then(|tag| shared.tests.mount(tag, &ctx.worktree));
        // The folder may have been moved onto a base that ships a `.clusia/` after the record
        // was made; its tracked markers must not stand for Clúsia's own.
        let clusia_problem =
            scan::clusia_dir_problem(shared.test_exec.as_ref(), &ctx.worktree, Some(&ctx.gitdir))
                .await
                .map(|problem| problem.replacen("This pull request", "Main", 1));
        let facts = Facts {
            engine_running: snap
                .env
                .as_ref()
                .and_then(|e| e.engine.as_ref())
                .is_some_and(|e| e.running),
            image_present: cached.image_present,
            markers,
            mount_ok: mount.as_ref().map(Result::is_ok),
            services_ready: false,
            stack_up: false,
            clusia_problem,
            compose_problem: None,
        };
        let mut built = main_plan(
            recipe,
            &main_recipe_hash(hash),
            env,
            &facts,
            record.approval.as_ref(),
        );
        for step in &mut built.steps {
            if step.state == StepState::Missing
                && let Some(message) = step_failures.get(&step.id)
            {
                step.state = StepState::Failed {
                    message: message.clone(),
                };
            }
        }
        if built.blocked.is_none()
            && let Some(Err(message)) = mount
        {
            built.blocked = Some(message);
        }
        if built.blocked.is_some() {
            built.ready = false;
        }
        plan = Some(built);
    }

    let unavailable = if let Some(problem) = &snap.shipped {
        Some(problem.clone())
    } else {
        match &snap.recipe {
            RecipeState::Missing => Some(NO_RECIPE.to_string()),
            RecipeState::Problem(message) => Some(message.clone()),
            RecipeState::Valid { recipe, .. } if recipe.services.is_some() => {
                Some(SERVICES_ON_MAIN.to_string())
            }
            RecipeState::Valid { .. } => record
                .as_ref()
                .zip(snap.env.as_ref())
                .filter(|(record, env)| record.runner != env.runner)
                .map(|_| MAIN_OTHER_RUNNER.to_string()),
        }
    };
    let step_failed = plan.as_ref().and_then(|plan| {
        plan.steps.iter().find_map(|step| match &step.state {
            StepState::Failed { message } => Some(message.clone()),
            _ => None,
        })
    });
    let phase = if opening {
        MainPhase::Creating
    } else if let Some(step) = preparing {
        MainPhase::Preparing {
            step: step.unwrap_or_default(),
        }
    } else if let Some(reason) = unavailable {
        MainPhase::Unavailable { reason }
    } else if record.is_none() {
        match failed {
            Some(message) => MainPhase::Failed { message },
            None => MainPhase::NotCreated,
        }
    } else if let Some(message) = failed.or(step_failed) {
        MainPhase::Failed { message }
    } else if plan.as_ref().is_some_and(|plan| plan.ready) {
        MainPhase::Ready
    } else {
        MainPhase::NeedsSetup
    };
    let view = MainView {
        base: record.as_ref().map(|record| record.base.clone()),
        at: ctx.as_ref().and_then(head_of),
        plan,
        image: image.clone(),
        first_build: record.as_ref().is_some_and(|record| !record.built),
        phase,
    };
    MainState {
        ready: view.phase == MainPhase::Ready,
        view,
        record,
        ctx,
        env,
        image,
    }
}

/// Ends main's activity `id`: clears the opening flag and keeps `failed` for the view.
fn main_end(shared: &Shared, pr: &PrRef, id: u64, failed: Option<String>) {
    shared.tests.with(pr, |entry| {
        if entry.activity.as_ref().is_some_and(|a| a.id == id) {
            entry.activity = None;
            entry.main_opening = false;
            entry.main_failed = failed;
        }
    });
}

/// Runs `job` in a task of its own, so a panic still ends main's activity `id`, then tells.
fn spawn_main(
    shared: &Arc<Shared>,
    pr: &PrRef,
    id: u64,
    live: Live,
    job: impl Future<Output = Option<String>> + Send + 'static,
) {
    let (shared, pr) = (shared.clone(), pr.clone());
    tokio::spawn(async move {
        let failed = match tokio::spawn(job).await {
            Ok(failed) => failed,
            Err(_) => {
                tracing::error!(pr = %pr, "preparing main failed inside the daemon");
                Some("Preparing main stopped unexpectedly".to_string())
            }
        };
        main_end(&shared, &pr, id, failed);
        drop(live);
        scenarios::tell_cockpit(&shared, &pr).await;
    });
}

fn set_step(shared: &Shared, pr: &PrRef, id: u64, step: Option<String>) {
    shared.tests.with(pr, |entry| {
        if let Some(activity) = entry.activity.as_mut().filter(|a| a.id == id) {
            activity.cockpit = Some(CockpitActivity::PreparingMain { step });
        }
    });
}

/// `OpenMain`: creates or moves main's worktree in the background. It spends no network and
/// runs no program of the recipe.
pub(crate) async fn open_main(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    if let Err(out) = super::active(shared, pr) {
        return out;
    }
    if shared.tests.closing.load(Ordering::SeqCst) {
        return reviews::invalid_state(STOPPING);
    }
    let snap = env::snapshot(shared, pr).await;
    if let Some(problem) = snap.shipped {
        return reviews::invalid_state(problem);
    }
    match &snap.recipe {
        RecipeState::Missing => return reviews::invalid_state(NO_RECIPE),
        RecipeState::Problem(message) => return reviews::invalid_state(message.clone()),
        RecipeState::Valid { recipe, .. } if recipe.services.is_some() => {
            return reviews::invalid_state(SERVICES_ON_MAIN);
        }
        RecipeState::Valid { .. } => {}
    }
    if snap.env.is_none() {
        return reviews::invalid_state(GENERATE_FIRST);
    }
    let started = match super::begin(shared, pr, Doing::Main, TestsPhase::Waiting) {
        Ok(started) => started,
        Err(refused) => return refused.outcome(pr),
    };
    let id = started.id;
    shared.tests.with(pr, |entry| entry.main_opening = true);
    set_step(shared, pr, id, None);
    // `Creating` goes out before the `Ack`, so a client always sees it begin.
    scenarios::tell_cockpit(shared, pr).await;
    let job = {
        let (shared, pr) = (shared.clone(), pr.clone());
        async move { ensure_main_at_base(&shared, &pr).await.err() }
    };
    spawn_main(shared, pr, id, started.live, job);
    Outcome::Ok(Reply::Ack)
}

/// `PrepareMain`: records the approval of main's own plan, then runs the approved steps that
/// are missing in main's worktree, in the background.
pub(crate) async fn prepare_main(
    shared: &Arc<Shared>,
    pr: &PrRef,
    main_hash: &str,
    approve: Approve,
) -> Outcome {
    if let Err(out) = super::active(shared, pr) {
        return out;
    }
    if shared.tests.closing.load(Ordering::SeqCst) {
        return reviews::invalid_state(STOPPING);
    }
    let state = main_state(shared, pr).await;
    if state.record.is_none() {
        return reviews::invalid_state(OPEN_MAIN_FIRST);
    }
    if let MainPhase::Unavailable { reason } = &state.view.phase {
        return reviews::invalid_state(reason.clone());
    }
    let Some(plan) = state.view.plan.clone() else {
        return reviews::invalid_state(GENERATE_FIRST);
    };
    if let Some(blocked) = &plan.blocked {
        return reviews::invalid_state(blocked.clone());
    }
    if main_hash != plan.recipe_hash {
        return reviews::invalid_state(RECIPE_CHANGED);
    }
    let wanted: BTreeSet<String> = match &approve {
        Approve::All => plan
            .steps
            .iter()
            .map(|step| step.id.clone())
            .chain([STEP_TESTS.to_string()])
            .collect(),
        Approve::Steps(ids) => {
            if let Some(unknown) = ids
                .iter()
                .find(|id| *id != STEP_TESTS && !plan.steps.iter().any(|s| s.id == **id))
            {
                return reviews::invalid_state(format!(
                    "There is no step {unknown} in the setup plan"
                ));
            }
            ids.iter().cloned().collect()
        }
    };
    let started = match super::begin(shared, pr, Doing::Main, TestsPhase::Waiting) {
        Ok(started) => started,
        Err(refused) => return refused.outcome(pr),
    };
    let id = started.id;
    let hash = plan.recipe_hash.clone();
    let stored = env::update_record(shared, pr, |record| {
        if let Some(main) = record.main.as_mut() {
            let steps = match main.approval.take() {
                Some(old) if old.recipe_hash == hash => {
                    old.steps.into_iter().chain(wanted).collect()
                }
                _ => wanted,
            };
            main.approval = Some(Approval {
                recipe_hash: hash,
                steps,
                at: now_unix(),
            });
        }
    })
    .await;
    if !stored {
        super::end(shared, pr, id, None);
        drop(started.live);
        return reviews::invalid_state("The review ended");
    }
    set_step(shared, pr, id, None);
    scenarios::tell_cockpit(shared, pr).await;
    let job = {
        let (shared, pr, stop) = (shared.clone(), pr.clone(), started.stop.clone());
        async move { walk(shared, pr, id, stop).await }
    };
    spawn_main(shared, pr, id, started.live, job);
    Outcome::Ok(Reply::Ack)
}

/// Walks main's plan with a place: the mount check as soon as the image exists, then each step
/// that is missing or failed, as far as it is approved. Nothing runs before the approval, and
/// the answer is why the walk failed, if it did.
async fn walk(shared: Arc<Shared>, pr: PrRef, id: u64, stop: Arc<StopFlag>) -> Option<String> {
    let _place = super::place(&shared, &pr, id, &stop).await?;
    let mut tried: BTreeSet<String> = BTreeSet::new();
    while !stop.fired() {
        let snap = env::snapshot(&shared, &pr).await;
        let state = main_state_of(&shared, &pr, &snap).await;
        let image_present = snap.cached.as_ref().is_some_and(|c| c.image_present);
        let (Some(plan), Some(ctx), Some(env), RecipeState::Valid { recipe, .. }) =
            (state.view.plan, state.ctx, state.env, snap.recipe)
        else {
            return Some(
                "The recipe or the environment changed during the setup; open main again".into(),
            );
        };
        if plan.blocked.is_some() {
            return None;
        }
        // Only a step that is approved and waiting makes the engine touch main's worktree, so
        // approving nothing runs nothing, not even the mount check.
        let step = plan
            .steps
            .iter()
            .find(|s| matches!(s.state, StepState::Missing | StepState::Failed { .. }))
            .filter(|s| s.approved)
            .cloned()?;
        if env.runner == RunnerKind::Container
            && image_present
            && let Some(tag) = &state.image
            && shared.tests.mount(tag, &ctx.worktree).is_none()
        {
            work::mount(&shared, &pr, id, &ctx, &env, tag).await;
            continue;
        }
        if !tried.insert(step.id.clone()) {
            let message = STEP_LEFT_NOTHING.to_string();
            keep_failure(&shared, &pr, &step.id, &message);
            return Some(message);
        }
        set_step(&shared, &pr, id, Some(step.id.clone()));
        scenarios::tell_cockpit(&shared, &pr).await;
        let walk = Walk {
            shared: &shared,
            pr: &pr,
            id,
            ctx: &ctx,
            env: &env,
            recipe: &recipe,
            hash: &plan.recipe_hash,
            stop: &stop,
            main: true,
            image_tag: state.image.as_deref(),
        };
        match work::run_step(&walk, &step).await {
            StepEnd::Done => {
                shared.tests.with(&pr, |entry| {
                    entry.main_step_failures.remove(&step.id);
                });
            }
            StepEnd::Failed(message, _) => {
                keep_failure(&shared, &pr, &step.id, &message);
                return Some(message);
            }
            StepEnd::Stopped => return None,
        }
    }
    None
}

fn keep_failure(shared: &Shared, pr: &PrRef, step: &str, message: &str) {
    shared.tests.with(pr, |entry| {
        entry
            .main_step_failures
            .insert(step.to_string(), message.to_string());
    });
}

/// Removes main's worktree, temp, local cache and container volumes. The caller holds
/// `worktree_lock` and, through the review, the review lock, so this takes neither and leaves
/// the stored record to `forget`. The repository is found through the pull request's worktree,
/// which must still be there, or else through main's own.
pub(crate) async fn remove_main(shared: &Shared, pr: &PrRef) {
    let main = shared.paths.main_worktree_for(pr);
    let had_worktree = std::fs::symlink_metadata(&main).is_ok();
    let had_record = env::load(shared, pr).main.is_some();
    if had_worktree {
        let repo = match repo_of_worktree(&shared.paths.worktree_for(pr)).await {
            Ok(repo) => Some(repo),
            Err(_) => repo_of_worktree(&main).await.ok(),
        };
        if let Some(repo) = repo
            && let Err(e) = remove_worktree(&repo, &main).await
        {
            tracing::warn!(error = %e, "cannot remove main's worktree");
        }
        if std::fs::symlink_metadata(&main).is_ok()
            && let Err(e) = std::fs::remove_dir_all(&main)
        {
            tracing::warn!(error = %e, "cannot remove main's worktree folder");
        }
    }
    for dir in [
        shared.paths.test_main_tmp_for(pr),
        shared.paths.test_main_cache_for(pr),
    ] {
        if std::fs::symlink_metadata(&dir).is_ok()
            && let Err(e) = std::fs::remove_dir_all(&dir)
        {
            tracing::warn!(error = %e, dir = %dir.display(), "cannot remove main's folder");
        }
    }
    // Nothing of main's was ever made: there are no volumes to ask the engine about.
    if !(had_worktree || had_record) {
        return;
    }
    let Some(docker) = env::docker_now(shared).await else {
        return;
    };
    let slug = names::repo_slug(&pr.owner, &pr.repo);
    for eco in ECOSYSTEMS {
        let volume = names::main_cache_volume(&slug, pr.number, eco);
        let cmd = docker.command(["volume", "rm", "-f", volume.as_str()]);
        // "No such volume" is the usual answer when main never built anything there.
        match shared.test_exec.output(&cmd, VOLUME_RM_LIMIT).await {
            Ok(out) if out.code == Some(0) => {}
            Ok(out) => tracing::warn!(
                volume = %volume,
                "docker could not remove main's volume: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => tracing::warn!(error = %e, volume = %volume, "cannot remove main's volume"),
        }
    }
}

/// Removes every main worktree whose review file and pull request worktree are both gone and
/// that no request is touching.
pub(crate) async fn sweep_main(shared: &Shared) {
    let Ok(entries) = std::fs::read_dir(shared.paths.worktrees_dir().join(".main")) else {
        return;
    };
    let orphan = |name: &str| {
        let pr = PrRef::from_file_key(name)?;
        let claimed = shared.is_touched(name)
            || shared.paths.review_file(&pr).exists()
            || shared.paths.worktree_for(&pr).exists();
        (!claimed).then_some(pr)
    };
    let found: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    for name in found {
        if orphan(&name).is_none() {
            continue;
        }
        let _serialized = shared.worktree_lock.lock().await;
        // Decided again under the lock: a review may have been opened while this waited.
        if let Some(pr) = orphan(&name) {
            remove_main(shared, &pr).await;
        }
    }
}
