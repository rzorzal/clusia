//! The tests module against a fake machine: `FakeExec` plays docker, git and sandbox-exec, and
//! `FakeClaude` plays the recipe turn.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clusia_core::tests::{
    Approve, RunOutcome, RunnerKind, STEP_ENGINE, STEP_TESTS, StepPlace, StepState, TAIL_MAX,
    TestRun, TestScope, TestStatus, TestsPhase, TestsRecord, TestsView, Totals,
};
use clusia_core::{Config, Paths, PrRef, Review};
use clusia_harness::testkit::{FakeClaude, Script, Turn, near_tests_block, recipe_block};
use clusia_protocol::{ErrorCode, Event, Outcome, Reply};
use clusia_store::{load_tests, save_review, save_tests};
use clusia_testrun::exec::{CommandSpec, Stream};
use clusia_testrun::results::parser_for;
use clusia_testrun::testkit::{FakeExec, FakeReply};
use serde_json::json;
use tokio::sync::broadcast;

use super::*;
use crate::options::DaemonOptions;

const COLIMA_CONTEXT: &str = r#"[{"Name":"colima","Metadata":{},"Endpoints":{"docker":{"Host":"unix:///Users/octo/.colima/default/docker.sock","SkipTLSVerify":false}}}]"#;

fn pr(n: u64) -> PrRef {
    format!("acme/widgets#{n}").parse().unwrap()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Machine {
    Colima { running: bool },
    NoEngine,
}

struct Lab {
    home: tempfile::TempDir,
    fake: tempfile::TempDir,
    exec: Arc<FakeExec>,
    shared: Arc<Shared>,
    events: broadcast::Receiver<(String, Event)>,
}

fn recipe() -> serde_json::Value {
    json!({
        "runner": "auto",
        "image": { "base": "rust:1.86" },
        "setup": [{ "name": "Fetch dependencies", "run": "cargo fetch", "network": true }],
        "tests": {
            "all": "cargo test --workspace",
            "one": "cargo test --workspace -- --exact {name}",
            "list": "cargo test --workspace -- --list --format terse",
            "results": "libtest",
            "env": { "RUST_BACKTRACE": "1" }
        }
    })
}

fn no_setup(mut recipe: serde_json::Value) -> serde_json::Value {
    recipe["setup"] = json!([]);
    recipe
}

fn answer(recipe: serde_json::Value) -> Turn {
    Turn::answer_blocks("Here is the recipe.", &[recipe_block(recipe)])
}

/// A linked worktree of `clone` for `pr(n)`, laid out as `git worktree add` leaves it.
fn worktree(paths: &Paths, clone: &Path, n: u64) -> PathBuf {
    let wt = paths.worktree_for(&pr(n));
    let admin = clone.join(".git/worktrees").join(pr(n).file_key());
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    std::fs::write(
        admin.join("gitdir"),
        format!("{}\n", wt.join(".git").display()),
    )
    .unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
    std::fs::write(wt.join("Cargo.toml"), "[package]\nname = \"widgets\"\n").unwrap();
    std::fs::write(wt.join("Cargo.lock"), "version = 4\n").unwrap();
    wt
}

/// The options of a daemon whose programs are `exec`, with docker and sandbox-exec in
/// `<home>/bin` (present only when a test calls `executable_at` for them).
fn options(exec: &Arc<FakeExec>, home: &Path) -> DaemonOptions {
    DaemonOptions {
        claude_program: Some("/nonexistent/claude".into()),
        codex_program: Some("/nonexistent/codex".into()),
        gh_config_dir: None,
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
        harness_env: Vec::new(),
        test_exec: exec.clone(),
        docker_program: Some(home.join("bin/docker")),
        sandbox_exec_program: home.join("bin/sandbox-exec"),
    }
}

/// The answers of a machine like `machine`; added after a test's own rules, which win.
fn machine_rules(exec: &FakeExec, machine: Machine, home: &Path) {
    // Every git call of the daemon starts with -C <worktree>; the shipped-.clusia check finds nothing.
    exec.on("git", &["-C"], FakeReply::ok(""));
    if let Machine::Colima { running } = machine {
        exec.executable_at(home.join("bin/docker"));
        exec.on(
            "docker",
            &["context", "inspect"],
            FakeReply::ok(COLIMA_CONTEXT),
        );
        let info = if running {
            FakeReply::ok("colima\n")
        } else {
            FakeReply::fail(1, "failed to connect to the docker API")
        };
        exec.on("docker", &["info"], info);
        exec.on(
            "docker",
            &["compose", "version"],
            FakeReply::ok("v2.29.1\n"),
        );
        // `run_container` labels each cache volume before the run.
        exec.on("docker", &["volume", "create"], FakeReply::ok(""));
    }
}

/// Saved reviews of acme/widgets `reviews`, each with a worktree, a fake `claude` playing
/// `turns`, and a machine whose programs answer as `machine` says. `script` adds rules before
/// the lab's own, so they win; it gets the paths, whose worktrees exist by then.
fn lab(
    machine: Machine,
    turns: Vec<Turn>,
    reviews: &[u64],
    script: impl FnOnce(&FakeExec, &Paths),
) -> Lab {
    let home = tempfile::tempdir().unwrap();
    let fake = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    // A script needs a turn; a lab that plays none gets a `claude` that is not there.
    config.harness.program = Some(if turns.is_empty() {
        "/nonexistent/claude".to_string()
    } else {
        FakeClaude::install(fake.path(), Script::turns(turns))
            .display()
            .to_string()
    });
    config.tests.run_timeout_secs = 60;
    let paths = Paths::new(home.path());
    let clone = home.path().join("clone");
    for &n in reviews {
        let review = Review::new(pr(n), "Refresh tokens".into(), "b".into(), "h1".into(), 1);
        save_review(&paths, &review).unwrap();
        worktree(&paths, &clone, n);
    }
    let exec = Arc::new(FakeExec::new());
    script(&exec, &paths);
    machine_rules(&exec, machine, home.path());
    let shared = Arc::new(Shared::new(paths, config, options(&exec, home.path())));
    let events = shared.events.subscribe();
    Lab {
        home,
        fake,
        exec,
        shared,
        events,
    }
}

/// The canonical worktree of `pr(n)`, as the daemon's git calls name it after `-C`.
fn wt(paths: &Paths, n: u64) -> String {
    std::fs::canonicalize(paths.worktree_for(&pr(n)))
        .unwrap()
        .display()
        .to_string()
}

/// `prepare` with the recipe hash of the plan the reviewer sees now.
async fn approve(lab: &Lab, n: u64, approve: Approve) -> Outcome {
    let seen = env::view(&lab.shared, &pr(n))
        .await
        .plan
        .map(|plan| plan.recipe_hash)
        .unwrap_or_default();
    prepare(&lab.shared, &pr(n), &seen, approve).await
}

impl Lab {
    fn worktree(&self, n: u64) -> PathBuf {
        std::fs::canonicalize(self.shared.paths.worktree_for(&pr(n))).unwrap()
    }

    fn recipe_file(&self, n: u64) -> PathBuf {
        self.worktree(n).join(".clusia/recipe.toml")
    }

    /// Every call the fake machine got, as `<program> <args…>`.
    fn ran(&self) -> Vec<String> {
        self.exec.calls().iter().map(line_of).collect()
    }

    fn calls_with(&self, args: &[&str]) -> Vec<CommandSpec> {
        self.exec
            .calls()
            .into_iter()
            .filter(|c| {
                args_of(c).starts_with(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
            })
            .collect()
    }

    fn drain(&mut self) -> Vec<Event> {
        let mut seen = Vec::new();
        while let Ok((_, event)) = self.events.try_recv() {
            seen.push(event);
        }
        seen
    }
}

fn args_of(call: &CommandSpec) -> Vec<String> {
    call.args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn line_of(call: &CommandSpec) -> String {
    format!("{} {}", call.program.display(), args_of(call).join(" "))
}

fn value_after(args: &[String], flag: &str) -> String {
    let at = args.iter().position(|a| a == flag).expect(flag);
    args[at + 1].clone()
}

fn refused(out: Outcome) -> String {
    match out {
        Outcome::Err(e) => {
            assert_eq!(e.code, ErrorCode::InvalidState, "{}", e.message);
            e.message
        }
        other => panic!("not refused: {other:?}"),
    }
}

fn idle(shared: &Shared, n: u64) -> bool {
    shared.tests.with(&pr(n), |entry| entry.activity.is_none())
}

async fn until(lab: &Lab, n: u64, what: &str, done: impl Fn(&TestsView) -> bool) -> TestsView {
    let give_up = Instant::now() + clusia_core::testkit::HANG_BOUND;
    loop {
        let view = env::view(&lab.shared, &pr(n)).await;
        if done(&view) {
            return view;
        }
        assert!(Instant::now() < give_up, "{what}: {view:#?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn settled(lab: &Lab, n: u64) -> TestsView {
    until(lab, n, "the activity ends", |_| idle(&lab.shared, n)).await
}

async fn generated(lab: &Lab, n: u64) -> TestsView {
    assert!(matches!(
        generate(&lab.shared, &pr(n)).await,
        Outcome::Ok(Reply::Ack)
    ));
    settled(lab, n).await
}

async fn ready(lab: &Lab, n: u64) -> TestsView {
    let view = generated(lab, n).await;
    assert_eq!(view.phase, TestsPhase::NeedsSetup, "{view:#?}");
    assert!(matches!(
        approve(lab, n, Approve::All).await,
        Outcome::Ok(Reply::Ack)
    ));
    let view = settled(lab, n).await;
    assert_eq!(view.phase, TestsPhase::Ready, "{view:#?}");
    view
}

async fn started(lab: &Lab, n: u64, scope: TestScope) -> String {
    match run(&lab.shared, &pr(n), scope).await {
        Outcome::Ok(Reply::TestRunStarted { run }) => run,
        other => panic!("{other:?}"),
    }
}

async fn finished(lab: &Lab, n: u64, id: &str) -> TestRun {
    let view = until(lab, n, "the run is stored", |v| {
        idle(&lab.shared, n) && v.runs.first().is_some_and(|r| r.id == id)
    })
    .await;
    view.runs[0].clone()
}

/// The recipe's image already exists on the machine.
fn built(exec: &FakeExec) {
    exec.on("docker", &["image", "inspect"], FakeReply::ok("sha256:1\n"));
}

#[tokio::test]
async fn nothing_runs_before_approval_not_even_list() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(recipe())],
        &[7],
        |x, _| {
            built(x);
            x.on("docker", &["run"], FakeReply::ok(""));
        },
    );
    let view = generated(&lab, 7).await;
    assert_eq!(view.phase, TestsPhase::NeedsSetup);
    let plan = view.plan.clone().unwrap();
    assert!(!plan.ready && !plan.commands_approved, "{plan:#?}");
    assert!(refused(list(&lab.shared, &pr(7)).await).starts_with("Set up the tests first"));
    for scope in [
        TestScope::All,
        TestScope::One {
            name: "store::refresh".into(),
        },
    ] {
        assert!(
            refused(run(&lab.shared, &pr(7), scope).await).starts_with("Set up the tests first")
        );
    }

    // Approving the commands alone sets nothing up: the work step still waits for its own yes.
    let only_tests = Approve::Steps(vec![STEP_TESTS.into()]);
    assert!(matches!(
        approve(&lab, 7, only_tests).await,
        Outcome::Ok(Reply::Ack)
    ));
    let view = settled(&lab, 7).await;
    assert_eq!(view.phase, TestsPhase::NeedsSetup);
    assert!(view.plan.as_ref().unwrap().commands_approved);
    assert!(
        refused(run(&lab.shared, &pr(7), TestScope::All).await)
            .starts_with("Set up the tests first")
    );
    assert!(
        refused(approve(&lab, 7, Approve::Steps(vec!["work:9".into()])).await)
            .contains("There is no step work:9 in the setup plan")
    );

    let ran = lab.ran();
    assert!(
        ran.iter().all(|c| !c.contains("cargo")),
        "recipe text ran: {ran:#?}"
    );
    assert!(
        ran.iter()
            .filter(|c| c.contains(" run "))
            .all(|c| c.contains("io.clusia.kind=probe")),
        "only the mount check's probe started: {ran:#?}"
    );
    assert!(ran.iter().all(|c| !c.contains("sandbox-exec")), "{ran:#?}");
    assert!(
        FakeClaude::calls(lab.fake.path()).len() == 1,
        "only the recipe turn"
    );

    // Once approved, the work step and then the list command run, in a container.
    ready_after(&lab, 7).await;
    assert!(
        lab.ran()
            .iter()
            .any(|c| c.contains(" run ") && c.ends_with("-c cargo fetch"))
    );
    match list(&lab.shared, &pr(7)).await {
        Outcome::Ok(Reply::TestList(tests)) => assert!(tests.is_empty()),
        other => panic!("{other:?}"),
    }
    assert!(
        lab.ran()
            .iter()
            .any(|c| c.ends_with("-c cargo test --workspace -- --list --format terse"))
    );
}

/// `ready` for a review already generated.
async fn ready_after(lab: &Lab, n: u64) {
    assert!(matches!(
        approve(lab, n, Approve::All).await,
        Outcome::Ok(Reply::Ack)
    ));
    let view = settled(lab, n).await;
    assert_eq!(view.phase, TestsPhase::Ready, "{view:#?}");
}

#[tokio::test]
async fn a_changed_recipe_asks_again() {
    let mut locked = no_setup(recipe());
    locked["tests"]["all"] = json!("cargo test --workspace --locked");
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(no_setup(recipe())), answer(locked)],
        &[7],
        |x, _| {
            built(x);
            x.on("docker", &["run"], FakeReply::ok(""));
        },
    );
    let first = ready(&lab, 7).await.plan.unwrap().recipe_hash;

    // The agent writes a new recipe over its own: the approval no longer covers it.
    let view = generated(&lab, 7).await;
    assert_eq!(view.phase, TestsPhase::NeedsSetup);
    let plan = view.plan.unwrap();
    assert_ne!(plan.recipe_hash, first);
    assert!(!plan.commands_approved);
    assert!(
        refused(run(&lab.shared, &pr(7), TestScope::All).await)
            .starts_with("Set up the tests first")
    );
    ready_after(&lab, 7).await;

    // A hand edit asks again too, and its command does not run until approved.
    let text = std::fs::read_to_string(lab.recipe_file(7)).unwrap();
    let edited = text.replace("--locked", "--release");
    assert_ne!(edited, text);
    std::fs::write(lab.recipe_file(7), &edited).unwrap();
    let view = env::view(&lab.shared, &pr(7)).await;
    assert_eq!(view.phase, TestsPhase::NeedsSetup);
    assert!(view.recipe.as_ref().unwrap().by_hand);
    assert!(!view.plan.as_ref().unwrap().commands_approved);
    assert!(
        refused(run(&lab.shared, &pr(7), TestScope::All).await)
            .starts_with("Set up the tests first")
    );
    assert!(lab.ran().iter().all(|c| !c.contains("--release")));
    ready_after(&lab, 7).await;
    let id = started(&lab, 7, TestScope::All).await;
    finished(&lab, 7, &id).await;
    assert!(
        lab.ran()
            .iter()
            .any(|c| c.ends_with("-c cargo test --workspace --release"))
    );
}

#[tokio::test]
async fn a_recipe_shipped_by_the_pr_is_refused() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(recipe())],
        &[7, 8],
        |x, paths| {
            // Review 7's pull request ships `.clusia/recipe.toml`; review 8's ships nothing.
            x.on(
                "git",
                &["-C", wt(paths, 7).as_str()],
                FakeReply::ok(".clusia/recipe.toml\0"),
            );
            x.on("git", &["-C", wt(paths, 8).as_str()], FakeReply::ok(""));
            built(x);
            x.on("docker", &["run"], FakeReply::ok(""));
        },
    );
    let view = generated(&lab, 7).await;
    assert_eq!(
        view.phase,
        TestsPhase::Failed {
            message: clusia_testrun::scan::SHIPPED_CLUSIA.into()
        }
    );
    assert!(
        FakeClaude::calls(lab.fake.path()).is_empty(),
        "no recipe turn"
    );
    assert!(!lab.recipe_file(7).exists());
    let git = lab.calls_with(&["-C", wt(&lab.shared.paths, 7).as_str()]);
    let args = args_of(&git[0]);
    assert_eq!(
        args,
        [
            "-C",
            wt(&lab.shared.paths, 7).as_str(),
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "ls-files",
            "-z",
            "--",
            ".clusia"
        ]
    );
    let gitdir = std::fs::canonicalize(
        lab.home
            .path()
            .join("clone/.git/worktrees")
            .join(pr(7).file_key()),
    )
    .unwrap();
    assert!(
        git[0]
            .env
            .iter()
            .any(|(k, v)| k == "GIT_DIR" && std::path::Path::new(v) == gitdir),
        "the gitdir the daemon verified: {:?}",
        git[0].env
    );
    for out in [
        run(&lab.shared, &pr(7), TestScope::All).await,
        list(&lab.shared, &pr(7)).await,
    ] {
        assert_eq!(refused(out), clusia_testrun::scan::SHIPPED_CLUSIA);
    }
    assert_eq!(
        refused(approve(&lab, 7, Approve::All).await),
        clusia_testrun::scan::SHIPPED_CLUSIA
    );

    // A recipe that turns into a symlink later is refused as well, before anything runs.
    ready(&lab, 8).await;
    let kept = lab.home.path().join("elsewhere.toml");
    std::fs::rename(lab.recipe_file(8), &kept).unwrap();
    std::os::unix::fs::symlink(&kept, lab.recipe_file(8)).unwrap();
    let before = lab.exec.calls().len();
    let message = refused(run(&lab.shared, &pr(8), TestScope::All).await);
    assert_eq!(message, clusia_testrun::scan::SHIPPED_CLUSIA);
    assert!(
        lab.exec.calls()[before..]
            .iter()
            .all(|c| !args_of(c).contains(&"run".to_string())),
        "nothing ran"
    );
}

#[tokio::test]
async fn a_hand_edited_recipe_gets_a_proposal_not_an_overwrite() {
    let mut locked = recipe();
    locked["tests"]["all"] = json!("cargo test --workspace --locked");
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(recipe()), answer(locked)],
        &[7],
        |_, _| {},
    );
    generated(&lab, 7).await;
    let mine = format!(
        "{}\n# my own note\n",
        std::fs::read_to_string(lab.recipe_file(7)).unwrap()
    );
    std::fs::write(lab.recipe_file(7), &mine).unwrap();
    assert!(env::view(&lab.shared, &pr(7)).await.recipe.unwrap().by_hand);

    let view = generated(&lab, 7).await;
    assert_eq!(
        std::fs::read_to_string(lab.recipe_file(7)).unwrap(),
        mine,
        "never overwritten"
    );
    let proposal = view
        .proposal
        .clone()
        .expect("the agent's recipe waits as a proposal");
    assert!(proposal.text.contains("--locked"));
    assert_eq!(
        proposal.hash,
        clusia_testrun::recipe::recipe_hash(proposal.text.as_bytes())
    );
    assert!(view.recipe.as_ref().unwrap().by_hand);

    assert_eq!(
        refused(accept_recipe(&lab.shared, &pr(7), "not-the-shown-one").await),
        clusia_core::tests::RECIPE_CHANGED
    );
    assert_eq!(
        std::fs::read_to_string(lab.recipe_file(7)).unwrap(),
        mine,
        "a stale hash writes nothing"
    );
    let Outcome::Ok(Reply::Tests(view)) = accept_recipe(&lab.shared, &pr(7), &proposal.hash).await
    else {
        panic!("accepted");
    };
    assert_eq!(
        std::fs::read_to_string(lab.recipe_file(7)).unwrap(),
        proposal.text
    );
    assert_eq!(view.proposal, None);
    assert!(!view.recipe.as_ref().unwrap().by_hand, "Clúsia's own again");
    assert_eq!(
        refused(accept_recipe(&lab.shared, &pr(7), &proposal.hash).await),
        "There is no recipe proposal to accept"
    );
}

fn stored_run(id: &str, head: &str) -> TestRun {
    TestRun {
        id: id.into(),
        head: head.into(),
        scope: TestScope::All,
        runner: RunnerKind::Container,
        command: "cargo test --workspace".into(),
        started_at: 1,
        duration_ms: 2,
        exit_code: Some(0),
        outcome: RunOutcome::Passed,
        totals: Totals::default(),
        results: Vec::new(),
        log: None,
        tail: String::new(),
    }
}

#[tokio::test]
async fn a_new_head_makes_runs_stale() {
    let lab = lab(Machine::NoEngine, Vec::new(), &[7], |_, _| {});
    let record = TestsRecord {
        runs: vec![stored_run("r0", "h1")],
        ..TestsRecord::default()
    };
    save_tests(&lab.shared.paths, &pr(7), &record).unwrap();
    let mut review = reviews::load_stored(&lab.shared, &pr(7)).unwrap().unwrap();
    review.head_sha = "h2".into();
    save_review(&lab.shared.paths, &review).unwrap();

    for n in 1..=6 {
        assert!(work::store_run(&lab.shared, &pr(7), stored_run(&format!("r{n}"), "h2")).await);
    }
    let view = env::view(&lab.shared, &pr(7)).await;
    assert_eq!(view.head, "h2");
    let heads: Vec<(&str, &str)> = view
        .runs
        .iter()
        .map(|r| (r.id.as_str(), r.head.as_str()))
        .collect();
    assert_eq!(
        heads,
        [
            ("r6", "h2"),
            ("r5", "h2"),
            ("r4", "h2"),
            ("r3", "h2"),
            ("r2", "h2"),
            ("r0", "h1")
        ],
        "newest first, five per head; the old head's run stays and is stale"
    );
    assert!(view.runs.iter().filter(|r| r.head != view.head).count() == 1);
}

#[tokio::test]
async fn publish_and_discard_take_the_stack_down() {
    let lab = lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[7, 8],
        |x, _| {
            x.on("docker", &["compose", "-p"], FakeReply::ok(""));
        },
    );
    for n in [7, 8] {
        let record = TestsRecord {
            stack: Some(format!("clusia-acme-widgets-1a2b3c-{n}")),
            ..TestsRecord::default()
        };
        save_tests(&lab.shared.paths, &pr(n), &record).unwrap();
    }
    let down = |n: u64| {
        let project = format!("clusia-acme-widgets-1a2b3c-{n}");
        lab.calls_with(&["compose", "-p", &project, "down", "-v", "--remove-orphans"])
            .len()
    };

    // Publishing stops the agent first, then forgets the checks under the review lock.
    crate::agent::stop(&lab.shared, &pr(7)).await;
    {
        let _guard = reviews::lock(&lab.shared, &pr(7)).await;
        crate::agent::forget_checks(&lab.shared, &pr(7));
    }
    assert_eq!(down(7), 1);
    assert!(!lab.shared.paths.tests_file(&pr(7)).exists());

    assert!(matches!(
        reviews::discard(&lab.shared, "test", &pr(8)).await,
        Outcome::Ok(Reply::Ack)
    ));
    assert_eq!(down(8), 1);
    assert!(!lab.shared.paths.tests_file(&pr(8)).exists());
    assert!(!lab.shared.paths.test_logs_for(&pr(8)).exists());
}

#[tokio::test]
async fn closing_a_saved_review_takes_its_stack_down_and_keeps_its_runs() {
    let lab = lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[7],
        |x, _| {
            x.on("docker", &["compose", "-p"], FakeReply::ok(""));
        },
    );
    let record = TestsRecord {
        stack: Some("clusia-acme-widgets-1a2b3c-7".into()),
        runs: vec![stored_run("r1", "h1")],
        ..TestsRecord::default()
    };
    save_tests(&lab.shared.paths, &pr(7), &record).unwrap();
    // A review with a draft is saved, not deleted, when it closes.
    let added = reviews::add_item(
        &lab.shared,
        "test",
        &pr(7),
        clusia_core::DraftKind::General,
        None,
        None,
        "Looks good",
    )
    .await;
    assert!(
        matches!(added, Outcome::Ok(Reply::DraftItem(_))),
        "{added:?}"
    );
    assert!(matches!(
        reviews::close(&lab.shared, "test", &pr(7)).await,
        Outcome::Ok(Reply::Ack)
    ));
    assert_eq!(
        lab.calls_with(&["compose", "-p", "clusia-acme-widgets-1a2b3c-7", "down"])
            .len(),
        1
    );
    let kept = load_tests(&lab.shared.paths, &pr(7)).unwrap();
    assert_eq!(kept.stack, None);
    assert_eq!(kept.runs.len(), 1);
}

#[tokio::test]
async fn the_last_worktree_of_a_repo_removes_its_images_and_volumes() {
    let lab = lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[7, 8],
        |x, _| {
            x.on("docker", &["ps", "-aq"], FakeReply::ok(""));
            x.on("docker", &["network", "ls"], FakeReply::ok(""));
            x.on("docker", &["image", "ls"], FakeReply::ok("sha256:aaa\n"));
            x.on(
                "docker",
                &["volume", "ls"],
                FakeReply::ok("clusia-cache-acme-widgets-1a2b3c-cargo\n"),
            );
            x.on("docker", &["image", "rm"], FakeReply::ok(""));
            x.on("docker", &["volume", "rm"], FakeReply::ok(""));
        },
    );
    std::fs::create_dir_all(lab.shared.paths.worktrees_dir().join("octo~other~1")).unwrap();
    let cache = lab.shared.paths.test_cache_for("acme", "widgets");
    std::fs::create_dir_all(cache.join("cargo")).unwrap();

    reviews::cleanup_checkout(&lab.shared, &pr(7)).await;
    assert!(
        lab.calls_with(&["image", "rm"]).is_empty(),
        "review 8 still uses the image"
    );
    assert!(cache.exists());

    reviews::cleanup_checkout(&lab.shared, &pr(8)).await;
    assert_eq!(
        lab.calls_with(&["image", "rm", "-f", "sha256:aaa"]).len(),
        1
    );
    assert_eq!(
        lab.calls_with(&[
            "volume",
            "rm",
            "-f",
            "clusia-cache-acme-widgets-1a2b3c-cargo"
        ])
        .len(),
        1
    );
    for call in lab.calls_with(&["image", "ls"]) {
        assert!(
            args_of(&call).contains(&"label=io.clusia.repo=acme/widgets".to_string()),
            "by label only: {}",
            line_of(&call)
        );
    }
    assert!(!cache.exists());
}

#[tokio::test]
async fn the_sweep_removes_orphan_containers() {
    let listing = "clusia-run-acme-widgets-1a2b3c-7-1 \n\
                   clusia-run-acme-widgets-1a2b3c-9-3 \n\
                   clusia-acme-widgets-1a2b3c-9-db-1 clusia-acme-widgets-1a2b3c-9\n\
                   clusia-acme-widgets-1a2b3c-7-db-1 clusia-acme-widgets-1a2b3c-7\n";
    let lab = lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[7],
        |x, _| {
            x.on("docker", &["ps", "-a", "--filter"], FakeReply::ok(listing));
            x.on("docker", &["rm"], FakeReply::ok(""));
            x.on("docker", &["compose", "-p"], FakeReply::ok(""));
        },
    );
    // Review 7 runs a test in container 7-1 and has its stack up.
    let record = TestsRecord {
        stack: Some("clusia-acme-widgets-1a2b3c-7".into()),
        ..TestsRecord::default()
    };
    save_tests(&lab.shared.paths, &pr(7), &record).unwrap();
    lab.shared.tests.with(&pr(7), |entry| {
        entry.activity = Some(Activity {
            id: 1,
            doing: Doing::Run,
            phase: TestsPhase::Running { run: "1-1".into() },
            stop: StopFlag::new(),
            turn: Stop::new(),
            container: Some("clusia-run-acme-widgets-1a2b3c-7-1".into()),
        });
    });
    // A record with no review is old enough to go.
    save_tests(&lab.shared.paths, &pr(9), &TestsRecord::default()).unwrap();

    sweep(&lab.shared, crate::sync::now_unix() + 1_000).await;
    assert_eq!(
        lab.calls_with(&["rm", "-f", "clusia-run-acme-widgets-1a2b3c-9-3"])
            .len(),
        1
    );
    assert_eq!(
        lab.calls_with(&[
            "compose",
            "-p",
            "clusia-acme-widgets-1a2b3c-9",
            "down",
            "-v",
            "--remove-orphans"
        ])
        .len(),
        1
    );
    assert!(
        lab.calls_with(&["compose", "-p", "clusia-acme-widgets-1a2b3c-7"])
            .is_empty()
    );
    assert!(
        lab.ran()
            .iter()
            .all(|c| !c.contains("rm -f clusia-run-acme-widgets-1a2b3c-7-1"))
    );
    assert!(!lab.shared.paths.tests_file(&pr(9)).exists());
    assert!(lab.shared.paths.tests_file(&pr(7)).exists());
}

#[tokio::test]
async fn stop_kills_the_container() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(no_setup(recipe()))],
        &[7],
        |x, _| {
            built(x);
            // The mount check exits 0; the test run hangs until it is stopped.
            x.on_sequence(
                "docker",
                &["run"],
                vec![FakeReply::ok(""), FakeReply::hang()],
            );
            x.on("docker", &["kill"], FakeReply::ok(""));
        },
    );
    ready(&lab, 7).await;
    let id = started(&lab, 7, TestScope::All).await;
    until(&lab, 7, "the run starts", |v| {
        v.phase == TestsPhase::Running { run: id.clone() }
    })
    .await;
    let give_up = Instant::now() + clusia_core::testkit::HANG_BOUND;
    while lab.calls_with(&["run"]).len() < 2 {
        assert!(Instant::now() < give_up, "the container never started");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(matches!(stop(&lab.shared, &pr(7)), Outcome::Ok(Reply::Ack)));
    let done = finished(&lab, 7, &id).await;
    assert_eq!(done.outcome, RunOutcome::Stopped);
    assert_eq!(done.exit_code, None);
    let name = value_after(&args_of(&lab.calls_with(&["run"])[1]), "--name");
    assert!(name.starts_with("clusia-run-acme-widgets-"), "{name}");
    // The fake client never ends on a kill, so `run_container` sends its second kill too.
    assert_eq!(
        lab.calls_with(&["kill", &name]).len(),
        2,
        "the container is killed, not only its client"
    );
    assert_eq!(
        env::view(&lab.shared, &pr(7)).await.phase,
        TestsPhase::Ready
    );
}

#[tokio::test]
async fn a_stopped_engine_says_how_to_start_it() {
    let lab = lab(
        Machine::Colima { running: false },
        vec![answer(recipe())],
        &[7],
        |_, _| {},
    );
    let view = generated(&lab, 7).await;
    let env = view.env.unwrap();
    assert!(!env.engine.as_ref().unwrap().running);
    assert_eq!(
        env.runner,
        RunnerKind::Container,
        "an engine that is found counts, running or not"
    );
    assert_eq!(
        env.note.as_deref(),
        Some("Colima is stopped. Start it with: colima start")
    );
    let plan = view.plan.unwrap();
    let first = &plan.steps[0];
    assert_eq!(first.id, STEP_ENGINE);
    assert_eq!(first.command, "colima start");
    assert_eq!(first.place, StepPlace::Host);
    assert_eq!(first.state, StepState::Missing);
}

#[tokio::test]
async fn a_failed_mount_check_blocks_ready_with_the_path() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(no_setup(recipe()))],
        &[7],
        |x, _| {
            built(x);
            x.on("docker", &["run"], FakeReply::fail(3, ""));
        },
    );
    generated(&lab, 7).await;
    assert!(matches!(
        approve(&lab, 7, Approve::All).await,
        Outcome::Ok(Reply::Ack)
    ));
    let view = settled(&lab, 7).await;
    assert_eq!(view.phase, TestsPhase::NeedsSetup);
    let plan = view.plan.unwrap();
    assert!(!plan.ready);
    let blocked = plan.blocked.expect("the mount check blocks the plan");
    assert!(blocked.contains("Colima can't see"), "{blocked}");
    assert!(
        blocked.contains(&lab.worktree(7).display().to_string()),
        "{blocked}"
    );
    assert_eq!(
        refused(run(&lab.shared, &pr(7), TestScope::All).await),
        blocked
    );

    // Trying again checks again.
    let checks = lab.calls_with(&["run"]).len();
    let _ = approve(&lab, 7, Approve::All).await;
    settled(&lab, 7).await;
    assert_eq!(lab.calls_with(&["run"]).len(), checks + 1);
}

#[tokio::test]
async fn a_failed_run_keeps_command_runner_code_and_output() {
    let output = vec![
        (Stream::Out, "running 2 tests\n".to_string()),
        (Stream::Out, "test store::refresh ... ok\n".to_string()),
        (Stream::Out, "test store::expired ... FAILED\n".to_string()),
        (
            Stream::Err,
            "thread 'store::expired' panicked\n".to_string(),
        ),
    ];
    let mut lab = lab(
        Machine::Colima { running: true },
        vec![answer(no_setup(recipe()))],
        &[7],
        |x, _| {
            built(x);
            x.on_sequence(
                "docker",
                &["run"],
                vec![FakeReply::ok(""), FakeReply::chunks(output, 101)],
            );
        },
    );
    ready(&lab, 7).await;
    lab.drain();
    let id = started(&lab, 7, TestScope::All).await;
    let done = finished(&lab, 7, &id).await;
    assert_eq!(done.command, "cargo test --workspace");
    assert_eq!(done.runner, RunnerKind::Container);
    assert_eq!(done.exit_code, Some(101));
    assert_eq!(done.outcome, RunOutcome::Failed);
    assert_eq!(
        done.totals,
        Totals {
            passed: 1,
            failed: 1,
            skipped: 0
        }
    );
    assert!(
        done.tail.contains("test store::expired ... FAILED"),
        "{}",
        done.tail
    );
    assert!(done.tail.contains("panicked"), "{}", done.tail);
    let failed = done
        .results
        .iter()
        .find(|r| r.name == "store::expired")
        .unwrap();
    assert_eq!(failed.status, TestStatus::Failed);
    let log = std::fs::read_to_string(done.log.as_deref().unwrap()).unwrap();
    assert!(
        log.contains("running 2 tests") && log.contains("panicked"),
        "{log}"
    );
    assert_eq!(done.head, "h1");

    let events = lab.drain();
    let results: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::TestResult { run, result, .. } if *run == id => Some(result.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results, ["store::refresh", "store::expired"]);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TestRunDone { run, .. } if run.id == id))
    );
}

#[tokio::test]
async fn failures_from_the_results_after_exit_0_keep_no_exit_code() {
    let output = vec![(Stream::Out, "test store::expired ... FAILED\n".to_string())];
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(no_setup(recipe()))],
        &[7],
        |x, _| {
            built(x);
            x.on_sequence(
                "docker",
                &["run"],
                vec![FakeReply::ok(""), FakeReply::chunks(output, 0)],
            );
        },
    );
    ready(&lab, 7).await;
    let id = started(&lab, 7, TestScope::All).await;
    let done = finished(&lab, 7, &id).await;
    assert_eq!(done.outcome, RunOutcome::Failed);
    assert_eq!(done.totals.failed, 1);
    assert_eq!(
        done.exit_code, None,
        "0 would read as a pass next to Failed"
    );
}

#[tokio::test]
async fn one_activity_per_review_and_parallel_runs_across_reviews() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(no_setup(recipe())), answer(no_setup(recipe()))],
        &[7, 8],
        |x, _| {
            built(x);
            // Two mount checks exit 0; every test run after them hangs.
            x.on_sequence(
                "docker",
                &["run"],
                vec![FakeReply::ok(""), FakeReply::ok(""), FakeReply::hang()],
            );
            x.on("docker", &["kill"], FakeReply::ok(""));
        },
    );
    ready(&lab, 7).await;
    ready(&lab, 8).await;
    let seven = started(&lab, 7, TestScope::All).await;
    until(&lab, 7, "7 runs", |v| {
        v.phase == TestsPhase::Running { run: seven.clone() }
    })
    .await;
    let eight = started(&lab, 8, TestScope::All).await;
    until(&lab, 8, "8 waits for the one place", |v| {
        v.phase == TestsPhase::Waiting
    })
    .await;

    let busy = refused(run(&lab.shared, &pr(7), TestScope::All).await);
    assert_eq!(
        busy,
        "The tests of acme/widgets#7 are busy running the tests; wait for it or stop it"
    );
    assert!(refused(generate(&lab.shared, &pr(7)).await).contains("busy"));
    assert!(refused(approve(&lab, 7, Approve::All).await).contains("busy"));
    assert!(refused(list(&lab.shared, &pr(7)).await).contains("busy"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        lab.calls_with(&["run"]).len(),
        3,
        "two mount checks and one test run"
    );

    let _ = stop(&lab.shared, &pr(7));
    until(&lab, 8, "8 takes the freed place", |v| {
        v.phase == TestsPhase::Running { run: eight.clone() }
    })
    .await;
    shutdown(&lab.shared).await;
    assert_eq!(finished(&lab, 8, &eight).await.outcome, RunOutcome::Stopped);
    assert!(refused(generate(&lab.shared, &pr(7)).await).contains("clusiad is stopping"));
}

#[tokio::test]
async fn output_is_sent_at_most_every_250_ms() {
    let mut lab = lab(Machine::NoEngine, Vec::new(), &[7], |_, _| {});
    let parser = parser_for(&clusia_core::tests::ResultsFormat::Libtest);
    let pr7 = pr(7);
    let mut sink = work::Sink::new(
        &lab.shared,
        &pr7,
        work::To::Run("r1".into()),
        None,
        Some(parser),
    );
    for n in 0..200 {
        sink.chunk(
            Stream::Out,
            format!("line {n} {}\n", "x".repeat(60)).as_bytes(),
        );
    }
    sink.flush();
    tokio::time::sleep(Duration::from_millis(260)).await;
    sink.chunk(Stream::Out, b"test store::refresh ... ok\n");
    sink.flush();
    let outputs: Vec<String> = lab
        .drain()
        .into_iter()
        .filter_map(|e| match e {
            Event::TestOutput { tail, .. } => Some(tail),
            _ => None,
        })
        .collect();
    assert_eq!(
        outputs.len(),
        3,
        "the first chunk, the flush, and one after the pause"
    );
    assert!(outputs.iter().all(|tail| tail.len() <= TAIL_MAX));
    assert!(outputs[1].contains("line 199"));
    assert!(outputs[2].ends_with("test store::refresh ... ok\n"));
}

#[tokio::test]
async fn near_runs_the_named_tests_and_an_empty_set_is_an_error() {
    let near = Turn::answer_blocks(
        "Here is the recipe.",
        &[
            recipe_block(no_setup(recipe())),
            near_tests_block(json!({ "tests": ["store::refresh", "store::it's"] })),
        ],
    );
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(no_setup(recipe())), near],
        &[7, 8],
        |x, _| {
            built(x);
            x.on("docker", &["run"], FakeReply::ok(""));
        },
    );
    ready(&lab, 7).await;
    assert_eq!(
        refused(run(&lab.shared, &pr(7), TestScope::NearChange).await),
        "No tests near this change were found"
    );
    ready(&lab, 8).await;
    let id = started(&lab, 8, TestScope::NearChange).await;
    let done = finished(&lab, 8, &id).await;
    assert_eq!(
        done.command,
        "cargo test --workspace -- --exact 'store::refresh'\ncargo test --workspace -- --exact 'store::it'\\''s'"
    );
    assert!(
        lab.ran()
            .iter()
            .any(|c| c.ends_with("-c cargo test --workspace -- --exact 'store::it'\\''s'"))
    );
}

#[tokio::test]
async fn an_invalid_recipe_fails_and_leaves_the_file_alone() {
    let lab = lab(
        Machine::NoEngine,
        vec![answer(json!({ "tests": {} }))],
        &[7],
        |_, _| {},
    );
    let view = generated(&lab, 7).await;
    let TestsPhase::Failed { message } = view.phase else {
        panic!("{view:#?}")
    };
    assert!(
        message.starts_with("The agent's recipe is not valid: "),
        "{message}"
    );
    assert!(!lab.recipe_file(7).exists());
}

#[tokio::test]
async fn a_review_that_is_not_open_is_refused_but_can_be_read() {
    let lab = lab(Machine::NoEngine, Vec::new(), &[], |_, _| {});
    for out in [
        generate(&lab.shared, &pr(9)).await,
        approve(&lab, 9, Approve::All).await,
        run(&lab.shared, &pr(9), TestScope::All).await,
        list(&lab.shared, &pr(9)).await,
        accept_recipe(&lab.shared, &pr(9), "h").await,
    ] {
        assert!(refused(out).contains("no review for acme/widgets#9; open it first"));
    }
    let Outcome::Ok(Reply::Tests(view)) = get(&lab.shared, &pr(9)).await else {
        panic!()
    };
    assert_eq!(view.phase, TestsPhase::NotGenerated);
}

#[tokio::test]
async fn forget_deletes_the_record_logs_and_tmp() {
    let mut lab = lab(Machine::NoEngine, Vec::new(), &[7], |_, _| {});
    let paths = &lab.shared.paths;
    save_tests(
        paths,
        &pr(7),
        &TestsRecord {
            runs: vec![stored_run("r1", "h1")],
            ..TestsRecord::default()
        },
    )
    .unwrap();
    std::fs::create_dir_all(paths.test_logs_for(&pr(7))).unwrap();
    std::fs::write(paths.test_log(&pr(7), "r1"), "log").unwrap();
    std::fs::create_dir_all(paths.test_tmp_for(&pr(7))).unwrap();
    {
        let _guard = reviews::lock(&lab.shared, &pr(7)).await;
        forget(&lab.shared, &pr(7));
    }
    assert!(!paths.tests_file(&pr(7)).exists());
    assert!(!paths.test_logs_for(&pr(7)).exists());
    assert!(!paths.test_tmp_for(&pr(7)).exists());
    assert!(lab.drain().iter().any(|e| matches!(
        e,
        Event::TestEnv { view, .. } if view.phase == TestsPhase::NotGenerated && view.runs.is_empty()
    )));
}

#[tokio::test]
async fn the_server_shutdown_takes_every_stack_down() {
    let home = tempfile::tempdir().unwrap();
    let exec = Arc::new(FakeExec::new());
    exec.on("docker", &["compose", "-p"], FakeReply::ok(""));
    machine_rules(&exec, Machine::Colima { running: true }, home.path());
    let paths = Paths::new(home.path());
    let record = TestsRecord {
        stack: Some("clusia-acme-widgets-1a2b3c-7".into()),
        ..TestsRecord::default()
    };
    save_tests(&paths, &pr(7), &record).unwrap();
    let daemon = crate::server::Daemon::bind_with(paths.clone(), options(&exec, home.path()))
        .await
        .unwrap();
    let handle = daemon.shutdown_handle();
    let serving = tokio::spawn(daemon.run());
    handle.trigger();
    serving.await.unwrap().unwrap();
    let down = exec
        .calls()
        .into_iter()
        .filter(|c| {
            args_of(c).starts_with(
                &["compose", "-p", "clusia-acme-widgets-1a2b3c-7", "down"].map(String::from),
            )
        })
        .count();
    assert_eq!(down, 1);
    assert_eq!(load_tests(&paths, &pr(7)).unwrap().stack, None);
}

#[tokio::test]
async fn after_shutdown_only_reads_and_stops_are_answered() {
    let lab = lab(Machine::NoEngine, Vec::new(), &[7], |_, _| {});
    shutdown(&lab.shared).await;
    assert_eq!(refused(generate(&lab.shared, &pr(7)).await), STOPPING);
    assert_eq!(refused(disk(&lab.shared).await), STOPPING);
    assert_eq!(
        refused(clean_up(&lab.shared, Some("acme/widgets".into())).await),
        STOPPING
    );
    assert!(matches!(stop(&lab.shared, &pr(7)), Outcome::Ok(Reply::Ack)));
    assert!(matches!(
        get(&lab.shared, &pr(7)).await,
        Outcome::Ok(Reply::Tests(_))
    ));
}

#[tokio::test]
async fn a_local_run_uses_the_offline_profile_and_none_of_the_daemon_env() {
    let mut local = recipe();
    local["setup"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "name": "System packages", "run": "apt-get install -y pkg-config", "into": "image" }));
    let lab = lab(Machine::NoEngine, vec![answer(local)], &[7], |x, _| {
        x.on("sandbox-exec", &["-p"], FakeReply::ok(""));
        x.on(
            "sandbox-exec",
            &["-f"],
            FakeReply::ok("test store::refresh ... ok\n"),
        );
    });
    lab.exec
        .executable_at(lab.home.path().join("bin/sandbox-exec"));
    let view = generated(&lab, 7).await;
    assert_eq!(view.env.as_ref().unwrap().runner, RunnerKind::Local);
    let plan = view.plan.unwrap();
    assert!(
        plan.steps.iter().all(|s| s.id != "image"),
        "no image is built in local mode"
    );
    let image_step = plan.steps.iter().find(|s| s.id == "image:1").unwrap();
    assert!(
        matches!(image_step.state, StepState::Skipped { .. }),
        "image steps are container only"
    );
    assert!(lab.ran().iter().all(|c| !c.contains("apt-get")));
    ready_after(&lab, 7).await;

    let id = started(&lab, 7, TestScope::All).await;
    finished(&lab, 7, &id).await;
    let runs: Vec<CommandSpec> = lab.calls_with(&["-f"]);
    assert_eq!(runs.len(), 2, "the work step, then the test run");
    let (setup, test) = (&runs[0], &runs[1]);
    let setup_profile = PathBuf::from(&args_of(setup)[1]);
    let test_profile = PathBuf::from(&args_of(test)[1]);
    assert!(
        setup_profile.to_string_lossy().ends_with("-online.sb"),
        "the step asked for the network"
    );
    assert!(test_profile.to_string_lossy().ends_with("-offline.sb"));
    assert_eq!(
        args_of(test)[2..],
        ["/bin/sh", "-c", "cargo test --workspace"]
    );
    for profile in [&setup_profile, &test_profile] {
        assert!(profile.starts_with(lab.shared.paths.tests_dir().join("profiles")));
        assert!(!profile.starts_with(lab.worktree(7)));
        assert!(!profile.starts_with(lab.shared.paths.test_tmp_for(&pr(7))));
    }
    assert!(test.clear_env);
    let keys: Vec<String> = test
        .env
        .iter()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    for key in &keys {
        assert!(
            !key.starts_with("CLUSIA_") && !key.starts_with("GH_") && key != "SSH_AUTH_SOCK",
            "{key}"
        );
    }
    assert!(keys.contains(&"TMPDIR".to_string()) && keys.contains(&"RUST_BACKTRACE".to_string()));
    assert!(
        clusia_testrun::container::marker_path(&lab.worktree(7), "work:0").is_file(),
        "the daemon wrote the work step's marker"
    );

    // Nothing Clúsia trusts later sits where a run may write.
    let roots = env::roots(&lab.shared, &env::Ctx::new(&lab.shared, &pr(7)).unwrap()).unwrap();
    let tests_dir = std::fs::canonicalize(lab.shared.paths.tests_dir()).unwrap();
    let override_file = env::override_file(&lab.shared, &pr(7));
    let build_dir = env::build_dir(&lab.shared, &pr(7));
    assert!(
        override_file.starts_with(lab.shared.paths.tests_dir().join("profiles")),
        "{override_file:?}"
    );
    assert!(
        build_dir.starts_with(lab.shared.paths.tests_dir().join("build")),
        "{build_dir:?}"
    );
    for file in [&setup_profile, &test_profile, &override_file, &build_dir] {
        let real = tests_dir.join(file.strip_prefix(lab.shared.paths.tests_dir()).unwrap());
        assert!(
            !clusia_testrun::sandbox::writable_by_sandbox(&roots, &real),
            "{real:?}"
        );
    }
}

#[tokio::test]
async fn the_clone_never_holds_a_credential() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(recipe())],
        &[7],
        |_, _| {},
    );
    let config = lab.home.path().join("clone/.git/config");
    std::fs::write(
        &config,
        "[remote \"origin\"]\n\turl = https://github.com/acme/widgets.git\n",
    )
    .unwrap();
    generated(&lab, 7).await;
    let text = std::fs::read_to_string(&config).unwrap();
    for secret in ["x-access-token", "extraheader", "@github.com"] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
}

#[tokio::test]
async fn a_recipe_changed_after_the_view_is_refused() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(recipe())],
        &[7],
        |x, _| built(x),
    );
    let seen = generated(&lab, 7).await.plan.unwrap().recipe_hash;
    let text = std::fs::read_to_string(lab.recipe_file(7)).unwrap();
    std::fs::write(
        lab.recipe_file(7),
        text.replace("cargo fetch", "cargo fetch --locked"),
    )
    .unwrap();
    assert_eq!(
        refused(prepare(&lab.shared, &pr(7), &seen, Approve::All).await),
        "The recipe changed since you saw the setup plan; look at it again"
    );
    assert_eq!(
        load_tests(&lab.shared.paths, &pr(7)).unwrap().approval,
        None,
        "nothing was approved"
    );
    assert!(idle(&lab.shared, 7), "the slot is free again");
}

fn with_services(mut recipe: serde_json::Value) -> serde_json::Value {
    recipe["services"] = json!({ "compose": "docker-compose.yml", "names": ["db"] });
    recipe
}

const MERGED: &str = r#"{"name":"x","services":{"db":{"image":"postgres:16"}}}"#;

fn compose_file(paths: &Paths, n: u64) {
    let file = paths.worktree_for(&pr(n)).join("docker-compose.yml");
    std::fs::write(file, "services:\n  db:\n    image: postgres:16\n").unwrap();
}

#[tokio::test]
async fn a_view_never_runs_compose() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(with_services(no_setup(recipe())))],
        &[7],
        |x, paths| {
            built(x);
            compose_file(paths, 7);
            x.on("docker", &["compose", "-p"], FakeReply::ok(MERGED));
        },
    );
    generated(&lab, 7).await;
    for _ in 0..3 {
        let _ = get(&lab.shared, &pr(7)).await;
    }
    assert!(
        lab.calls_with(&["compose", "-p"]).is_empty(),
        "{:#?}",
        lab.ran()
    );
    assert!(
        lab.ran().iter().all(|c| !c.contains(" config")),
        "{:#?}",
        lab.ran()
    );
}

#[tokio::test]
async fn the_services_and_stack_steps_bring_up_the_stack_on_its_internal_network() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(with_services(no_setup(recipe())))],
        &[7],
        |x, paths| {
            built(x);
            compose_file(paths, 7);
            x.on("docker", &["compose", "-p"], FakeReply::ok(MERGED));
            x.on("docker", &["run"], FakeReply::ok(""));
        },
    );
    ready(&lab, 7).await;
    let project = clusia_testrun::names::compose_project(
        &clusia_testrun::names::repo_slug("acme", "widgets"),
        7,
    );
    let compose = lab.calls_with(&["compose", "-p", project.as_str()]);
    let words: Vec<Vec<String>> = compose.iter().map(args_of).collect();
    let config = words
        .iter()
        .position(|w| w.contains(&"config".to_string()))
        .expect("config ran");
    let pull = words
        .iter()
        .position(|w| w.contains(&"pull".to_string()))
        .expect("pull ran");
    let up = words
        .iter()
        .position(|w| w.contains(&"up".to_string()))
        .expect("up ran");
    assert!(config < pull && pull < up, "{words:#?}");
    let up = &words[up];
    assert!(
        up.ends_with(
            &["up", "-d", "--wait", "--pull", "never", "--no-build", "db"].map(String::from)
        ),
        "{up:?}"
    );
    let files: Vec<&String> = up
        .iter()
        .enumerate()
        .filter(|(_, w)| *w == "-f")
        .map(|(at, _)| &up[at + 1])
        .collect();
    assert!(
        Path::new(files[1]).starts_with(lab.shared.paths.tests_dir().join("profiles")),
        "{files:?}"
    );
    assert_eq!(
        load_tests(&lab.shared.paths, &pr(7))
            .unwrap()
            .stack
            .as_deref(),
        Some(project.as_str())
    );

    let id = started(&lab, 7, TestScope::All).await;
    finished(&lab, 7, &id).await;
    let runs = lab.calls_with(&["run"]);
    let last = args_of(runs.last().unwrap());
    assert_eq!(
        value_after(&last, "--network"),
        clusia_testrun::compose::internal_network(&project)
    );
}

#[tokio::test]
async fn include_and_extends_are_refused_in_every_form_before_compose_runs() {
    let forms = [
        (
            7,
            "\"include\":\n  - ../../../x.yml\nservices:\n  db:\n    image: postgres:16\n",
        ),
        (
            8,
            "services:\n  db: {extends: {file: /abs/x.yml, service: db}}\n",
        ),
        (
            9,
            r#"{"include": ["../../../x.yml"], "services": {"db": {"image": "postgres:16"}}}"#,
        ),
    ];
    let turns = forms
        .iter()
        .map(|_| answer(with_services(no_setup(recipe()))))
        .collect();
    let lab = lab(
        Machine::Colima { running: true },
        turns,
        &[7, 8, 9],
        |x, paths| {
            built(x);
            for (n, text) in forms {
                std::fs::write(paths.worktree_for(&pr(n)).join("docker-compose.yml"), text)
                    .unwrap();
            }
            x.on("docker", &["compose", "-p"], FakeReply::ok(MERGED));
            x.on("docker", &["run"], FakeReply::ok(""));
        },
    );
    for (n, _) in forms {
        generated(&lab, n).await;
        assert!(matches!(
            approve(&lab, n, Approve::All).await,
            Outcome::Ok(Reply::Ack)
        ));
        let view = settled(&lab, n).await;
        let plan = view.plan.unwrap();
        assert_eq!(
            plan.blocked.as_deref(),
            Some("The compose file uses include or extends, which Clúsia does not follow"),
            "review {n}: {plan:#?}"
        );
    }
    assert!(
        lab.calls_with(&["compose", "-p"]).is_empty(),
        "{:#?}",
        lab.ran()
    );
}

#[tokio::test]
async fn clean_up_refuses_a_repo_that_is_not_a_name() {
    let lab = lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[7],
        |_, _| {},
    );
    let outside = lab.home.path().join("y");
    std::fs::create_dir_all(&outside).unwrap();
    for repo in [
        "x/../../y",
        "x/../y",
        "acme",
        "acme/wid gets",
        "/etc/x",
        "acme/widgets/extra",
    ] {
        assert_eq!(
            refused(clean_up(&lab.shared, Some(repo.into())).await),
            format!("\"{repo}\" is not an owner/repo name")
        );
    }
    assert!(outside.exists(), "nothing was removed");
    assert!(lab.calls_with(&["image", "rm"]).is_empty());
}

#[tokio::test]
async fn clean_up_removes_by_label_and_answers_the_disk() {
    let lab = lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[7],
        |x, _| {
            x.on("docker", &["ps", "-aq"], FakeReply::ok(""));
            x.on("docker", &["ps", "-a", "--filter"], FakeReply::ok(""));
            x.on("docker", &["network", "ls"], FakeReply::ok(""));
            x.on("docker", &["image", "ls"], FakeReply::ok("sha256:aaa\n"));
            x.on("docker", &["volume", "ls"], FakeReply::ok(""));
            x.on("docker", &["image", "rm"], FakeReply::ok(""));
        },
    );
    let cache = lab.shared.paths.test_cache_for("acme", "widgets");
    std::fs::create_dir_all(cache.join("cargo")).unwrap();
    let Outcome::Ok(Reply::TestDisk(_)) = clean_up(&lab.shared, Some("acme/widgets".into())).await
    else {
        panic!("a disk report");
    };
    assert_eq!(
        lab.calls_with(&["image", "rm", "-f", "sha256:aaa"]).len(),
        1
    );
    // The orphan pass looks at this repository's containers only.
    let orphans = lab.calls_with(&["ps", "-a", "--filter"]);
    assert_eq!(orphans.len(), 1);
    assert!(
        args_of(&orphans[0]).contains(&"label=io.clusia.repo=acme/widgets".to_string()),
        "{}",
        line_of(&orphans[0])
    );
    // The removal lists by this repository's label; the report then lists every repository's.
    let removal = lab.calls_with(&["image", "ls", "-q"]);
    assert!(!removal.is_empty());
    for call in removal {
        assert!(
            args_of(&call).contains(&"label=io.clusia.repo=acme/widgets".to_string()),
            "{}",
            line_of(&call)
        );
    }
    assert!(!cache.exists());
}

#[tokio::test]
async fn the_engine_step_starts_it_and_waits() {
    let lab = lab(
        Machine::Colima { running: false },
        vec![answer(no_setup(recipe()))],
        &[7],
        |x, _| {
            built(x);
            // Down when generated and when the setup begins, down at the first poll, then up.
            let down = || FakeReply::fail(1, "failed to connect to the docker API");
            x.on_sequence(
                "docker",
                &["info"],
                vec![down(), down(), down(), FakeReply::ok("colima\n")],
            );
            x.on("colima", &["start"], FakeReply::ok(""));
            x.on("docker", &["run"], FakeReply::ok(""));
        },
    );
    let view = generated(&lab, 7).await;
    assert_eq!(view.plan.unwrap().steps[0].id, STEP_ENGINE);
    // The two-second polls pass at once on a paused clock.
    tokio::time::pause();
    assert!(matches!(
        approve(&lab, 7, Approve::All).await,
        Outcome::Ok(Reply::Ack)
    ));
    let view = settled(&lab, 7).await;
    assert_eq!(view.phase, TestsPhase::Ready, "{view:#?}");
    assert!(view.env.unwrap().engine.unwrap().running);
    let started: Vec<CommandSpec> = lab
        .exec
        .calls()
        .into_iter()
        .filter(|c| c.program.ends_with("colima"))
        .collect();
    assert_eq!(started.len(), 1);
    assert_eq!(args_of(&started[0]), ["start"]);
    assert_eq!(
        lab.calls_with(&["info"]).len(),
        4,
        "it polled until the engine answered"
    );
}

#[tokio::test]
async fn a_stopped_engine_before_a_run_asks_for_setup_again() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(no_setup(recipe()))],
        &[7],
        |x, _| {
            built(x);
            // Up when generated and set up; down when the run probes it again.
            let up = || FakeReply::ok("colima\n");
            x.on_sequence(
                "docker",
                &["info"],
                vec![
                    up(),
                    up(),
                    FakeReply::fail(1, "failed to connect to the docker API"),
                ],
            );
            x.on("docker", &["run"], FakeReply::ok(""));
        },
    );
    ready(&lab, 7).await;
    assert!(
        refused(run(&lab.shared, &pr(7), TestScope::All).await)
            .starts_with("Set up the tests first")
    );
    let view = env::view(&lab.shared, &pr(7)).await;
    assert_eq!(view.phase, TestsPhase::NeedsSetup);
    assert_eq!(view.plan.unwrap().steps[0].id, STEP_ENGINE);
    assert_eq!(
        lab.calls_with(&["run"]).len(),
        1,
        "only the mount check ever ran"
    );
}

#[tokio::test]
async fn twenty_runs_are_kept_in_all() {
    let lab = lab(Machine::NoEngine, Vec::new(), &[7], |_, _| {});
    for n in 0..25 {
        assert!(
            work::store_run(
                &lab.shared,
                &pr(7),
                stored_run(&format!("r{n}"), &format!("h{n}"))
            )
            .await
        );
    }
    let runs = load_tests(&lab.shared.paths, &pr(7)).unwrap().runs;
    let ids: Vec<String> = runs.iter().map(|r| r.id.clone()).collect();
    assert_eq!(
        ids,
        (5..25).rev().map(|n| format!("r{n}")).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_run_past_its_limit_is_timed_out() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(no_setup(recipe()))],
        &[7],
        |x, _| {
            built(x);
            x.on_sequence(
                "docker",
                &["run"],
                vec![FakeReply::ok(""), FakeReply::hang()],
            );
            x.on("docker", &["kill"], FakeReply::ok(""));
        },
    );
    ready(&lab, 7).await;
    lab.shared.config.write().await.tests.run_timeout_secs = 1;
    let id = started(&lab, 7, TestScope::All).await;
    let done = finished(&lab, 7, &id).await;
    assert_eq!(done.outcome, RunOutcome::TimedOut);
    assert_eq!(done.exit_code, None);
}

#[tokio::test]
async fn a_run_that_cannot_start_is_an_error_with_no_exit_code() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(no_setup(recipe()))],
        &[7],
        |x, _| {
            built(x);
            x.on_sequence(
                "docker",
                &["run"],
                vec![FakeReply::ok(""), FakeReply::missing()],
            );
        },
    );
    ready(&lab, 7).await;
    let id = started(&lab, 7, TestScope::All).await;
    let done = finished(&lab, 7, &id).await;
    assert!(
        matches!(done.outcome, RunOutcome::Error { .. }),
        "{:?}",
        done.outcome
    );
    assert_eq!(done.exit_code, None);
}

#[tokio::test]
async fn a_marker_with_other_content_makes_the_step_missing_again() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(recipe())],
        &[7],
        |x, _| {
            built(x);
            x.on("docker", &["run"], FakeReply::ok(""));
        },
    );
    ready(&lab, 7).await;
    // A marker of another inputs hash, as an older setup of the repository would leave.
    clusia_testrun::container::write_marker(&lab.worktree(7), "work:0", "image=old inputs=other")
        .unwrap();
    let view = env::view(&lab.shared, &pr(7)).await;
    let step = view
        .plan
        .as_ref()
        .unwrap()
        .steps
        .iter()
        .find(|s| s.id == "work:0")
        .unwrap()
        .clone();
    assert_eq!(step.state, StepState::Missing);
    assert_eq!(view.phase, TestsPhase::NeedsSetup);
    assert!(
        refused(run(&lab.shared, &pr(7), TestScope::All).await)
            .starts_with("Set up the tests first")
    );
}

#[tokio::test]
async fn the_probe_and_a_stack_coming_up_are_live_for_the_orphan_pass() {
    let slow = || FakeReply::after(Duration::from_millis(1500), FakeReply::ok(""));
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(with_services(no_setup(recipe())))],
        &[7],
        |x, paths| {
            built(x);
            compose_file(paths, 7);
            // config, pull, build, then a slow `up -d --wait`.
            x.on_sequence(
                "docker",
                &["compose", "-p"],
                vec![
                    FakeReply::ok(MERGED),
                    FakeReply::ok(""),
                    FakeReply::ok(""),
                    slow(),
                    FakeReply::ok(""),
                ],
            );
            x.on("docker", &["run"], slow());
        },
    );
    let project = clusia_testrun::names::compose_project(
        &clusia_testrun::names::repo_slug("acme", "widgets"),
        7,
    );
    generated(&lab, 7).await;
    assert!(matches!(
        approve(&lab, 7, Approve::All).await,
        Outcome::Ok(Reply::Ack)
    ));
    let (mut probe_seen, mut stack_seen) = (false, false);
    while !idle(&lab.shared, 7) {
        let live = env::live_names(&lab.shared);
        let probing = lab.shared.tests.with(&pr(7), |entry| {
            entry.activity.as_ref().is_some_and(|a| {
                a.phase
                    == TestsPhase::Preparing {
                        step: "mount".into(),
                    }
            })
        });
        probe_seen |= probing && live.iter().any(|n| n.starts_with("clusia-run-"));
        let stack_recorded = load_tests(&lab.shared.paths, &pr(7))
            .unwrap()
            .stack
            .is_some();
        stack_seen |= !stack_recorded && live.contains(&project);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(probe_seen, "the mount-check probe is live while it runs");
    assert!(stack_seen, "the project is live while `up` runs");
    assert!(
        env::live_names(&lab.shared).contains(&project),
        "then by the record"
    );
}

#[tokio::test]
async fn a_panic_between_begin_and_the_spawn_frees_the_review() {
    let lab = lab(Machine::NoEngine, Vec::new(), &[7], |_, _| {});
    let shared = lab.shared.clone();
    let task = tokio::spawn(async move {
        let started = begin(&shared, &pr(7), Doing::Prepare, TestsPhase::Waiting)
            .unwrap_or_else(|_| panic!("free"));
        let _guard = Ending::new(&shared, &pr(7), started.id, started.live);
        panic!("a bug before the background task starts");
    });
    assert!(task.await.unwrap_err().is_panic());
    assert!(idle(&lab.shared, 7));
    assert!(begin(&lab.shared, &pr(7), Doing::Run, TestsPhase::Waiting).is_ok());
}

#[tokio::test]
async fn the_mount_check_comes_before_the_work_steps() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(recipe())],
        &[7],
        |x, _| {
            built(x);
            x.on("docker", &["run"], FakeReply::fail(3, ""));
        },
    );
    generated(&lab, 7).await;
    assert!(matches!(
        approve(&lab, 7, Approve::All).await,
        Outcome::Ok(Reply::Ack)
    ));
    let view = settled(&lab, 7).await;
    let blocked = view
        .plan
        .unwrap()
        .blocked
        .expect("blocked by the mount check");
    assert!(blocked.contains("Colima can't see"), "{blocked}");
    assert!(
        lab.ran().iter().all(|c| !c.ends_with("-c cargo fetch")),
        "the work step never ran"
    );
}

#[tokio::test]
async fn a_dropped_list_request_frees_the_slot() {
    let lab = lab(
        Machine::Colima { running: true },
        vec![answer(no_setup(recipe()))],
        &[7],
        |x, _| {
            built(x);
            x.on_sequence(
                "docker",
                &["run"],
                vec![FakeReply::ok(""), FakeReply::hang()],
            );
            x.on("docker", &["kill"], FakeReply::ok(""));
        },
    );
    ready(&lab, 7).await;
    let shared = lab.shared.clone();
    let request = tokio::spawn(async move { list(&shared, &pr(7)).await });
    until(&lab, 7, "the list runs", |v| {
        v.phase == TestsPhase::Running { run: "list".into() }
    })
    .await;
    request.abort();
    let _ = stop(&lab.shared, &pr(7));
    let view = settled(&lab, 7).await;
    assert_eq!(view.phase, TestsPhase::Ready);
}

#[test]
fn a_rewritten_git_file_cannot_point_a_mount_elsewhere() {
    let home = tempfile::tempdir().unwrap();
    let paths = Paths::new(home.path());
    let clone = home.path().join("clone");
    let wt = worktree(&paths, &clone, 7);
    // Not canonical on purpose: `/var/folders` is a link to `/private/var/folders`.
    let writable = [wt.clone(), paths.tests_dir()];
    let common = std::fs::canonicalize(clone.join(".git")).unwrap();
    assert_eq!(
        env::git_dirs_of(&wt, &writable),
        Some(env::GitDirs {
            gitdir: common.join("worktrees").join(pr(7).file_key()),
            common
        })
    );

    // A run points `.git` at a folder of its own making, whose back-pointer it also wrote.
    let fake = wt.join("fake/worktrees/x");
    std::fs::create_dir_all(&fake).unwrap();
    std::fs::write(fake.join("commondir"), "../..\n").unwrap();
    std::fs::write(
        fake.join("gitdir"),
        format!("{}\n", wt.join(".git").display()),
    )
    .unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", fake.display())).unwrap();
    assert_eq!(
        env::git_dirs_of(&wt, &writable),
        None,
        "a common dir inside the worktree"
    );

    // Or at a real folder elsewhere, with no back-pointer to this worktree.
    let elsewhere = home.path().join("secrets/worktrees/x");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("commondir"), "../..\n").unwrap();
    std::fs::write(
        wt.join(".git"),
        format!("gitdir: {}\n", elsewhere.display()),
    )
    .unwrap();
    assert_eq!(env::git_dirs_of(&wt, &writable), None, "no back-pointer");
}

#[tokio::test]
async fn the_view_counts_the_listed_tests_of_its_own_head_only() {
    let lab = lab(Machine::NoEngine, Vec::new(), &[7], |_, _| {});
    assert_eq!(env::view(&lab.shared, &pr(7)).await.listed, None);
    let known = |name: &str| clusia_core::tests::KnownTest {
        name: name.into(),
        file: None,
        module: None,
    };
    let record = TestsRecord {
        listing: Some(clusia_core::tests::Listing {
            head: "h1".into(),
            tests: vec![known("a::one"), known("a::two"), known("b::three")],
        }),
        ..TestsRecord::default()
    };
    save_tests(&lab.shared.paths, &pr(7), &record).unwrap();
    let view = env::view(&lab.shared, &pr(7)).await;
    assert_eq!((view.head.as_str(), view.listed), ("h1", Some(3)));
    let mut review = reviews::load_stored(&lab.shared, &pr(7)).unwrap().unwrap();
    review.head_sha = "h2".into();
    save_review(&lab.shared.paths, &review).unwrap();
    assert_eq!(
        env::view(&lab.shared, &pr(7)).await.listed,
        None,
        "a listing of an older head counts nothing"
    );
}

#[tokio::test]
async fn the_engine_check_names_the_engine_and_the_cores() {
    let lab = lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[],
        |_, _| {},
    );
    let Outcome::Ok(Reply::TestEngine(info)) = check_engine(&lab.shared, None).await else {
        panic!("an answer");
    };
    let engine = info.engine.expect("colima is found");
    assert_eq!(engine.kind, clusia_core::tests::EngineKind::Colima);
    assert!(engine.running);
    assert_eq!(info.note, None);
    assert!(info.cores >= 1);
}

#[tokio::test]
async fn the_engine_check_without_an_engine_says_why() {
    let lab = lab(Machine::NoEngine, Vec::new(), &[], |_, _| {});
    let Outcome::Ok(Reply::TestEngine(info)) = check_engine(&lab.shared, None).await else {
        panic!("an answer");
    };
    assert_eq!(info.engine, None);
    assert!(!info.sandbox_ok);
    assert!(
        info.note
            .as_deref()
            .is_some_and(|n| n.contains("sandbox-exec is missing")),
        "{:?}",
        info.note
    );
}

#[tokio::test]
async fn the_engine_check_of_a_review_reads_the_engine_again_and_tells_its_view() {
    let mut lab = lab(
        Machine::Colima { running: true },
        vec![answer(recipe())],
        &[7],
        |_, _| {},
    );
    generated(&lab, 7).await;
    // The engine was not there when the environment was probed.
    env::update_record(&lab.shared, &pr(7), |record| {
        let env = record.env.as_mut().expect("the probe is stored");
        env.engine = None;
        env.note = Some("Start your container engine.".into());
    })
    .await;
    lab.drain();
    let Outcome::Ok(Reply::TestEngine(info)) = check_engine(&lab.shared, Some(pr(7))).await else {
        panic!("an answer");
    };
    assert!(info.engine.as_ref().is_some_and(|e| e.running));
    let told = lab
        .drain()
        .into_iter()
        .find_map(|event| match event {
            Event::TestEnv { view, .. } => Some(view),
            _ => None,
        })
        .expect("the review's view is told");
    let env = told.env.expect("an environment");
    assert!(env.engine.as_ref().is_some_and(|e| e.running));
    assert_eq!(env.note, None);
    assert_eq!(
        FakeClaude::calls(lab.fake.path()).len(),
        1,
        "only the recipe turn: the check spends no agent turn"
    );
}

#[tokio::test]
async fn the_engine_check_tells_no_view_for_a_review_that_is_not_open() {
    let mut lab = lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[],
        |_, _| {},
    );
    lab.drain();
    assert!(matches!(
        check_engine(&lab.shared, Some(pr(99))).await,
        Outcome::Ok(Reply::TestEngine(_))
    ));
    assert!(
        lab.drain()
            .iter()
            .all(|event| !matches!(event, Event::TestEnv { .. }))
    );
}

#[tokio::test]
async fn the_engine_check_is_refused_while_the_daemon_stops() {
    let lab = lab(Machine::NoEngine, Vec::new(), &[], |_, _| {});
    lab.shared
        .tests
        .closing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        refused(check_engine(&lab.shared, None).await),
        "clusiad is stopping"
    );
}
