//! The cockpit's main side, against a real tiny git repository: a merge base, a base that moved
//! on, and a pull request commit.

use std::path::Path;

use clusia_core::Paths;
use clusia_core::cockpit::*;
use clusia_core::tests::{Approve, RECIPE_CHANGED, RunnerKind};
use clusia_harness::testkit::Turn;
use clusia_protocol::{Event, Outcome, Reply};
use clusia_testrun::names;
use clusia_testrun::testkit::{FakeExec, FakeReply};
use serde_json::json;

use super::cockpit_lab::{cockpit_lab_with, cockpit_of, told_cockpit};
use super::functions_lab::{REFRESH_SIG, functions_recipe, index};
use super::lab::{Lab, Machine, idle, pr, refused, settled};
use super::*;

/// The commits of the fixture repository.
pub(super) struct Shas {
    /// Where the pull request branched from.
    pub merge_base: String,
    /// The pull request's head.
    #[allow(dead_code)]
    pub head: String,
    /// `main`'s tip, which holds a commit the pull request does not have.
    pub base_tip: String,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=octo",
            "-c",
            "user.email=octo@example.com",
            "-c",
            "commit.gpgsign=false",
            "-C",
        ])
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn commit(repo: &Path, file: &str, text: &str, message: &str) -> String {
    let path = repo.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", message]);
    git(repo, &["rev-parse", "HEAD"])
}

/// The recipe turn's answer with one `work` setup step, so main has something to approve.
pub(super) fn main_answer() -> Turn {
    let mut recipe = functions_recipe();
    recipe["setup"] =
        json!([{ "name": "Fetch dependencies", "run": "cargo fetch", "network": true }]);
    Turn::answer_blocks(
        "Here is the recipe.",
        &[clusia_harness::testkit::recipe_block(recipe)],
    )
}

/// Main's worktree as the daemon names it after `-C`: the data folder through `realpath`.
pub(super) fn main_wt(paths: &Paths) -> String {
    let root = std::fs::canonicalize(paths.root()).unwrap();
    format!("{}/worktrees/.main/{}", root.display(), pr(7).file_key())
}

/// A `cockpit_lab_with` whose clone is a real repository: commit A (the merge base), commit B
/// on `main` after it (the review's base), and the pull request's commit C on top of A, which
/// is the head of review 7. The review's worktree is a real linked worktree at C.
pub(super) fn git_lab(
    machine: Machine,
    turns: Vec<Turn>,
    script: impl FnOnce(&FakeExec, &Paths),
) -> (Lab, Shas) {
    let lab = cockpit_lab_with(machine, turns, script);
    let clone = lab.home.path().join("clone");
    let worktree = lab.shared.paths.worktree_for(&pr(7));
    std::fs::remove_dir_all(&worktree).unwrap();
    std::fs::remove_dir_all(clone.join(".git")).unwrap();
    git(&clone, &["init", "-q", "-b", "main"]);
    commit(
        &clone,
        "Cargo.toml",
        "[package]\nname = \"widgets\"\n",
        "manifest",
    );
    commit(&clone, "Cargo.lock", "version = 4\n", "lock");
    let merge_base = commit(&clone, "src/store.rs", "pub fn refresh() {}\n", "store");
    let base_tip = commit(&clone, "CHANGELOG.md", "0.1\n", "changelog");
    git(&clone, &["checkout", "-q", "-b", "feature", &merge_base]);
    let head = commit(&clone, "src/feature.rs", "pub fn feature() {}\n", "feature");
    git(&clone, &["checkout", "-q", "main"]);
    let target = worktree.display().to_string();
    git(
        &clone,
        &["worktree", "add", "-q", "--detach", &target, &head],
    );
    let mut review = reviews::load_stored(&lab.shared, &pr(7)).unwrap().unwrap();
    review.base_sha = base_tip.clone();
    review.head_sha = head.clone();
    clusia_store::save_review(&lab.shared.paths, &review).unwrap();
    index(&lab, 7, &head, REFRESH_SIG);
    (
        lab,
        Shas {
            merge_base,
            head,
            base_tip,
        },
    )
}

/// Opens main and approves everything its plan lists, as a reviewer clicking through does. The
/// pull request's own tests are set up already.
pub(super) async fn ready_main(lab: &Lab, shas: &Shas) {
    assert!(matches!(
        open_main(&lab.shared, &pr(7)).await,
        Outcome::Ok(Reply::Ack)
    ));
    settled(lab, 7).await;
    let hash = cockpit_of(cockpit(&lab.shared, &pr(7)).await)
        .main
        .plan
        .expect("main's plan")
        .recipe_hash;
    assert!(matches!(
        prepare_main(&lab.shared, &pr(7), &hash, Approve::All).await,
        Outcome::Ok(Reply::Ack)
    ));
    settled(lab, 7).await;
    let view = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert_eq!(view.main.phase, MainPhase::Ready, "{:#?}", view.main);
    assert_eq!(view.main.base.as_deref(), Some(shas.merge_base.as_str()));
}

fn docker_runs(lab: &Lab) -> usize {
    lab.calls_with(&["run"]).len()
}

fn volume_rm() -> impl FnOnce(&FakeExec, &Paths) {
    |x, _| {
        x.on("docker", &["volume", "rm"], FakeReply::ok(""));
    }
}

fn running() -> Machine {
    Machine::Colima { running: true }
}

async fn pr_ready(turns: Vec<Turn>, script: impl FnOnce(&FakeExec, &Paths)) -> (Lab, Shas) {
    let (lab, shas) = git_lab(running(), turns, script);
    super::lab::ready(&lab, 7).await;
    (lab, shas)
}

#[tokio::test]
async fn open_main_makes_a_worktree_at_the_merge_base_with_its_own_plan() {
    let (mut lab, shas) = pr_ready(vec![main_answer()], |_, _| {}).await;
    lab.drain();
    assert_ne!(shas.merge_base, shas.base_tip);
    assert!(matches!(
        open_main(&lab.shared, &pr(7)).await,
        Outcome::Ok(Reply::Ack)
    ));
    let creating = told_cockpit(&mut lab, "creating", |v| {
        v.main.phase == MainPhase::Creating
    })
    .await;
    assert!(creating.main.plan.is_none());
    let done = told_cockpit(&mut lab, "created", |v| {
        v.main.phase == MainPhase::NeedsSetup
    })
    .await;
    assert_eq!(
        done.main.base.as_deref(),
        Some(shas.merge_base.as_str()),
        "not the base's tip"
    );
    assert_eq!(done.main.at.as_deref(), Some(shas.merge_base.as_str()));
    assert_eq!(done.base.as_deref(), Some(shas.merge_base.as_str()));
    assert!(done.main.first_build);
    let plan = done.main.plan.expect("a plan");
    let pr_plan = env::view(&lab.shared, &pr(7)).await.plan.unwrap();
    assert_eq!(plan.recipe_hash, main_recipe_hash(&pr_plan.recipe_hash));
    assert_ne!(
        plan.recipe_hash, pr_plan.recipe_hash,
        "main's approval is never the PR's"
    );
    assert_eq!(
        plan.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["work:0"]
    );
    assert!(!plan.steps[0].approved && !plan.commands_approved && !plan.ready);
    let tag = done.main.image.expect("the PR's image tag");
    assert!(tag.starts_with("clusia"), "{tag}");
    let main = lab.shared.paths.main_worktree_for(&pr(7));
    assert!(main.join("src/store.rs").is_file() && !main.join("CHANGELOG.md").is_file());
    assert!(
        !main.join("src/feature.rs").exists(),
        "main has none of the pull request's changes"
    );
    let record = env::load(&lab.shared, &pr(7)).main.expect("a record");
    assert_eq!(record.base, shas.merge_base);
    assert_eq!(record.runner, RunnerKind::Container);
    assert!(!record.built && record.approval.is_none());
    assert_eq!(
        docker_runs(&lab),
        2,
        "only the pull request's own mount check and setup step ran"
    );
}

#[tokio::test]
async fn main_runs_nothing_before_its_own_approval() {
    let (lab, _shas) = pr_ready(vec![main_answer()], |_, _| {}).await;
    // The pull request's own mount check and its one setup step.
    let before = docker_runs(&lab);
    assert_eq!(before, 2);
    assert_eq!(
        refused(prepare_main(&lab.shared, &pr(7), "abc", Approve::All).await),
        "Open main first: Run on main too creates its worktree"
    );
    assert!(matches!(
        open_main(&lab.shared, &pr(7)).await,
        Outcome::Ok(Reply::Ack)
    ));
    settled(&lab, 7).await;
    let hash = cockpit_of(cockpit(&lab.shared, &pr(7)).await)
        .main
        .plan
        .unwrap()
        .recipe_hash;
    assert_eq!(
        refused(prepare_main(&lab.shared, &pr(7), "abc", Approve::All).await),
        RECIPE_CHANGED
    );
    assert_eq!(
        refused(
            prepare_main(
                &lab.shared,
                &pr(7),
                &hash,
                Approve::Steps(vec!["work:9".into()])
            )
            .await
        ),
        "There is no step work:9 in the setup plan"
    );
    // Approving nothing walks nothing.
    assert!(matches!(
        prepare_main(&lab.shared, &pr(7), &hash, Approve::Steps(Vec::new())).await,
        Outcome::Ok(Reply::Ack)
    ));
    settled(&lab, 7).await;
    assert_eq!(
        docker_runs(&lab),
        before,
        "no program ran in main's worktree"
    );
    let view = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert_eq!(view.main.phase, MainPhase::NeedsSetup);
    assert!(!view.main.plan.unwrap().steps[0].approved);
    // The step alone is not the tests' commands: main still is not ready.
    prepare_main(
        &lab.shared,
        &pr(7),
        &hash,
        Approve::Steps(vec!["work:0".into()]),
    )
    .await;
    settled(&lab, 7).await;
    // The mount check of main's worktree, then the step.
    assert_eq!(docker_runs(&lab), before + 2);
    let view = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert_eq!(view.main.phase, MainPhase::NeedsSetup);
    let plan = view.main.plan.unwrap();
    assert!(!plan.commands_approved && !plan.ready);
    // A changed recipe is a new hash: the stored approval no longer covers main.
    let record = env::load(&lab.shared, &pr(7)).main.unwrap();
    assert_eq!(record.approval.as_ref().unwrap().recipe_hash, hash);
}

#[tokio::test]
async fn main_has_its_own_worktree_caches_and_temp() {
    let (mut lab, shas) = pr_ready(vec![main_answer()], |_, _| {}).await;
    ready_main(&lab, &shas).await;
    let paths = &lab.shared.paths;
    assert_ne!(paths.main_worktree_for(&pr(7)), paths.worktree_for(&pr(7)));
    assert_ne!(paths.test_main_tmp_for(&pr(7)), paths.test_tmp_for(&pr(7)));
    assert_ne!(
        paths.test_main_cache_for(&pr(7)),
        paths.test_cache_for("acme", "widgets")
    );
    // Main's contexts and roots are its own.
    let pr_ctx = env::Ctx::new(&lab.shared, &pr(7)).unwrap();
    let main_ctx = env::Ctx::for_main(&lab.shared, &pr(7)).unwrap();
    assert_eq!(
        main_ctx.worktree,
        std::fs::canonicalize(paths.main_worktree_for(&pr(7))).unwrap()
    );
    assert_ne!(main_ctx.worktree, pr_ctx.worktree);
    assert_ne!(main_ctx.gitdir, pr_ctx.gitdir);
    assert_eq!(main_ctx.git_common, pr_ctx.git_common, "one repository");
    let pr_roots = env::roots(&lab.shared, &pr_ctx).unwrap();
    let main_roots = env::roots_for_main(&lab.shared, &main_ctx, None).unwrap();
    assert_eq!(main_roots.worktree, main_ctx.worktree);
    assert_ne!(main_roots.review_tmp, pr_roots.review_tmp);
    assert_ne!(main_roots.repo_cache, pr_roots.repo_cache);
    assert!(
        main_roots
            .review_tmp
            .ends_with(paths.test_main_tmp_for(&pr(7)).file_name().unwrap())
    );
    assert!(
        main_roots
            .repo_cache
            .ends_with(format!("main/{}", pr(7).file_key()))
    );
    assert!(main_roots.read_only.is_empty());
    let folder = lab.home.path().join("run-folder");
    std::fs::create_dir_all(&folder).unwrap();
    let with_folder = env::roots_for_main(&lab.shared, &main_ctx, Some(&folder)).unwrap();
    assert_eq!(
        with_folder.read_only,
        vec![std::fs::canonicalize(&folder).unwrap()]
    );
    // Main's container runs mount main's worktree and its own volumes, never the PR's.
    let env_state = env::load(&lab.shared, &pr(7)).env.unwrap();
    let mounts = main_tree::mounts_for_main(&main_ctx, &env_state);
    assert_eq!(mounts.worktree, main_ctx.worktree);
    assert!(mounts.scenario.is_none());
    let slug = names::repo_slug("acme", "widgets");
    let main_volume = names::main_cache_volume(&slug, 7, clusia_core::tests::Ecosystem::Cargo);
    let pr_volume = names::cache_volume(&slug, clusia_core::tests::Ecosystem::Cargo);
    assert_ne!(main_volume, pr_volume);
    assert!(
        mounts
            .caches
            .iter()
            .any(|(volume, _)| *volume == main_volume)
    );
    assert!(mounts.caches.iter().all(|(volume, _)| *volume != pr_volume));
    // The step that ran in main's container used those, and the same image as the PR.
    let runs = lab.calls_with(&["run"]);
    let main_run = runs.last().unwrap();
    let line = super::lab::line_of(main_run);
    assert!(
        line.contains(&main_ctx.worktree.display().to_string()),
        "{line}"
    );
    assert!(line.contains(&main_volume), "{line}");
    assert!(!line.contains(&pr_volume), "{line}");
    assert!(
        !line.contains(&pr_ctx.worktree.display().to_string()),
        "{line}"
    );
    let tag = cockpit_of(cockpit(&lab.shared, &pr(7)).await)
        .main
        .image
        .unwrap();
    assert!(line.contains(&tag), "{line}");
    lab.drain();
}

#[tokio::test]
async fn main_goes_with_the_review_and_with_prune() {
    let (lab, shas) = pr_ready(vec![main_answer()], volume_rm()).await;
    ready_main(&lab, &shas).await;
    let paths = lab.shared.paths.clone();
    let (main, tmp, cache) = (
        paths.main_worktree_for(&pr(7)),
        paths.test_main_tmp_for(&pr(7)),
        paths.test_main_cache_for(&pr(7)),
    );
    for dir in [&tmp, &cache] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("kept.txt"), "x").unwrap();
    }
    assert!(main.is_dir());
    reviews::cleanup_checkout(&lab.shared, &pr(7)).await;
    assert!(!main.exists() && !tmp.exists() && !cache.exists());
    assert!(!paths.worktree_for(&pr(7)).exists());
    let clone = lab.home.path().join("clone");
    assert!(!git(&clone, &["worktree", "list"]).contains(".main"));
    let slug = names::repo_slug("acme", "widgets");
    let removed: Vec<String> = lab
        .calls_with(&["volume", "rm"])
        .iter()
        .flat_map(super::lab::args_of)
        .collect();
    assert!(
        removed.contains(&names::main_cache_volume(
            &slug,
            7,
            clusia_core::tests::Ecosystem::Cargo
        )),
        "{removed:?}"
    );
    // The record goes with the review: forget deletes it, main's approval included.
    {
        let _guard = reviews::lock(&lab.shared, &pr(7)).await;
        forget(&lab.shared, &pr(7));
    }
    assert!(env::load(&lab.shared, &pr(7)).main.is_none());
}

#[tokio::test]
async fn the_sweep_removes_a_main_worktree_whose_review_and_worktree_are_gone() {
    let (lab, shas) = pr_ready(vec![main_answer()], volume_rm()).await;
    assert!(matches!(
        open_main(&lab.shared, &pr(7)).await,
        Outcome::Ok(Reply::Ack)
    ));
    settled(&lab, 7).await;
    let paths = lab.shared.paths.clone();
    let main = paths.main_worktree_for(&pr(7));
    assert!(main.is_dir());
    // Still claimed by a review and a worktree: kept.
    sweep_main(&lab.shared).await;
    assert!(main.is_dir());
    let clone = lab.home.path().join("clone");
    git(
        &clone,
        &[
            "worktree",
            "remove",
            "--force",
            &paths.worktree_for(&pr(7)).display().to_string(),
        ],
    );
    assert!(main.is_dir(), "the review file still claims it");
    std::fs::remove_file(paths.review_file(&pr(7))).unwrap();
    lab.shared.touch(&pr(7));
    sweep_main(&lab.shared).await;
    assert!(main.is_dir(), "a review being touched keeps it");
    lab.shared.touched.lock().unwrap().clear();
    sweep_main(&lab.shared).await;
    assert!(!main.exists());
    assert!(!git(&clone, &["worktree", "list"]).contains(".main"));
    let _ = shas;
}

#[tokio::test]
async fn a_rebase_moves_main_and_keeps_its_approval() {
    let (lab, shas) = pr_ready(vec![main_answer()], |_, _| {}).await;
    ready_main(&lab, &shas).await;
    let clone = lab.home.path().join("clone");
    // The pull request is rebased onto main's tip: the merge base moves there.
    git(&clone, &["checkout", "-q", "-b", "rebased", &shas.base_tip]);
    let head = commit(
        &clone,
        "src/feature.rs",
        "pub fn feature() {}\n",
        "feature again",
    );
    git(&clone, &["checkout", "-q", "main"]);
    let mut review = reviews::load_stored(&lab.shared, &pr(7)).unwrap().unwrap();
    review.head_sha = head.clone();
    clusia_store::save_review(&lab.shared.paths, &review).unwrap();
    index(&lab, 7, &head, REFRESH_SIG);
    let before = docker_runs(&lab);
    let moved = main_tree::ensure_main_at_base(&lab.shared, &pr(7))
        .await
        .unwrap();
    assert_eq!(moved.base, shas.base_tip);
    assert!(
        moved.approval.is_some(),
        "the approval is not tied to the base"
    );
    let view = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert_eq!(view.main.at.as_deref(), Some(shas.base_tip.as_str()));
    assert_eq!(view.base.as_deref(), Some(shas.base_tip.as_str()));
    assert!(
        lab.shared
            .paths
            .main_worktree_for(&pr(7))
            .join("CHANGELOG.md")
            .is_file()
    );
    assert_eq!(
        view.main.phase,
        MainPhase::Ready,
        "the markers still match: same inputs"
    );
    // Nothing else ran: no new docker run, no network.
    assert_eq!(docker_runs(&lab), before);
}

#[tokio::test]
async fn a_recreated_main_worktree_starts_from_a_first_build_and_keeps_its_approval() {
    let (lab, shas) = pr_ready(vec![main_answer()], volume_rm()).await;
    ready_main(&lab, &shas).await;
    env::update_record(&lab.shared, &pr(7), |record| {
        record.main.as_mut().unwrap().built = true
    })
    .await;
    // Prune took main's worktree, and with it its caches; the record stayed.
    {
        let _serialized = lab.shared.worktree_lock.lock().await;
        remove_main(&lab.shared, &pr(7)).await;
    }
    assert!(!lab.shared.paths.main_worktree_for(&pr(7)).exists());
    let view = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert_eq!(
        view.main.phase,
        MainPhase::NotCreated,
        "a record with no worktree is ignored"
    );
    assert!(matches!(
        open_main(&lab.shared, &pr(7)).await,
        Outcome::Ok(Reply::Ack)
    ));
    settled(&lab, 7).await;
    let record = env::load(&lab.shared, &pr(7)).main.unwrap();
    assert!(!record.built, "the new worktree builds from scratch");
    assert!(
        record.approval.is_some(),
        "the approval is tied to the recipe, not to the folder"
    );
    let view = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert!(view.main.first_build);
    assert_eq!(
        view.main.phase,
        MainPhase::NeedsSetup,
        "the markers went with the folder"
    );
}

#[tokio::test]
async fn a_main_tree_that_ships_clusia_is_refused_and_stores_nothing() {
    let (mut lab, _shas) = pr_ready(vec![main_answer()], |x, paths| {
        x.on(
            "git",
            &["-C", main_wt(paths).as_str()],
            FakeReply::ok(".clusia/e2e/x/scenario.toml\0"),
        );
    })
    .await;
    assert!(matches!(
        open_main(&lab.shared, &pr(7)).await,
        Outcome::Ok(Reply::Ack)
    ));
    let view = told_cockpit(&mut lab, "failed", |v| {
        matches!(v.main.phase, MainPhase::Failed { .. })
    })
    .await;
    assert_eq!(
        view.main.phase,
        MainPhase::Failed {
            message: "Main ships its own .clusia/ folder; Clúsia won't use it.".into()
        }
    );
    assert!(env::load(&lab.shared, &pr(7)).main.is_none());
}

#[tokio::test]
async fn open_main_is_refused_without_a_recipe_with_services_or_while_busy() {
    let (lab, _shas) = git_lab(running(), Vec::new(), |_, _| {});
    assert_eq!(
        refused(open_main(&lab.shared, &pr(7)).await),
        super::NO_RECIPE
    );
    let mut services = functions_recipe();
    services["services"] = json!({ "compose": "docker-compose.yml", "names": ["db"] });
    let (lab, _shas) = git_lab(
        running(),
        vec![Turn::answer_blocks(
            "r",
            &[clusia_harness::testkit::recipe_block(services)],
        )],
        |_, _| {},
    );
    super::lab::generated(&lab, 7).await;
    assert_eq!(
        refused(open_main(&lab.shared, &pr(7)).await),
        "Comparing with main isn't supported for recipes with services yet"
    );
    let view = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert_eq!(
        view.main.phase,
        MainPhase::Unavailable {
            reason: "Comparing with main isn't supported for recipes with services yet".into()
        }
    );
    let (lab, _shas) = git_lab(running(), vec![Turn::hanging()], |_, _| {});
    assert!(matches!(
        write_scenarios(&lab.shared, &pr(7), ScenarioAsk::Fresh).await,
        Outcome::Ok(Reply::Ack)
    ));
    // The recipe is checked before the slot, so a review with no recipe says so even when busy.
    assert_eq!(
        refused(open_main(&lab.shared, &pr(7)).await),
        super::NO_RECIPE
    );
    assert!(matches!(stop(&lab.shared, &pr(7)), Outcome::Ok(Reply::Ack)));
    settled(&lab, 7).await;
    assert!(idle(&lab.shared, 7));
}

#[tokio::test]
async fn a_record_of_another_runner_is_unavailable_until_opened_again() {
    let (lab, shas) = pr_ready(vec![main_answer()], |_, _| {}).await;
    ready_main(&lab, &shas).await;
    env::update_record(&lab.shared, &pr(7), |record| {
        record.main.as_mut().unwrap().runner = RunnerKind::Local;
    })
    .await;
    let view = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert_eq!(
        view.main.phase,
        MainPhase::Unavailable {
            reason: "Main was prepared for another runner; open it again".into()
        }
    );
    assert!(matches!(
        open_main(&lab.shared, &pr(7)).await,
        Outcome::Ok(Reply::Ack)
    ));
    settled(&lab, 7).await;
    let view = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert_eq!(
        view.main.phase,
        MainPhase::Ready,
        "the approval and the markers stayed"
    );
    assert_eq!(
        env::load(&lab.shared, &pr(7)).main.unwrap().runner,
        RunnerKind::Container
    );
}

#[tokio::test]
async fn preparing_main_tells_each_step_and_nothing_after_it_ends() {
    let (mut lab, _shas) = pr_ready(vec![main_answer()], |_, _| {}).await;
    open_main(&lab.shared, &pr(7)).await;
    settled(&lab, 7).await;
    let hash = cockpit_of(cockpit(&lab.shared, &pr(7)).await)
        .main
        .plan
        .unwrap()
        .recipe_hash;
    lab.drain();
    prepare_main(&lab.shared, &pr(7), &hash, Approve::All).await;
    told_cockpit(&mut lab, "preparing", |v| {
        matches!(v.main.phase, MainPhase::Preparing { .. })
    })
    .await;
    settled(&lab, 7).await;
    // After the end nothing is told any more.
    lab.drain();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        lab.drain()
            .iter()
            .all(|e| !matches!(e, Event::CockpitChanged { .. }))
    );
}
