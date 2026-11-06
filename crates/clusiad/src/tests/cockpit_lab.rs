use std::time::Instant;

use clusia_core::cockpit::*;
use clusia_core::tests::TestsPhase;
use clusia_harness::testkit::{FakeClaude, Turn, bench_block, scenario_block, suggestion_block};
use clusia_protocol::{ErrorCode, Event, Outcome, Reply};
use serde_json::json;

use clusia_testrun::testkit::{FakeExec, FakeReply};

use super::functions_lab::{DRIVER, REFRESH_SIG, generate_answer, index, refresh, states};
use super::lab::{Lab, Machine, built, generated, idle, lab, pr, refused, settled, wt};
use super::*;
use clusia_core::Paths;

/// The agent's scenario block: two requests, one integer input and one script.
pub(super) fn scenario_json(title: &str) -> serde_json::Value {
    json!({
        "title": title,
        "purpose": "Two callers refresh an expired token together; only one exchange may happen.",
        "steps": ["Store an expired token", "Start two refreshes at once", "Count the exchanges"],
        "inputs": [
            {"name": "callers", "label": "Callers", "kind": "integer", "default": "2"},
            {"name": "mode", "label": "Mode", "kind": "one_of", "choices": ["fast", "slow"], "default": "fast"}
        ],
        "run": "python3 {script} --callers {callers}",
        "scripts": [{"name": "run.py", "text": "import os\nprint(os.environ['CLUSIA_ARG_CALLERS'])\n"}]
    })
}

/// A lab like `functions_lab`: review 7 with the index at `h1`, `machine`, the agent playing
/// `turns`; `script` adds FakeExec rules before the lab's own, so they win. The review is
/// not generated or set up yet.
pub(super) fn cockpit_lab_with(
    machine: Machine,
    turns: Vec<Turn>,
    script: impl FnOnce(&FakeExec, &Paths),
) -> Lab {
    let lab = lab(machine, turns, &[7], |x, paths| {
        script(x, paths);
        if machine != Machine::NoEngine {
            built(x);
            x.on("docker", &["run"], FakeReply::ok(""));
        }
    });
    index(&lab, 7, "h1", REFRESH_SIG);
    lab
}

pub(super) fn cockpit_lab(turns: Vec<Turn>) -> Lab {
    cockpit_lab_with(Machine::Colima { running: true }, turns, |_, _| {})
}

/// `cockpit_lab_with`, whose agent first answers the recipe turn (`generate_answer`) and whose
/// review 7 is then generated and set up: the plan is ready and the tests commands approved.
#[allow(dead_code)]
pub(super) async fn ready_lab_with(
    machine: Machine,
    turns: Vec<Turn>,
    script: impl FnOnce(&FakeExec, &Paths),
) -> Lab {
    let mut all = vec![generate_answer()];
    all.extend(turns);
    let lab = cockpit_lab_with(machine, all, script);
    super::lab::ready(&lab, 7).await;
    lab
}

pub(super) fn cockpit_of(out: Outcome) -> CockpitView {
    match out {
        Outcome::Ok(Reply::Cockpit(view)) => *view,
        other => panic!("not a cockpit view: {other:?}"),
    }
}

pub(super) async fn scenario_view(lab: &Lab, slug: &str) -> ScenarioView {
    let view = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    view.scenarios
        .iter()
        .find(|s| s.slug == slug)
        .cloned()
        .expect(slug)
}

/// Has the agent's turn write the scenario and gives the slug of `title`.
pub(super) async fn write_by_turn(lab: &mut Lab, title: &str) -> String {
    // Events told before this ask must not satisfy the wait below.
    lab.drain();
    assert!(matches!(
        write_scenarios(&lab.shared, &pr(7), ScenarioAsk::Fresh).await,
        Outcome::Ok(Reply::Ack)
    ));
    told_cockpit(lab, "the scenario is written", |v| {
        v.activity.is_none() && !v.scenarios.is_empty()
    })
    .await;
    scenario_slug(title, &Default::default())
}

/// The scenario `put_scenario` writes: a string input `text` (default "hello") and an integer
/// input `callers` (default "2"), 3 rounds, 120 s.
#[allow(dead_code)]
pub(super) fn stored_scenario(slug: &str, run: &str, files: &[(&str, &str)]) -> StoredScenario {
    StoredScenario {
        title: slug.to_string(),
        purpose: "A scenario for a test.".into(),
        steps: vec!["Do it".into()],
        inputs: vec![
            Input {
                name: "text".into(),
                label: "Text".into(),
                kind: InputKind::String,
                choices: Vec::new(),
                default: "hello".into(),
            },
            Input {
                name: "callers".into(),
                label: "Callers".into(),
                kind: InputKind::Integer,
                choices: Vec::new(),
                default: "2".into(),
            },
        ],
        run: run.to_string(),
        scripts: files.iter().map(|(name, _)| name.to_string()).collect(),
        rounds: 3,
        timeout_secs: 120,
        written_head: "h1".into(),
        written_at: 1_762_300_000,
    }
}

/// Writes `.clusia/e2e/<slug>/` straight to disk, as a turn would leave it: no agent runs.
#[allow(dead_code)]
pub(super) fn put_scenario_with(
    lab: &Lab,
    stored: &StoredScenario,
    slug: &str,
    files: &[(&str, &str)],
) {
    let dir = scenario_dir(lab, slug);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, text) in files {
        std::fs::write(dir.join(name), text).unwrap();
    }
    let toml = toml::to_string(&toml::Value::try_from(stored).unwrap()).unwrap();
    std::fs::write(dir.join("scenario.toml"), toml).unwrap();
}

#[allow(dead_code)]
pub(super) fn put_scenario(lab: &Lab, slug: &str, run: &str, files: &[(&str, &str)]) {
    put_scenario_with(lab, &stored_scenario(slug, run, files), slug, files);
}

pub(super) async fn approve_scenario_of(lab: &Lab, slug: &str) -> ScenarioView {
    let view = scenario_view(lab, slug).await;
    cockpit_of(approve_scenario(&lab.shared, &pr(7), slug, &view.approval_hash).await);
    scenario_view(lab, slug).await
}

/// The `CockpitChanged` views told so far, waiting until one matches `done`.
pub(super) async fn told_cockpit(
    lab: &mut Lab,
    what: &str,
    done: impl Fn(&CockpitView) -> bool,
) -> CockpitView {
    let give_up = Instant::now() + clusia_core::testkit::HANG_BOUND;
    loop {
        for event in lab.drain() {
            if let Event::CockpitChanged { view, .. } = event
                && done(&view)
            {
                return *view;
            }
        }
        assert!(Instant::now() < give_up, "{what}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
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

fn scenario_answer(titles: &[&str]) -> Turn {
    let blocks: Vec<String> = titles
        .iter()
        .map(|t| scenario_block(scenario_json(t)))
        .collect();
    Turn::answer_blocks("Here are the scenarios.", &blocks)
}

const TWO: &str = "Two requests refresh at once";
const TWO_SLUG: &str = "two-requests-refresh-at-once";

fn scenario_dir(lab: &Lab, slug: &str) -> std::path::PathBuf {
    lab.worktree(7).join(".clusia/e2e").join(slug)
}

#[tokio::test]
async fn an_empty_cockpit_says_nothing_and_a_closed_review_says_how_to_open_it() {
    let lab = cockpit_lab(Vec::new());
    let view = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert!(view.scenarios.is_empty() && view.benches.is_empty());
    assert_eq!(
        view.main.phase,
        MainPhase::Unavailable {
            reason: super::NO_RECIPE.into()
        },
        "main needs the pull request's recipe"
    );
    assert!(view.note.is_none());
    let closed = cockpit_of(cockpit(&lab.shared, &pr(99)).await);
    assert_eq!(
        closed.note.as_deref(),
        Some("Open the review to use the QA cockpit")
    );
    assert!(closed.scenarios.is_empty());
}

#[tokio::test]
async fn write_scenarios_lists_new_scenarios_with_a_rewritten_command() {
    let mut lab = cockpit_lab(vec![scenario_answer(&[TWO, "Slow endpoint"])]);
    assert!(matches!(
        write_scenarios(&lab.shared, &pr(7), ScenarioAsk::Fresh).await,
        Outcome::Ok(Reply::Ack)
    ));
    let writing = told_cockpit(&mut lab, "writing", |v| {
        v.activity
            == Some(CockpitActivity::Writing {
                what: "scenarios".into(),
            })
    })
    .await;
    assert!(writing.scenarios.is_empty());
    let done = told_cockpit(&mut lab, "written", |v| v.activity.is_none()).await;
    let slugs: Vec<&str> = done.scenarios.iter().map(|s| s.slug.as_str()).collect();
    assert_eq!(slugs, ["slow-endpoint", TWO_SLUG]);
    let s = done.scenarios.iter().find(|s| s.slug == TWO_SLUG).unwrap();
    assert_eq!(s.title, TWO);
    assert_eq!(s.run, "python3 {script} --callers {callers}");
    assert_eq!(
        s.command.as_deref(),
        Some("python3 \"$CLUSIA_SCENARIO_DIR/run.py\" --callers \"$CLUSIA_ARG_CALLERS\"")
    );
    assert_eq!(s.inputs[0].value, "2");
    assert_eq!(s.inputs[1].value, "fast");
    assert_eq!(s.scripts[0].name, "run.py");
    assert_eq!((s.rounds, s.timeout_secs), (3, 120));
    assert_eq!(s.approval, ScenarioApproval::NotApproved);
    assert_eq!(s.support, ItemSupport::NeedsApproval);
    assert_eq!(s.status, "not approved");
    assert_eq!(done.hash, scenarios_hash(&done.scenarios));
    assert!(done.failed.is_none());
    assert!(scenario_dir(&lab, TWO_SLUG).join("scenario.toml").is_file());
    assert_eq!(
        std::fs::read_to_string(scenario_dir(&lab, TWO_SLUG).join("run.py")).unwrap(),
        "import os\nprint(os.environ['CLUSIA_ARG_CALLERS'])\n"
    );
    assert_eq!(states(&lab), ["waiting", "running", "done"]);
}

#[tokio::test]
async fn an_edit_of_the_script_or_command_needs_a_new_approval() {
    let mut lab = cockpit_lab(vec![scenario_answer(&[TWO])]);
    let slug = write_by_turn(&mut lab, TWO).await;
    let approved = approve_scenario_of(&lab, &slug).await;
    assert_eq!(approved.approval, ScenarioApproval::Approved);
    assert_eq!(approved.support, ItemSupport::Ready);
    assert_eq!(approved.status, "not run");

    // A value edit never asks again, and moves only the input hash.
    let input =
        |values: &[(&str, &str)], files: &[(&str, &str)], run: Option<&str>| ScenarioInput {
            values: values
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            files: files
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            run: run.map(String::from),
            rounds: None,
            timeout_secs: None,
        };
    let valued = cockpit_of(
        save_scenario(
            &lab.shared,
            &pr(7),
            &slug,
            input(&[("callers", "8")], &[], None),
        )
        .await,
    );
    let s = &valued.scenarios[0];
    assert_eq!(s.approval, ScenarioApproval::Approved);
    assert_eq!(s.approval_hash, approved.approval_hash);
    assert_ne!(s.input_hash, approved.input_hash);
    assert!(s.edited && s.inputs[0].edited && s.inputs[0].value == "8");

    // A script edit changes the approval hash: the stored approval is Changed.
    let scripted = cockpit_of(
        save_scenario(
            &lab.shared,
            &pr(7),
            &slug,
            input(&[("callers", "8")], &[("run.py", "print('other')\n")], None),
        )
        .await,
    );
    let s = &scripted.scenarios[0];
    assert_eq!(s.approval, ScenarioApproval::Changed);
    assert_ne!(s.approval_hash, approved.approval_hash);
    assert_eq!(s.support, ItemSupport::NeedsApproval);
    assert_eq!(s.status, "approve again: the script changed");
    assert!(s.scripts[0].edited);
    let old_hash = approved.approval_hash.clone();
    assert_eq!(
        refused(approve_scenario(&lab.shared, &pr(7), &slug, &old_hash).await),
        "The scenario or benchmark changed since you saw it; look at it again"
    );

    // Editing the script back clears the edit and the approval is current again.
    let back = cockpit_of(
        save_scenario(
            &lab.shared,
            &pr(7),
            &slug,
            input(&[("callers", "8")], &[], None),
        )
        .await,
    );
    assert_eq!(back.scenarios[0].approval, ScenarioApproval::Approved);
    assert!(!back.scenarios[0].scripts[0].edited);

    // A command edit changes the approval hash too.
    let commanded = cockpit_of(
        save_scenario(
            &lab.shared,
            &pr(7),
            &slug,
            input(&[], &[], Some("python3 {script}")),
        )
        .await,
    );
    let s = &commanded.scenarios[0];
    assert_eq!(s.approval, ScenarioApproval::Changed);
    assert_eq!(
        s.command.as_deref(),
        Some("python3 \"$CLUSIA_SCENARIO_DIR/run.py\"")
    );
}

#[tokio::test]
async fn a_script_changed_on_disk_asks_again_and_a_hostile_file_is_unsupported() {
    let mut lab = cockpit_lab(vec![scenario_answer(&[TWO])]);
    let slug = write_by_turn(&mut lab, TWO).await;
    let approved = approve_scenario_of(&lab, &slug).await;
    std::fs::write(
        scenario_dir(&lab, &slug).join("run.py"),
        "print('PR code wrote this')\n",
    )
    .unwrap();
    let after = scenario_view(&lab, &slug).await;
    assert_eq!(after.approval, ScenarioApproval::Changed);
    assert_ne!(after.approval_hash, approved.approval_hash);
    // A script that grew past its cap, or a command with an unknown placeholder, is no scenario.
    std::fs::write(
        scenario_dir(&lab, &slug).join("run.py"),
        "x".repeat(40 * 1024),
    )
    .unwrap();
    let big = scenario_view(&lab, &slug).await;
    assert!(
        matches!(big.support, ItemSupport::Unsupported { .. }),
        "{:?}",
        big.support
    );
    std::fs::write(scenario_dir(&lab, &slug).join("run.py"), "print(1)\n").unwrap();
    let toml = std::fs::read_to_string(scenario_dir(&lab, &slug).join("scenario.toml")).unwrap();
    std::fs::write(
        scenario_dir(&lab, &slug).join("scenario.toml"),
        toml.replace("{callers}", "{nowhere}"),
    )
    .unwrap();
    let unknown = scenario_view(&lab, &slug).await;
    match unknown.support {
        ItemSupport::Unsupported { reason } => {
            assert!(
                reason.contains("The command uses {nowhere}, which is not an input"),
                "{reason}"
            )
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(unknown.command, None);
}

#[tokio::test]
async fn save_reset_and_remove_follow_the_scenario_rules() {
    let mut lab = cockpit_lab(vec![scenario_answer(&[TWO])]);
    let slug = write_by_turn(&mut lab, TWO).await;
    let save = |lab: &Lab, input: ScenarioInput| {
        let shared = lab.shared.clone();
        let slug = slug.clone();
        async move { save_scenario(&shared, &pr(7), &slug, input).await }
    };
    let empty = || ScenarioInput {
        values: Default::default(),
        files: Default::default(),
        run: None,
        rounds: None,
        timeout_secs: None,
    };
    let with = |key: &str, value: &str| {
        let mut input = empty();
        input.values.insert(key.into(), value.into());
        input
    };
    assert_eq!(
        bad(save(&lab, with("callers", "two")).await),
        "Enter a whole number"
    );
    assert_eq!(
        bad(save(&lab, with("mode", "medium")).await),
        "Choose one of: fast, slow"
    );
    assert_eq!(
        bad(save(&lab, with("nowhere", "1")).await),
        "No input nowhere in this scenario"
    );
    let mut file = empty();
    file.files.insert("other.py".into(), "x".into());
    assert_eq!(
        bad(save(&lab, file).await),
        "No script other.py in this scenario"
    );
    let mut rounds = empty();
    rounds.rounds = Some(11);
    assert_eq!(
        bad(save(&lab, rounds).await),
        "Rounds must be between 1 and 10"
    );
    let mut timeout = empty();
    timeout.timeout_secs = Some(4);
    assert_eq!(
        bad(save(&lab, timeout).await),
        "The timeout must be between 5 and 600 seconds"
    );
    let mut run = empty();
    run.run = Some("python3 {script} {nowhere}".into());
    assert!(
        bad(save(&lab, run).await).contains("The command uses {nowhere}, which is not an input")
    );
    assert!(!scenario_dir(&lab, &slug).join("edits.json").exists());

    // A value equal to the default drops its key, and an empty overlay removes the file.
    cockpit_of(save(&lab, with("callers", "5")).await);
    assert!(scenario_dir(&lab, &slug).join("edits.json").is_file());
    let same = cockpit_of(save(&lab, with("callers", "2")).await);
    assert!(!scenario_dir(&lab, &slug).join("edits.json").exists());
    assert!(!same.scenarios[0].edited);

    cockpit_of(save(&lab, with("callers", "9")).await);
    let reset = cockpit_of(reset_scenario(&lab.shared, &pr(7), &slug).await);
    assert!(!reset.scenarios[0].edited);
    assert_eq!(reset.scenarios[0].inputs[0].value, "2");

    // Removing the folder removes its approval and its results.
    approve_scenario_of(&lab, &slug).await;
    env::update_record(&lab.shared, &pr(7), |record| {
        record
            .cockpit_results
            .insert(format!("scenario:{slug}"), ItemResults::default());
    })
    .await;
    let removed = cockpit_of(remove_scenario(&lab.shared, &pr(7), &slug).await);
    assert!(removed.scenarios.is_empty());
    assert!(!scenario_dir(&lab, &slug).exists());
    let record = env::load(&lab.shared, &pr(7));
    assert!(record.scenario_approvals.is_empty());
    assert!(record.cockpit_results.is_empty());
    assert_eq!(
        refused(remove_scenario(&lab.shared, &pr(7), &slug).await),
        format!("No scenario {slug} in acme/widgets#7")
    );
}

#[tokio::test]
async fn invalid_scenario_and_function_ids_are_bad_requests() {
    let lab = cockpit_lab(Vec::new());
    for id in ["", "..", "a/b", "Two", "-x", "x-", "a_b", &"a".repeat(80)] {
        for out in [
            save_scenario(
                &lab.shared,
                &pr(7),
                id,
                ScenarioInput {
                    values: Default::default(),
                    files: Default::default(),
                    run: None,
                    rounds: None,
                    timeout_secs: None,
                },
            )
            .await,
            reset_scenario(&lab.shared, &pr(7), id).await,
            remove_scenario(&lab.shared, &pr(7), id).await,
            approve_scenario(&lab.shared, &pr(7), id, "h").await,
            scenario_script(&lab.shared, &pr(7), id).await,
        ] {
            assert_eq!(bad(out), "invalid scenario id", "{id:?}");
        }
    }
    assert_eq!(
        bad(reset_bench(&lab.shared, &pr(7), "../x").await),
        "invalid function id"
    );
}

#[tokio::test]
async fn a_symlinked_scenario_folder_is_unsupported_and_refuses_writes() {
    let mut lab = cockpit_lab(vec![scenario_answer(&[TWO])]);
    let slug = write_by_turn(&mut lab, TWO).await;
    let elsewhere = lab.home.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::rename(scenario_dir(&lab, &slug), elsewhere.join("kept")).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("kept"), scenario_dir(&lab, &slug)).unwrap();
    let view = scenario_view(&lab, &slug).await;
    assert_eq!(
        view.support,
        ItemSupport::Unsupported {
            reason: "Clúsia's folder for this scenario is not a plain folder".into()
        }
    );
    let input = ScenarioInput {
        values: [("callers".to_string(), "4".to_string())].into(),
        files: Default::default(),
        run: None,
        rounds: None,
        timeout_secs: None,
    };
    assert_eq!(
        refused(save_scenario(&lab.shared, &pr(7), &slug, input).await),
        "Clúsia's folder for this scenario is not a plain folder"
    );
    assert!(
        !elsewhere.join("kept/edits.json").exists(),
        "nothing is written through the link"
    );
    // A link in place of `e2e` itself empties the list and refuses every write.
    std::fs::remove_file(scenario_dir(&lab, &slug)).unwrap();
    std::fs::remove_dir_all(lab.worktree(7).join(".clusia/e2e")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, lab.worktree(7).join(".clusia/e2e")).unwrap();
    let view = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert!(view.scenarios.is_empty());
    assert_eq!(
        view.note.as_deref(),
        Some("Clúsia's folder for scenarios is not a plain folder")
    );
}

#[tokio::test]
async fn a_shipped_clusia_folder_empties_the_cockpit_and_refuses_every_write() {
    let lab = cockpit_lab_with(Machine::NoEngine, Vec::new(), |x, paths| {
        x.on(
            "git",
            &["-C", wt(paths, 7).as_str()],
            FakeReply::ok(".clusia/e2e/x/scenario.toml\0"),
        );
    });
    let shipped = clusia_testrun::scan::SHIPPED_CLUSIA;
    let view = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert!(view.scenarios.is_empty());
    assert_eq!(view.note.as_deref(), Some(shipped));
    assert_eq!(
        refused(write_scenarios(&lab.shared, &pr(7), ScenarioAsk::Fresh).await),
        shipped
    );
    assert_eq!(
        refused(approve_scenario(&lab.shared, &pr(7), "x", "h").await),
        shipped
    );
    assert_eq!(
        refused(ask_copilot(&lab.shared, &pr(7), None, CopilotAsk::Next).await),
        shipped
    );
}

#[tokio::test]
async fn an_invalid_scenario_answer_changes_nothing_and_says_why() {
    let mut bad_block = scenario_json("Broken");
    bad_block["inputs"][0]["name"] = json!("Callers");
    let mut lab = cockpit_lab(vec![Turn::answer_blocks(
        "One.",
        &[scenario_block(bad_block)],
    )]);
    assert!(matches!(
        write_scenarios(&lab.shared, &pr(7), ScenarioAsk::Fresh).await,
        Outcome::Ok(Reply::Ack)
    ));
    let done = told_cockpit(&mut lab, "failed", |v| {
        v.activity.is_none() && v.failed.is_some()
    })
    .await;
    assert!(done.scenarios.is_empty());
    assert!(!lab.worktree(7).join(".clusia/e2e/broken").exists());
    let failed = done.failed.unwrap();
    assert!(
        failed.starts_with("Claude Code's scenario is not valid: "),
        "{failed}"
    );
    assert!(failed.len() < 400);
    // A command with a placeholder that is not an input is invalid, too.
    let mut unknown = scenario_json("Unknown placeholder");
    unknown["run"] = json!("python3 {script} {nowhere}");
    let mut lab = cockpit_lab(vec![Turn::answer_blocks(
        "One.",
        &[scenario_block(unknown)],
    )]);
    write_scenarios(&lab.shared, &pr(7), ScenarioAsk::Fresh).await;
    let done = told_cockpit(&mut lab, "failed", |v| {
        v.activity.is_none() && v.failed.is_some()
    })
    .await;
    assert!(done.failed.unwrap().contains("which is not an input"));
    assert!(done.scenarios.is_empty());
    // No block at all.
    let mut lab = cockpit_lab(vec![Turn::answer("No block here.")]);
    write_scenarios(&lab.shared, &pr(7), ScenarioAsk::Fresh).await;
    let done = told_cockpit(&mut lab, "failed", |v| {
        v.activity.is_none() && v.failed.is_some()
    })
    .await;
    assert_eq!(
        done.failed.as_deref(),
        Some("Claude Code's answer holds no clusia-scenario block")
    );
}

#[tokio::test]
async fn fresh_adds_at_most_three_more_keeps_the_old_and_a_taken_slug_gets_a_number() {
    let first = ["One", "Two", "Three", "Four"];
    let mut lab = cockpit_lab(vec![
        scenario_answer(&first),
        scenario_answer(&["One", "Five"]),
    ]);
    write_scenarios(&lab.shared, &pr(7), ScenarioAsk::Fresh).await;
    let done = told_cockpit(&mut lab, "first", |v| {
        v.activity.is_none() && !v.scenarios.is_empty()
    })
    .await;
    let slugs: Vec<&str> = done.scenarios.iter().map(|s| s.slug.as_str()).collect();
    assert_eq!(
        slugs,
        ["one", "three", "two"],
        "the fourth block is dropped"
    );
    write_scenarios(&lab.shared, &pr(7), ScenarioAsk::More).await;
    let more = told_cockpit(&mut lab, "more", |v| {
        v.activity.is_none() && v.scenarios.len() == 5
    })
    .await;
    let slugs: Vec<&str> = more.scenarios.iter().map(|s| s.slug.as_str()).collect();
    assert_eq!(slugs, ["five", "one", "one-2", "three", "two"]);
}

#[tokio::test]
async fn a_new_scenario_never_inherits_the_approval_or_results_of_a_removed_one() {
    let mut lab = cockpit_lab(vec![scenario_answer(&[TWO]), scenario_answer(&[TWO])]);
    let slug = write_by_turn(&mut lab, TWO).await;
    approve_scenario_of(&lab, &slug).await;
    std::fs::remove_dir_all(scenario_dir(&lab, &slug)).unwrap();
    let again = write_by_turn(&mut lab, TWO).await;
    assert_eq!(again, slug);
    let view = scenario_view(&lab, &slug).await;
    assert_eq!(
        view.approval,
        ScenarioApproval::NotApproved,
        "the old approval went with the folder"
    );
}

#[tokio::test]
async fn describe_and_values_asks_write_one_scenario_each() {
    let values = json!({
        "title": "Ignored title", "purpose": "p", "steps": ["s"],
        "inputs": [
            {"name": "callers", "label": "x", "kind": "string", "default": "7"},
            {"name": "mode", "label": "x", "kind": "string", "default": "slow"}
        ],
        "run": "ignored", "scripts": [{"name": "x.sh", "text": "echo\n"}]
    });
    let mut lab = cockpit_lab(vec![
        scenario_answer(&[TWO]),
        Turn::answer_blocks("Values.", &[scenario_block(values)]),
        scenario_answer(&["Token endpoint is down"]),
    ]);
    let slug = write_by_turn(&mut lab, TWO).await;
    let one = |v: &str| ScenarioInput {
        values: [("callers".to_string(), v.to_string())].into(),
        files: [("run.py".to_string(), "print('edited')\n".to_string())].into(),
        run: None,
        rounds: None,
        timeout_secs: None,
    };
    save_scenario(&lab.shared, &pr(7), &slug, one("9")).await;
    write_scenarios(
        &lab.shared,
        &pr(7),
        ScenarioAsk::Values {
            scenario: slug.clone(),
        },
    )
    .await;
    told_cockpit(&mut lab, "values written", |v| {
        v.activity.is_none() && v.scenarios[0].inputs[0].value == "7"
    })
    .await;
    let view = scenario_view(&lab, &slug).await;
    assert_eq!(view.title, TWO, "only the defaults are taken");
    assert_eq!(view.inputs[1].value, "slow");
    assert!(!view.inputs[0].edited, "the values overlay is dropped");
    assert!(view.scripts[0].edited, "the other overlays stay");
    let edits = std::fs::read_to_string(scenario_dir(&lab, &slug).join("edits.json")).unwrap();
    assert!(!edits.contains("\"callers\""), "{edits}");

    write_scenarios(
        &lab.shared,
        &pr(7),
        ScenarioAsk::Describe {
            text: "Token endpoint is down: it answers 500".into(),
        },
    )
    .await;
    told_cockpit(&mut lab, "described", |v| {
        v.activity.is_none() && v.scenarios.len() == 2
    })
    .await;
    let calls = FakeClaude::calls(lab.fake.path());
    let argv = &calls[2].argv;
    let prompt = &argv[argv.iter().position(|a| a == "-p").unwrap() + 1];
    assert!(
        prompt.contains(
            "Write one scenario for this idea (data): Token endpoint is down: it answers 500"
        ),
        "{prompt}"
    );
    let long = "x".repeat(601);
    assert_eq!(
        bad(write_scenarios(&lab.shared, &pr(7), ScenarioAsk::Describe { text: long }).await),
        "Describe the scenario in one line of at most 600 characters"
    );
    assert_eq!(
        bad(write_scenarios(
            &lab.shared,
            &pr(7),
            ScenarioAsk::Describe {
                text: "a\nb".into()
            }
        )
        .await),
        "Describe the scenario in one line of at most 600 characters"
    );
}

#[tokio::test]
async fn writing_scenarios_takes_the_review_slot_and_a_stop_ends_it_with_no_failure() {
    let mut lab = cockpit_lab(vec![Turn::hanging()]);
    assert!(matches!(
        write_scenarios(&lab.shared, &pr(7), ScenarioAsk::Fresh).await,
        Outcome::Ok(Reply::Ack)
    ));
    let busy = "The tests of acme/widgets#7 are busy writing scenarios; wait for it or stop it";
    assert_eq!(
        refused(write_scenarios(&lab.shared, &pr(7), ScenarioAsk::Fresh).await),
        busy
    );
    assert_eq!(refused(super::generate(&lab.shared, &pr(7)).await), busy);
    assert_eq!(
        env::view(&lab.shared, &pr(7)).await.phase,
        TestsPhase::NotGenerated
    );
    assert!(matches!(stop(&lab.shared, &pr(7)), Outcome::Ok(Reply::Ack)));
    let ended = told_cockpit(&mut lab, "stopped", |v| v.activity.is_none()).await;
    assert!(ended.failed.is_none(), "a stop is no failure");
    settled(&lab, 7).await;
}

#[tokio::test]
async fn the_scenario_turn_is_forked_and_read_only_with_the_context() {
    let mut lab = cockpit_lab(vec![scenario_answer(&[TWO])]);
    write_by_turn(&mut lab, TWO).await;
    let calls = FakeClaude::calls(lab.fake.path());
    let argv = &calls[0].argv;
    let prompt = &argv[argv.iter().position(|a| a == "-p").unwrap() + 1];
    assert!(
        prompt.starts_with("Write end-to-end scenarios for this pull request"),
        "{prompt}"
    );
    assert!(prompt.contains("CLUSIA_SCENARIO_DIR"));
    assert!(prompt.contains("Scenarios run in a container, with no network."));
    assert!(prompt.contains("Refresh tokens"));
    assert!(
        argv.iter().any(|a| a == "--fork-session") || argv.iter().any(|a| a == "--session-id"),
        "{argv:?}"
    );
}

#[tokio::test]
async fn write_bench_writes_the_loop_file_and_the_json_and_refuses_without_a_command() {
    let bench = json!({
        "function": "TokenStore::refresh", "file": "src/store.rs",
        "values": {"stored_token": "old", "endpoint_status": "200"},
        "warmup": 5, "secs": 4,
        "bench": "#[test]\nfn clusia_case() {}\n"
    });
    let mut lab = cockpit_lab(vec![
        generate_answer(),
        Turn::answer_blocks("The bench.", &[bench_block(bench)]),
    ]);
    generated(&lab, 7).await;
    assert_eq!(
        refused(write_bench(&lab.shared, &pr(7), "nowhere-1234567890", BenchAsk::Values).await),
        "No changed function nowhere-1234567890 in acme/widgets#7"
    );
    assert_eq!(
        refused(write_bench(&lab.shared, &pr(7), &refresh(), BenchAsk::Values).await),
        "There is no benchmark for TokenStore::refresh yet"
    );
    assert!(matches!(
        write_bench(&lab.shared, &pr(7), &refresh(), BenchAsk::Fresh).await,
        Outcome::Ok(Reply::Ack)
    ));
    let writing = told_cockpit(&mut lab, "writing", |v| {
        v.activity
            == Some(CockpitActivity::Writing {
                what: format!("bench:{}", refresh()),
            })
    })
    .await;
    assert!(writing.benches.is_empty());
    let done = told_cockpit(&mut lab, "written", |v| {
        v.activity.is_none() && !v.benches.is_empty()
    })
    .await;
    let b = &done.benches[0];
    assert_eq!(
        (b.slug.as_str(), b.qualified.as_str()),
        (refresh().as_str(), "TokenStore::refresh")
    );
    assert_eq!((b.warmup, b.secs, b.rounds), (5, 4, 5));
    assert_eq!(b.values["stored_token"], "old");
    assert_eq!(b.path, format!(".clusia/functions/{}/bench.rs", refresh()));
    assert_eq!(b.support, ItemSupport::Ready);
    assert_eq!(b.status, "not run");
    let folder = lab.worktree(7).join(".clusia/functions").join(refresh());
    assert_eq!(
        std::fs::read_to_string(folder.join("bench.rs")).unwrap(),
        "#[test]\nfn clusia_case() {}\n"
    );
    let stored: StoredBench =
        serde_json::from_slice(&std::fs::read(folder.join("bench.json")).unwrap()).unwrap();
    assert_eq!(
        stored.bench_sha,
        script_sha(b"#[test]\nfn clusia_case() {}\n")
    );
    // The cases and their driver were never touched.
    assert!(folder.join("function.json").is_file());
    assert_eq!(
        std::fs::read_to_string(folder.join("driver.rs")).unwrap(),
        DRIVER
    );
    assert_eq!(done.hash, scenarios_hash(&done.scenarios));
}

#[tokio::test]
async fn a_benchmark_goes_stale_with_its_signature_or_its_file_and_edits_are_checked() {
    let bench = json!({
        "function": "TokenStore::refresh", "file": "src/store.rs",
        "values": {"stored_token": "old", "endpoint_status": "200"},
        "bench": "#[test]\nfn clusia_case() {}\n"
    });
    let mut lab = cockpit_lab(vec![
        generate_answer(),
        Turn::answer_blocks("b", &[bench_block(bench)]),
    ]);
    generated(&lab, 7).await;
    write_bench(&lab.shared, &pr(7), &refresh(), BenchAsk::Fresh).await;
    told_cockpit(&mut lab, "written", |v| {
        v.activity.is_none() && !v.benches.is_empty()
    })
    .await;
    let input = |warmup: Option<u32>,
                 secs: Option<u32>,
                 rounds: Option<u32>,
                 status: Option<&str>| BenchInput {
        values: status
            .map(|s| [("endpoint_status".to_string(), s.to_string())].into())
            .unwrap_or_default(),
        warmup,
        secs,
        rounds,
    };
    assert_eq!(
        bad(save_bench(
            &lab.shared,
            &pr(7),
            &refresh(),
            input(Some(1001), None, None, None)
        )
        .await),
        "Warm-up calls must be between 0 and 1000"
    );
    assert_eq!(
        bad(save_bench(
            &lab.shared,
            &pr(7),
            &refresh(),
            input(None, Some(31), None, None)
        )
        .await),
        "Seconds must be between 1 and 30"
    );
    assert_eq!(
        bad(save_bench(
            &lab.shared,
            &pr(7),
            &refresh(),
            input(None, None, Some(0), None)
        )
        .await),
        "Rounds must be between 1 and 10"
    );
    assert!(
        bad(save_bench(
            &lab.shared,
            &pr(7),
            &refresh(),
            input(None, None, None, Some("abc"))
        )
        .await)
        .contains("whole number")
    );
    let before = cockpit_of(cockpit(&lab.shared, &pr(7)).await).benches[0]
        .input_hash
        .clone();
    let saved = cockpit_of(
        save_bench(
            &lab.shared,
            &pr(7),
            &refresh(),
            input(Some(20), Some(8), Some(7), Some("500")),
        )
        .await,
    );
    let b = &saved.benches[0];
    assert_eq!((b.warmup, b.secs, b.rounds), (20, 8, 7));
    assert_eq!(b.values["endpoint_status"], "500");
    assert!(b.edited);
    assert_ne!(b.input_hash, before);
    let reset = cockpit_of(reset_bench(&lab.shared, &pr(7), &refresh()).await);
    assert!(!reset.benches[0].edited);
    assert_eq!(reset.benches[0].input_hash, before);

    // The loop file edited on disk, or gone, makes the benchmark Stale.
    let file = lab
        .worktree(7)
        .join(".clusia/functions")
        .join(refresh())
        .join("bench.rs");
    std::fs::write(&file, "fn changed() {}\n").unwrap();
    let stale = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert_eq!(
        stale.benches[0].support,
        ItemSupport::Stale {
            reason: "The benchmark file changed on disk".into()
        }
    );
    std::fs::remove_file(&file).unwrap();
    let gone = cockpit_of(cockpit(&lab.shared, &pr(7)).await);
    assert_eq!(
        gone.benches[0].support,
        ItemSupport::Stale {
            reason: "The benchmark file is missing".into()
        }
    );
}

#[tokio::test]
async fn a_copilot_answer_is_plain_text_with_at_most_three_suggestions() {
    let sug = |kind: &str, title: &str, function: Option<&str>| {
        let mut value = json!({"kind": kind, "title": title, "why": "It covers the slow path."});
        if let Some(f) = function {
            value["function"] = json!(f);
        }
        suggestion_block(value)
    };
    let answer = Turn::answer_blocks(
        &format!(
            "Memory grows on this PR.\u{202e}\u{1b}[31m See [x](http://evil) text\tkept\n{}",
            "y".repeat(5000)
        ),
        &[
            sug("scenario", "Slow endpoint", None),
            sug("bench", "Bench refresh", Some("TokenStore::refresh")),
            sug("bench", "Bench nowhere", Some("nowhere")),
            sug("scenario", "Fourth", None),
            sug("scenario", "Fifth", None),
        ],
    );
    let mut lab = cockpit_lab(vec![generate_answer(), answer]);
    generated(&lab, 7).await;
    assert!(matches!(
        ask_copilot(&lab.shared, &pr(7), None, CopilotAsk::Next).await,
        Outcome::Ok(Reply::Ack)
    ));
    let asked = told_cockpit(&mut lab, "asked", |v| v.copilot.busy).await;
    assert_eq!(asked.copilot.messages.len(), 1);
    assert_eq!(asked.copilot.messages[0].text, "What should I try next?");
    assert_eq!(asked.copilot.messages[0].from, CopilotFrom::Reviewer);
    let done = told_cockpit(&mut lab, "answered", |v| {
        !v.copilot.busy && v.copilot.messages.len() == 2
    })
    .await;
    let message = &done.copilot.messages[1];
    assert_eq!(message.from, CopilotFrom::Agent);
    assert_eq!(message.id, 2);
    assert!(message.text.chars().count() <= 4000);
    assert!(message.text.starts_with("Memory grows on this PR."));
    assert!(!message.text.contains('\u{202e}') && !message.text.contains('\u{1b}'));
    assert!(message.text.contains("text\tkept"));
    assert!(!message.text.contains("clusia-suggestion"));
    let titles: Vec<&str> = message
        .suggestions
        .iter()
        .map(|s| s.title.as_str())
        .collect();
    assert_eq!(
        titles,
        ["Slow endpoint", "Bench refresh"],
        "a bench for an unlisted function is dropped"
    );
    assert_eq!(
        message.suggestions[1].function.as_deref(),
        Some("TokenStore::refresh")
    );
    assert_eq!(env::load(&lab.shared, &pr(7)).copilot.len(), 2);
}

#[tokio::test]
async fn at_most_three_valid_suggestions_are_kept() {
    let blocks: Vec<String> = ["A", "B", "C", "D"]
        .iter()
        .map(|t| suggestion_block(json!({"kind": "scenario", "title": t, "why": "w"})))
        .collect();
    let mut lab = cockpit_lab(vec![Turn::answer_blocks("Try these.", &blocks)]);
    ask_copilot(
        &lab.shared,
        &pr(7),
        None,
        CopilotAsk::Free {
            text: "what next?".into(),
        },
    )
    .await;
    let done = told_cockpit(&mut lab, "answered", |v| {
        !v.copilot.busy && v.copilot.messages.len() == 2
    })
    .await;
    assert_eq!(done.copilot.messages[0].text, "what next?");
    assert_eq!(done.copilot.messages[1].suggestions.len(), 3);
}

#[tokio::test]
async fn the_copilot_does_not_take_the_slot_and_a_second_ask_is_refused() {
    let mut lab = cockpit_lab(vec![Turn::hanging(), scenario_answer(&[TWO])]);
    assert!(matches!(
        ask_copilot(&lab.shared, &pr(7), None, CopilotAsk::Next).await,
        Outcome::Ok(Reply::Ack)
    ));
    told_cockpit(&mut lab, "busy", |v| v.copilot.busy).await;
    assert!(idle(&lab.shared, 7), "the review's slot is free");
    assert_eq!(
        refused(ask_copilot(&lab.shared, &pr(7), None, CopilotAsk::Next).await),
        "The QA copilot is still answering"
    );
    // A scenario turn runs while the copilot waits.
    assert!(matches!(
        write_scenarios(&lab.shared, &pr(7), ScenarioAsk::Fresh).await,
        Outcome::Ok(Reply::Ack)
    ));
    assert!(matches!(stop(&lab.shared, &pr(7)), Outcome::Ok(Reply::Ack)));
    let ended = told_cockpit(&mut lab, "stopped", |v| {
        !v.copilot.busy && v.activity.is_none()
    })
    .await;
    assert!(ended.copilot.failed.is_none(), "a stop is no failure");
    assert_eq!(ended.copilot.messages.len(), 1, "no answer was appended");
}

#[tokio::test]
async fn copilot_asks_are_checked_before_anything_starts() {
    let lab = cockpit_lab(Vec::new());
    let item = |kind: ItemKind, id: &str| {
        Some(ItemRef {
            kind,
            id: id.into(),
        })
    };
    assert_eq!(
        refused(
            ask_copilot(
                &lab.shared,
                &pr(7),
                item(ItemKind::Load, "x"),
                CopilotAsk::Next
            )
            .await
        ),
        "Load tests come later"
    );
    assert_eq!(
        refused(ask_copilot(&lab.shared, &pr(7), None, CopilotAsk::Explain).await),
        "Choose a scenario or a benchmark to explain"
    );
    assert_eq!(
        bad(ask_copilot(
            &lab.shared,
            &pr(7),
            item(ItemKind::Scenario, "../x"),
            CopilotAsk::Next
        )
        .await),
        "invalid scenario id"
    );
    assert_eq!(
        bad(ask_copilot(
            &lab.shared,
            &pr(7),
            item(ItemKind::Bench, "../x"),
            CopilotAsk::Next
        )
        .await),
        "invalid function id"
    );
    assert_eq!(
        refused(
            ask_copilot(
                &lab.shared,
                &pr(7),
                item(ItemKind::Scenario, "nowhere"),
                CopilotAsk::Next
            )
            .await
        ),
        "No scenario nowhere in acme/widgets#7"
    );
    assert_eq!(
        refused(
            ask_copilot(
                &lab.shared,
                &pr(7),
                None,
                CopilotAsk::Free { text: "  ".into() }
            )
            .await
        ),
        "Write a question first"
    );
    assert_eq!(
        refused(
            ask_copilot(
                &lab.shared,
                &pr(7),
                None,
                CopilotAsk::Free {
                    text: "x".repeat(2001)
                }
            )
            .await
        ),
        "At most 2000 characters"
    );
    assert!(env::load(&lab.shared, &pr(7)).copilot.is_empty());
    assert!(lab.calls_with(&["claude"]).is_empty());
}

#[tokio::test]
async fn explain_needs_a_result_and_the_prompt_carries_the_item() {
    let mut lab = cockpit_lab(vec![scenario_answer(&[TWO]), Turn::answer("It is flat.")]);
    let slug = write_by_turn(&mut lab, TWO).await;
    let item = Some(ItemRef {
        kind: ItemKind::Scenario,
        id: slug.clone(),
    });
    assert_eq!(
        refused(ask_copilot(&lab.shared, &pr(7), item.clone(), CopilotAsk::Explain).await),
        "Run this item first: there is nothing to explain yet"
    );
    let side = SideRecord {
        side: RunSide::Pr,
        sha: "h1".into(),
        input_hash: scenario_view(&lab, &slug).await.input_hash,
        runner: clusia_core::tests::RunnerKind::Container,
        rounds: Vec::new(),
        series: Series::default(),
        errors: Vec::new(),
        stopped: false,
    };
    env::update_record(&lab.shared, &pr(7), |record| {
        record.cockpit_results.insert(
            format!("scenario:{slug}"),
            ItemResults {
                single: Some(side),
                compare: None,
            },
        );
    })
    .await;
    assert!(matches!(
        ask_copilot(&lab.shared, &pr(7), item, CopilotAsk::Explain).await,
        Outcome::Ok(Reply::Ack)
    ));
    let done = told_cockpit(&mut lab, "answered", |v| {
        !v.copilot.busy && v.copilot.messages.len() == 2
    })
    .await;
    assert_eq!(done.copilot.messages[0].text, "Explain this comparison");
    assert_eq!(
        done.copilot.messages[0].item,
        Some(ItemRef {
            kind: ItemKind::Scenario,
            id: slug
        })
    );
    let calls = FakeClaude::calls(lab.fake.path());
    let argv = &calls[1].argv;
    let prompt = &argv[argv.iter().position(|a| a == "-p").unwrap() + 1];
    assert!(
        prompt.contains("Explain this comparison in plain words"),
        "{prompt}"
    );
    assert!(
        prompt.contains("    Two requests refresh at once\n"),
        "{prompt}"
    );
}

#[tokio::test]
async fn nothing_in_the_cockpit_runs_a_program_that_is_not_the_agent() {
    let mut lab = cockpit_lab(vec![scenario_answer(&[TWO])]);
    let slug = write_by_turn(&mut lab, TWO).await;
    approve_scenario_of(&lab, &slug).await;
    let ran = lab.ran();
    assert!(
        ran.iter().all(|line| !line.contains("docker run")
            && !line.contains("sandbox-exec")
            && !line.contains("python3")),
        "{ran:?}"
    );
    assert!(
        lab.worktree(7)
            .join(".clusia/e2e")
            .join(&slug)
            .join("run.py")
            .is_file()
    );
}

#[tokio::test]
async fn a_script_named_like_the_overlay_file_is_not_valid_and_writes_nothing() {
    let mut block = scenario_json("Overlay clash");
    block["scripts"] = json!([{"name": "edits.json", "text": "{}\n"}]);
    let mut lab = cockpit_lab(vec![Turn::answer_blocks("One.", &[scenario_block(block)])]);
    write_scenarios(&lab.shared, &pr(7), ScenarioAsk::Fresh).await;
    let done = told_cockpit(&mut lab, "failed", |v| {
        v.activity.is_none() && v.failed.is_some()
    })
    .await;
    assert!(done.scenarios.is_empty());
    assert!(
        done.failed
            .unwrap()
            .contains("The script name edits.json is reserved")
    );
    assert!(!lab.worktree(7).join(".clusia/e2e/overlay-clash").exists());
}
