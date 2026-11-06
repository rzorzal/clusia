//! Single runs and comparisons against a fake machine: the local sandbox's two profiles tell
//! the PR's runs from main's, a scripted process group feeds the sampler, and a real tiny git
//! repository gives main its merge base.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clusia_core::Paths;
use clusia_core::cockpit::{
    COCKPIT_CHANGED, CockpitScope, CockpitView, CompareOutcome, DeltaLabel, ErrorLine, Freshness,
    ItemKind, ItemRef, LOAD_LATER, MAIN_FIRST, Metric, NO_SCENARIOS, NOT_APPROVED, RoundEnd,
    RoundResult, RunSide, Sample, ScenarioApproval, ScenarioInput, StoredBench, Verdict,
};
use clusia_core::functions::{FunctionLang, function_slug};
use clusia_core::tests::RunnerKind;
use clusia_harness::testkit::Turn;
use clusia_protocol::{ErrorCode, Event, Outcome, Reply};
use clusia_testrun::exec::{CommandSpec, GroupUsage, ProcUsage, Stream};
use clusia_testrun::names::sha256_hex;
use clusia_testrun::testkit::{FakeExec, FakeReply};
use serde_json::json;

use super::cockpit_lab::{approve_scenario_of, put_scenario, ready_lab_with, scenario_view};
use super::functions_lab::{REFRESH_SIG, RUST_COMMAND};
use super::lab::{
    Lab, Machine, answer, args_of, generated, idle, no_setup, pr, ready, ready_after, recipe,
    refused,
};
use super::main_lab::{Shas, git_lab, ready_main};
use super::*;

const SLUG: &str = "two-refreshes";
const RUN: &str = "python3 {script} --callers {callers} --text {text}";
const SCRIPT: &str = "print('two refreshes')\n";
const LOOP: &str = "#[test]\nfn clusia_case() { /* calls TokenStore::refresh in a loop */ }\n";
const GROUP: u32 = 4242;

/// The recipe turn: no setup steps, and a command for Rust functions.
fn recipe_turn() -> Turn {
    let mut recipe = no_setup(recipe());
    recipe["functions"] = json!({ "rust": RUST_COMMAND });
    answer(recipe)
}

fn item(kind: ItemKind, id: &str) -> ItemRef {
    ItemRef {
        kind,
        id: id.into(),
    }
}

fn one(slug: &str) -> CockpitScope {
    CockpitScope::Item {
        item: item(ItemKind::Scenario, slug),
    }
}

fn profile(paths: &Paths, side: RunSide) -> String {
    env::cockpit_profile_path(paths, &pr(7), side)
        .display()
        .to_string()
}

/// A scenario that passes and reports one output, with a line of noise on each stream.
fn passing() -> FakeReply {
    FakeReply::marked(
        vec![
            (Stream::Out, "starting\n@MARK@ output exchanges 1\n".into()),
            (Stream::Err, "warn: slow disk\n".into()),
        ],
        0,
    )
}

fn usage() -> GroupUsage {
    GroupUsage {
        procs: vec![ProcUsage {
            pid: GROUP,
            rss_bytes: 8 * 1024 * 1024,
            cpu_ns: 125_000_000,
        }],
    }
}

/// The rules every local cockpit lab needs: the sandbox, its health, a group to sample, and
/// the PR's (and main's) measured runs.
fn local_rules(x: &FakeExec, paths: &Paths, pr_side: Vec<FakeReply>, main_side: Vec<FakeReply>) {
    x.executable_at(paths.root().join("bin/sandbox-exec"));
    x.on("sandbox-exec", &["-p"], FakeReply::ok(""));
    x.on_sequence(
        "sandbox-exec",
        &["-f", &profile(paths, RunSide::Pr)],
        pr_side,
    );
    x.on_sequence(
        "sandbox-exec",
        &["-f", &profile(paths, RunSide::Main)],
        main_side,
    );
    x.group(GROUP, vec![Some(usage())]);
}

/// Review 7 in the local sandbox, set up, with scenario `SLUG` approved.
async fn local_cockpit(pr_side: Vec<FakeReply>) -> Lab {
    let lab = ready_lab_with(Machine::NoEngine, vec![], move |x, paths| {
        local_rules(x, paths, pr_side, vec![FakeReply::ok("")])
    })
    .await;
    put_scenario(&lab, SLUG, RUN, &[("run.py", SCRIPT)]);
    approve_scenario_of(&lab, SLUG).await;
    lab
}

/// Review 7 in the local sandbox of a real git repository, set up, main prepared at the
/// merge base, and scenario `SLUG` approved.
async fn compared_lab(pr_side: Vec<FakeReply>, main_side: Vec<FakeReply>) -> (Lab, Shas) {
    let (lab, shas) = git_lab(Machine::NoEngine, vec![recipe_turn()], move |x, paths| {
        local_rules(x, paths, pr_side, main_side)
    });
    ready(&lab, 7).await;
    ready_main(&lab, &shas).await;
    put_scenario(&lab, SLUG, RUN, &[("run.py", SCRIPT)]);
    approve_scenario_of(&lab, SLUG).await;
    (lab, shas)
}

/// The same in containers: every `docker run` of the cockpit passes, every `docker exec`
/// reads one cgroup sample.
async fn container_compared() -> (Lab, Shas) {
    let (lab, shas) = git_lab(
        Machine::Colima { running: true },
        vec![recipe_turn()],
        |x, _| {
            x.on("docker", &["run"], passing());
            x.on(
                "docker",
                &["exec"],
                FakeReply::chunks(
                    vec![(
                        Stream::Out,
                        "clusia-sample 10485760 2097152 1000000\n".into(),
                    )],
                    0,
                ),
            );
        },
    );
    ready(&lab, 7).await;
    ready_main(&lab, &shas).await;
    put_scenario(&lab, SLUG, RUN, &[("run.py", SCRIPT)]);
    approve_scenario_of(&lab, SLUG).await;
    (lab, shas)
}

fn seen_of(view: &CockpitView, scope: &CockpitScope) -> String {
    match scope {
        CockpitScope::Scenarios => view.hash.clone(),
        CockpitScope::Item { item } if item.kind == ItemKind::Scenario => view
            .scenarios
            .iter()
            .find(|s| s.slug == item.id)
            .expect("listed")
            .hash
            .clone(),
        CockpitScope::Item { item } => view
            .benches
            .iter()
            .find(|b| b.slug == item.id)
            .expect("listed")
            .hash
            .clone(),
    }
}

async fn view_now(lab: &Lab) -> CockpitView {
    scenarios::read(&lab.shared, &pr(7)).await.view
}

/// Starts a run of `scope` with the hash of the view now; answers its id and the skipped.
async fn start(lab: &Lab, scope: CockpitScope, on_main: bool) -> (String, Vec<String>) {
    let seen = seen_of(&view_now(lab).await, &scope);
    match run_cockpit(&lab.shared, &pr(7), scope, on_main, seen).await {
        Outcome::Ok(Reply::CockpitStarted { run, skipped }) => (run, skipped),
        other => panic!("not started: {other:?}"),
    }
}

/// What run `run` sent, once it ended and the view said so: its rounds and its sample batches.
async fn rounds(lab: &mut Lab, run: &str) -> (Vec<RoundResult>, Vec<(RunSide, u32, Vec<Sample>)>) {
    let give_up = Instant::now() + clusia_core::testkit::HANG_BOUND;
    let (mut done, mut samples, mut ended) = (Vec::new(), Vec::new(), false);
    loop {
        let finished = idle(&lab.shared, 7);
        for event in lab.drain() {
            match event {
                Event::CockpitRound {
                    run: id, result, ..
                } if id == run => done.push(*result),
                Event::CockpitSamples {
                    run: id,
                    side,
                    round,
                    points,
                    ..
                } if id == run => samples.push((side, round, points)),
                Event::CockpitChanged { view, .. } if view.activity.is_none() => ended = true,
                _ => {}
            }
        }
        if finished && ended {
            return (done, samples);
        }
        assert!(Instant::now() < give_up, "run {run} did not end");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The cockpit's local runs, in the order they started.
fn measured(lab: &Lab) -> Vec<CommandSpec> {
    let (pr_side, main_side) = (
        profile(&lab.shared.paths, RunSide::Pr),
        profile(&lab.shared.paths, RunSide::Main),
    );
    lab.exec
        .calls()
        .into_iter()
        .filter(|c| {
            let args = args_of(c);
            args.len() > 1 && args[0] == "-f" && (args[1] == pr_side || args[1] == main_side)
        })
        .collect()
}

fn side_of(lab: &Lab, call: &CommandSpec) -> RunSide {
    if args_of(call)[1] == profile(&lab.shared.paths, RunSide::Main) {
        RunSide::Main
    } else {
        RunSide::Pr
    }
}

/// The cockpit's container runs (never the mount check's probe), in order.
fn container_runs(lab: &Lab) -> Vec<Vec<String>> {
    lab.calls_with(&["run"])
        .iter()
        .map(args_of)
        .filter(|a| a.iter().any(|x| x == "io.clusia.kind=run"))
        .collect()
}

fn env_of(call: &CommandSpec) -> BTreeMap<String, String> {
    call.env
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

fn main_worktree(lab: &Lab) -> PathBuf {
    fs::canonicalize(lab.shared.paths.main_worktree_for(&pr(7))).unwrap()
}

fn bad(out: Outcome) -> String {
    match out {
        Outcome::Err(e) => {
            assert_eq!(e.code, ErrorCode::BadRequest, "{}", e.message);
            e.message
        }
        other => panic!("not refused: {other:?}"),
    }
}

/// Waits until `n` cockpit runs have started.
async fn started_runs(lab: &Lab, n: usize) {
    let give_up = Instant::now() + clusia_core::testkit::HANG_BOUND;
    while measured(lab).len() < n {
        assert!(
            Instant::now() < give_up,
            "{} runs started, not {n}",
            measured(lab).len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn run_cockpit_refuses_in_order() {
    let lab = super::cockpit_lab::cockpit_lab(vec![recipe_turn()]);
    let p = pr(7);
    let load = CockpitScope::Item {
        item: item(ItemKind::Load, "get-token"),
    };
    assert_eq!(
        refused(run_cockpit(&lab.shared, &p, load, false, "x".into()).await),
        LOAD_LATER
    );
    assert_eq!(
        bad(run_cockpit(&lab.shared, &p, one("Bad Slug"), false, "x".into()).await),
        "invalid scenario id"
    );
    let bench = CockpitScope::Item {
        item: item(ItemKind::Bench, "../x"),
    };
    assert_eq!(
        bad(run_cockpit(&lab.shared, &p, bench, false, "x".into()).await),
        "invalid function id"
    );
    put_scenario(&lab, SLUG, RUN, &[("run.py", SCRIPT)]);
    assert_eq!(
        refused(run_cockpit(&lab.shared, &p, one(SLUG), false, "x".into()).await),
        GENERATE_FIRST
    );
    generated(&lab, 7).await;
    assert_eq!(
        refused(run_cockpit(&lab.shared, &p, one(SLUG), false, "x".into()).await),
        SET_UP_FIRST
    );
    ready_after(&lab, 7).await;
    let view = view_now(&lab).await;
    assert_eq!(
        refused(run_cockpit(&lab.shared, &p, one(SLUG), false, "stale".into()).await),
        COCKPIT_CHANGED
    );
    assert_eq!(
        refused(run_cockpit(&lab.shared, &p, one("nope"), false, view.hash.clone()).await),
        "No scenario nope in acme/widgets#7"
    );
    assert_eq!(
        refused(
            run_cockpit(
                &lab.shared,
                &p,
                one(SLUG),
                false,
                seen_of(&view, &one(SLUG))
            )
            .await
        ),
        NOT_APPROVED
    );
    assert_eq!(
        refused(
            run_cockpit(
                &lab.shared,
                &p,
                CockpitScope::Scenarios,
                false,
                view.hash.clone()
            )
            .await
        ),
        NO_SCENARIOS
    );
    approve_scenario_of(&lab, SLUG).await;
    let view = view_now(&lab).await;
    assert_eq!(
        refused(run_cockpit(&lab.shared, &p, one(SLUG), true, seen_of(&view, &one(SLUG))).await),
        MAIN_FIRST
    );
    assert!(
        container_runs(&lab).is_empty(),
        "nothing ran: {:#?}",
        lab.ran()
    );
    assert!(idle(&lab.shared, 7));
}

#[tokio::test]
async fn a_single_run_measures_one_round_on_this_pr() {
    let slow = FakeReply::after(
        Duration::from_millis(700),
        FakeReply::marked(
            vec![(
                Stream::Out,
                "@MARK@ output exchanges 1\n@MARK@ error token expired\n".into(),
            )],
            1,
        ),
    );
    let mut lab = local_cockpit(vec![slow]).await;
    let runs = lab.shared.paths.test_runs_for(&pr(7));
    let leftover = runs.join("1-1-0");
    fs::create_dir_all(&leftover).unwrap();
    fs::write(leftover.join("run.py"), "old").unwrap();
    fs::set_permissions(&leftover, fs::Permissions::from_mode(0o555)).unwrap();
    lab.drain();
    let (run, skipped) = start(&lab, one(SLUG), false).await;
    assert!(skipped.is_empty());
    let (done, samples) = rounds(&mut lab, &run).await;
    assert_eq!(done.len(), 1, "one round, no warm-up");
    let r = &done[0];
    assert_eq!(
        (
            r.item.clone(),
            r.side,
            r.round,
            r.end,
            r.passed,
            r.exit_code,
            r.sha.as_str()
        ),
        (
            item(ItemKind::Scenario, SLUG),
            RunSide::Pr,
            1,
            RoundEnd::Finished,
            Some(false),
            Some(1),
            "h1"
        )
    );
    assert_eq!(
        r.outputs,
        BTreeMap::from([("exchanges".to_string(), "1".to_string())])
    );
    assert_eq!(
        r.errors,
        [ErrorLine {
            text: "token expired".into(),
            count: 1,
            first_ms: r.errors[0].first_ms
        }]
    );
    assert!(r.metrics[&Metric::Duration] >= 600, "{:?}", r.metrics);
    assert_eq!(r.metrics.get(&Metric::PeakMemory), Some(&8192));
    assert!(r.metrics.contains_key(&Metric::AvgCpu));
    assert!(!samples.is_empty());
    for (side, round, points) in &samples {
        assert_eq!((*side, *round), (RunSide::Pr, 1));
        assert!(!points.is_empty() && points.len() <= 16);
    }
    assert_eq!(measured(&lab).len(), 1);
    assert!(
        lab.exec.group_calls() >= 1,
        "the sampler read the run's group"
    );
    let record = env::load(&lab.shared, &pr(7));
    let results = &record.cockpit_results[&item(ItemKind::Scenario, SLUG).key()];
    let single = results.single.as_ref().unwrap();
    assert_eq!(
        (single.side, single.rounds.len(), single.runner),
        (RunSide::Pr, 1, RunnerKind::Local)
    );
    assert!(!single.series.points.is_empty());
    assert!(results.compare.is_none());
    assert!(
        !leftover.exists(),
        "a leftover run folder goes at the next run's start"
    );
    assert_eq!(
        fs::read_dir(&runs).unwrap().count(),
        0,
        "the item's folder went with its run"
    );
    assert_eq!(scenario_view(&lab, SLUG).await.status, "failed");
}

#[tokio::test]
async fn a_scenarios_run_skips_the_unapproved_ones() {
    let mut lab = local_cockpit(vec![passing()]).await;
    put_scenario(&lab, "unapproved-one", RUN, &[("run.py", SCRIPT)]);
    let (run, skipped) = start(&lab, CockpitScope::Scenarios, false).await;
    assert_eq!(skipped, ["unapproved-one"]);
    let (done, _) = rounds(&mut lab, &run).await;
    assert_eq!(
        done.iter().map(|r| r.item.id.as_str()).collect::<Vec<_>>(),
        [SLUG]
    );
    assert_eq!(measured(&lab).len(), 1);
}

#[tokio::test]
async fn a_comparison_alternates_on_one_runner() {
    // Each run lasts past the sampler's first tick, so every round has samples.
    let slow = || FakeReply::after(Duration::from_millis(300), passing());
    let (mut lab, shas) = compared_lab(vec![slow()], vec![slow()]).await;
    lab.drain();
    let (run, _) = start(&lab, one(SLUG), true).await;
    let (done, samples) = rounds(&mut lab, &run).await;
    assert!(
        samples.iter().all(|(_, round, _)| *round >= 1),
        "a warm-up publishes no samples"
    );
    for side in [RunSide::Pr, RunSide::Main] {
        assert!(
            samples
                .iter()
                .any(|(s, _, points)| *s == side && !points.is_empty()),
            "{side:?}"
        );
    }
    assert_eq!(
        done.iter().map(|r| (r.side, r.round)).collect::<Vec<_>>(),
        [
            (RunSide::Pr, 1),
            (RunSide::Main, 1),
            (RunSide::Pr, 2),
            (RunSide::Main, 2),
            (RunSide::Pr, 3),
            (RunSide::Main, 3)
        ]
    );
    for r in &done {
        let sha = match r.side {
            RunSide::Pr => &shas.head,
            RunSide::Main => &shas.merge_base,
        };
        assert_eq!(&r.sha, sha);
    }
    let calls = measured(&lab);
    assert_eq!(
        calls.iter().map(|c| side_of(&lab, c)).collect::<Vec<_>>(),
        [
            RunSide::Pr,
            RunSide::Main,
            RunSide::Pr,
            RunSide::Main,
            RunSide::Pr,
            RunSide::Main,
            RunSide::Pr,
            RunSide::Main
        ],
        "a warm-up on each side, then alternated rounds"
    );
    let (pr_wt, main_wt) = (lab.worktree(7), main_worktree(&lab));
    let pr_tmp = format!(
        "{}/",
        fs::canonicalize(lab.shared.paths.test_tmp_for(&pr(7)))
            .unwrap()
            .display()
    );
    let main_tmp = format!(
        "{}/",
        fs::canonicalize(lab.shared.paths.test_main_tmp_for(&pr(7)))
            .unwrap()
            .display()
    );
    for call in &calls {
        let env = env_of(call);
        let (wt, tmp) = match side_of(&lab, call) {
            RunSide::Pr => (&pr_wt, &pr_tmp),
            RunSide::Main => (&main_wt, &main_tmp),
        };
        assert_eq!(call.cwd.as_ref(), Some(wt));
        assert_eq!(env["CLUSIA_ROOT"], wt.display().to_string());
        assert_eq!(&env["TMPDIR"], tmp, "each side has its own temp folder");
        assert!(
            env.keys().all(|k| !k.contains("SIDE")),
            "the side is never in the env"
        );
    }
    let record = env::load(&lab.shared, &pr(7));
    let compare = record.cockpit_results[&item(ItemKind::Scenario, SLUG).key()]
        .compare
        .clone()
        .unwrap();
    assert_eq!(
        (compare.pr.runner, compare.main.runner),
        (RunnerKind::Local, RunnerKind::Local)
    );
    assert_eq!((compare.pr.rounds.len(), compare.main.rounds.len()), (3, 3));
    assert_eq!(compare.outcome, CompareOutcome::Done);
    assert_eq!(compare.run, run);
    assert!(
        record.main.unwrap().built,
        "main's first finished round built it"
    );
    let shown = scenario_view(&lab, SLUG).await.comparison.unwrap();
    assert_eq!(
        shown.exercises,
        Some(false),
        "same result and outputs on both sides"
    );

    env::update_record(&lab.shared, &pr(7), |r| {
        if let Some(main) = r.main.as_mut() {
            main.runner = RunnerKind::Container;
        }
    })
    .await;
    let view = view_now(&lab).await;
    assert_eq!(
        refused(
            run_cockpit(
                &lab.shared,
                &pr(7),
                one(SLUG),
                true,
                seen_of(&view, &one(SLUG))
            )
            .await
        ),
        "Main was prepared for another runner; open it again"
    );
}

#[tokio::test]
async fn a_stopped_comparison_keeps_its_rounds_and_gives_no_verdict() {
    let (mut lab, _) = compared_lab(
        vec![passing(), passing(), FakeReply::hang()],
        vec![passing()],
    )
    .await;
    let (run, _) = start(&lab, one(SLUG), true).await;
    started_runs(&lab, 5).await;
    assert!(matches!(stop(&lab.shared, &pr(7)), Outcome::Ok(Reply::Ack)));
    let (done, _) = rounds(&mut lab, &run).await;
    assert_eq!(
        done.iter()
            .map(|r| (r.side, r.round, r.end))
            .collect::<Vec<_>>(),
        [
            (RunSide::Pr, 1, RoundEnd::Finished),
            (RunSide::Main, 1, RoundEnd::Finished),
            (RunSide::Pr, 2, RoundEnd::Stopped)
        ]
    );
    assert_eq!(measured(&lab).len(), 5, "nothing ran after the stop");
    let record = env::load(&lab.shared, &pr(7));
    let compare = record.cockpit_results[&item(ItemKind::Scenario, SLUG).key()]
        .compare
        .clone()
        .unwrap();
    assert_eq!(compare.outcome, CompareOutcome::Stopped);
    assert_eq!((compare.pr.rounds.len(), compare.main.rounds.len()), (2, 1));
    assert!(compare.pr.stopped && !compare.main.stopped);
    let shown = scenario_view(&lab, SLUG).await.comparison.unwrap();
    assert_eq!(shown.verdict, Verdict::Stopped);
    assert_eq!(shown.line, "Stopped: no verdict");
    assert_eq!(
        fs::read_dir(lab.shared.paths.test_runs_for(&pr(7)))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn main_results_from_an_older_base_are_left_out() {
    let (mut lab, _) = compared_lab(vec![passing()], vec![passing()]).await;
    let (run, _) = start(&lab, one(SLUG), true).await;
    rounds(&mut lab, &run).await;
    let shown = scenario_view(&lab, SLUG).await.comparison.unwrap();
    assert_eq!(
        (shown.pr.freshness, shown.main.freshness),
        (Freshness::Current, Freshness::Current)
    );
    // A rebase moved the merge base: main's rounds are from an older base now.
    env::update_record(&lab.shared, &pr(7), |r| {
        if let Some(main) = r.main.as_mut() {
            main.base = "1".repeat(40);
        }
    })
    .await;
    let shown = scenario_view(&lab, SLUG).await.comparison.unwrap();
    assert_eq!(shown.main.freshness, Freshness::OlderBase);
    assert_eq!(shown.verdict, Verdict::NotEnoughRounds);
    assert_eq!(shown.exercises, None);
    assert!(
        shown.rows.iter().all(|r| matches!(
            r.delta.label,
            DeltaLabel::NoVerdict | DeltaLabel::NotMeasured
        )),
        "{:#?}",
        shown.rows
    );
}

#[tokio::test]
async fn a_scenario_changed_on_disk_since_seen_is_refused() {
    let (lab, _) = compared_lab(vec![passing()], vec![passing()]).await;
    let seen = seen_of(&view_now(&lab).await, &one(SLUG));
    fs::write(
        lab.worktree(7)
            .join(".clusia/e2e")
            .join(SLUG)
            .join("run.py"),
        "import os; os.system('curl https://example.invalid')\n",
    )
    .unwrap();
    assert_eq!(
        refused(run_cockpit(&lab.shared, &pr(7), one(SLUG), true, seen).await),
        COCKPIT_CHANGED
    );
    let after = scenario_view(&lab, SLUG).await;
    assert_eq!(after.approval, ScenarioApproval::Changed);
    assert_eq!(
        refused(run_cockpit(&lab.shared, &pr(7), one(SLUG), true, after.hash.clone()).await),
        NOT_APPROVED
    );
    assert!(measured(&lab).is_empty());
    let runs = lab.shared.paths.test_runs_for(&pr(7));
    assert!(!runs.exists() || fs::read_dir(&runs).unwrap().count() == 0);
}

#[tokio::test]
async fn main_runs_the_approved_bytes_from_a_read_only_folder() {
    let (mut lab, _) = compared_lab(
        vec![passing()],
        vec![
            FakeReply::after(Duration::from_millis(500), passing()),
            passing(),
        ],
    )
    .await;
    let command = scenario_view(&lab, SLUG).await.command.unwrap();
    let (run, _) = start(&lab, one(SLUG), true).await;
    started_runs(&lab, 2).await;
    let calls = measured(&lab);
    let (pr_call, main_call) = (&calls[0], &calls[1]);
    assert_eq!(
        (side_of(&lab, pr_call), side_of(&lab, main_call)),
        (RunSide::Pr, RunSide::Main)
    );
    let dir = PathBuf::from(&env_of(main_call)["CLUSIA_SCENARIO_DIR"]);
    assert_eq!(
        env_of(pr_call)["CLUSIA_SCENARIO_DIR"],
        dir.display().to_string(),
        "one folder for both sides"
    );
    let runs = fs::canonicalize(lab.shared.paths.test_runs_for(&pr(7))).unwrap();
    assert!(dir.starts_with(&runs), "{}", dir.display());
    assert_eq!(fs::read_to_string(dir.join("run.py")).unwrap(), SCRIPT);
    assert_eq!(
        fs::metadata(dir.join("run.py"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o444
    );
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o555
    );
    let text = fs::read_to_string(profile(&lab.shared.paths, RunSide::Main)).unwrap();
    let writes =
        &text[text.find("(allow file-write*").unwrap()..text.find("(deny file-read*").unwrap()];
    assert!(!writes.contains(&dir.display().to_string()), "{writes}");
    assert!(text.contains(&format!("(subpath \"{}\")", dir.display())));
    assert!(writes.contains(&format!("(subpath \"{}\")", main_worktree(&lab).display())));
    assert_eq!(
        args_of(main_call)[2..],
        ["/bin/sh".to_string(), "-c".to_string(), command.clone()]
    );
    rounds(&mut lab, &run).await;
    assert!(!dir.exists(), "the run folder goes with the item's run");

    // In containers: the same folder, read-only at /clusia-scenario, on both sides.
    let (mut boxed, _) = container_compared().await;
    let (run, _) = start(&boxed, one(SLUG), true).await;
    rounds(&mut boxed, &run).await;
    let runs = container_runs(&boxed);
    assert_eq!(runs.len(), 8);
    let main_mount = format!("{}:/work", main_worktree(&boxed).display());
    let pr_mount = format!("{}:/work", boxed.worktree(7).display());
    let folders: Vec<&String> = runs
        .iter()
        .map(|a| {
            let at = a
                .windows(2)
                .position(|w| w[0] == "-v" && w[1].ends_with(":/clusia-scenario:ro"))
                .unwrap();
            &a[at + 1]
        })
        .collect();
    assert!(folders.iter().all(|f| *f == folders[0]), "{folders:?}");
    assert!(
        runs.iter().any(|a| a.contains(&main_mount)) && runs.iter().any(|a| a.contains(&pr_mount))
    );
    for args in &runs {
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "CLUSIA_SCENARIO_DIR=/clusia-scenario")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "CLUSIA_ROOT=/work")
        );
        assert_eq!(args.last().unwrap(), &command);
    }
}

#[tokio::test]
async fn values_are_absent_from_the_command_on_both_sides() {
    let hostile = "$(touch /tmp/pwned)'; rm -rf ~ #";
    let (mut lab, _) = container_compared().await;
    let input = ScenarioInput {
        values: BTreeMap::from([("text".to_string(), hostile.to_string())]),
        files: BTreeMap::new(),
        run: None,
        rounds: None,
        timeout_secs: None,
    };
    assert!(matches!(
        save_scenario(&lab.shared, &pr(7), SLUG, input).await,
        Outcome::Ok(Reply::Cockpit(_))
    ));
    let view = scenario_view(&lab, SLUG).await;
    assert_eq!(
        view.approval,
        ScenarioApproval::Approved,
        "a value needs no approval"
    );
    let command = view.command.unwrap();
    assert!(!command.contains("pwned"));
    let (run, _) = start(&lab, one(SLUG), true).await;
    rounds(&mut lab, &run).await;
    let runs = container_runs(&lab);
    assert_eq!(runs.len(), 8, "two warm-ups and three rounds a side");
    let main_mount = format!("{}:/work", main_worktree(&lab).display());
    assert_eq!(runs.iter().filter(|a| a.contains(&main_mount)).count(), 4);
    for args in &runs {
        let at: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| a.contains("pwned"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(at.len(), 1, "{args:?}");
        assert_eq!(args[at[0]], format!("CLUSIA_ARG_TEXT={hostile}"));
        assert_eq!(args[at[0] - 1], "-e");
        assert_eq!(args.last().unwrap(), &command);
    }
    let script = fs::read_to_string(
        lab.worktree(7)
            .join(".clusia/e2e")
            .join(SLUG)
            .join("run.py"),
    )
    .unwrap();
    assert!(!script.contains("pwned"), "the script holds no value");
}

#[tokio::test]
async fn nothing_is_sent_after_a_comparison_ends() {
    let slow = || FakeReply::after(Duration::from_millis(300), passing());
    let (mut lab, _) = compared_lab(vec![slow()], vec![slow()]).await;
    let (run, _) = start(&lab, one(SLUG), true).await;
    let (_, samples) = rounds(&mut lab, &run).await;
    assert!(!samples.is_empty(), "the sampler ran during the rounds");
    let (calls, reads) = (lab.exec.calls().len(), lab.exec.group_calls());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        lab.drain().is_empty(),
        "no event after the comparison ended"
    );
    assert_eq!(lab.exec.calls().len(), calls, "no program started after it");
    assert_eq!(lab.exec.group_calls(), reads, "no group read after it");
    assert!(idle(&lab.shared, 7));
}

/// Writes the benchmark of `TokenStore::refresh` as the benchmark turn leaves them: `bench.json` and the
/// Rust loop file. The function has no cases, so its params, and the values, are empty.
fn put_bench(lab: &Lab) -> String {
    let slug = function_slug("src/store.rs", "TokenStore::refresh");
    let dir = lab.worktree(7).join(".clusia/functions").join(&slug);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("bench.rs"), LOOP).unwrap();
    let stored = StoredBench {
        qualified: "TokenStore::refresh".into(),
        file: "src/store.rs".into(),
        lang: FunctionLang::Rust,
        // As the benchmark turn stores it: the signature at head, without its body's `{`.
        signature: super::cases::trim_signature(REFRESH_SIG, "rust"),
        values: BTreeMap::new(),
        warmup: 10,
        secs: 6,
        rounds: 5,
        bench_sha: sha256_hex(LOOP.as_bytes()),
        written_head: "h1".into(),
        written_at: 1_762_300_000,
    };
    fs::write(dir.join("bench.json"), serde_json::to_vec(&stored).unwrap()).unwrap();
    slug
}

#[tokio::test]
async fn a_benchmark_round_places_its_loop_file_and_reads_its_line() {
    let line = FakeReply::after(
        Duration::from_millis(300),
        FakeReply::marked(
            vec![(
                Stream::Out,
                "@MARK@ start\n@MARK@ bench 2000 6000000000 3000000 4000000 7000000\n@MARK@ done\n"
                    .into(),
            )],
            0,
        ),
    );
    let mut lab = local_cockpit(vec![
        line,
        FakeReply::marked(vec![(Stream::Out, "@MARK@ done\n".into())], 0),
    ])
    .await;
    let slug = put_bench(&lab);
    let target = clusia_testrun::bench::bench_target(&slug);
    let placed = lab.worktree(7).join("tests").join(format!("{target}.rs"));
    let scope = CockpitScope::Item {
        item: item(ItemKind::Bench, &slug),
    };
    let (run, _) = start(&lab, scope.clone(), false).await;
    started_runs(&lab, 1).await;
    assert_eq!(
        fs::read_to_string(&placed).unwrap(),
        LOOP,
        "placed while it runs"
    );
    assert_eq!(
        env::load(&lab.shared, &pr(7)).placed,
        [format!("tests/{target}.rs")]
    );
    let (done, _) = rounds(&mut lab, &run).await;
    let r = &done[0];
    assert_eq!((r.end, r.passed, r.round), (RoundEnd::Finished, None, 1));
    assert_eq!(r.metrics[&Metric::P50], 3_000_000);
    assert_eq!(r.metrics[&Metric::P95], 4_000_000);
    assert_eq!(r.metrics[&Metric::CallsPerSec], 333_333);
    let call = &measured(&lab)[0];
    assert_eq!(
        args_of(call)[2..],
        [
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "cargo test --offline -p 'widgets' --test '{target}' -- --exact clusia_case --nocapture"
            ),
        ]
    );
    let env = env_of(call);
    for (key, value) in [
        ("CLUSIA_CASE", "bench"),
        ("CLUSIA_BENCH_WARMUP", "10"),
        ("CLUSIA_BENCH_SECS", "6"),
        ("CLUSIA_BENCH_CALLS_MAX", "10000000"),
    ] {
        assert_eq!(env[key], value);
    }
    assert!(!placed.exists(), "the loop file goes after the run");
    assert!(env::load(&lab.shared, &pr(7)).placed.is_empty());

    let (run, _) = start(&lab, scope, false).await;
    let (done, _) = rounds(&mut lab, &run).await;
    assert_eq!(done[0].end, RoundEnd::Error);
    assert_eq!(
        done[0].message.as_deref(),
        Some("The benchmark ended without a result (exit 0)")
    );
    assert!(!done[0].metrics.contains_key(&Metric::P50));
}

#[tokio::test]
async fn forget_removes_the_run_folders() {
    let lab = super::cockpit_lab::cockpit_lab(vec![]);
    let folder = lab.shared.paths.test_runs_for(&pr(7)).join("1-1-0");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("run.py"), SCRIPT).unwrap();
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o555)).unwrap();
    forget(&lab.shared, &pr(7));
    assert!(!lab.shared.paths.test_runs_for(&pr(7)).exists());
}
