//! The changed functions of a review against a fake machine: an index put in memory, the
//! storage in the lab's worktree, and `FakeClaude` playing the recipe and case turns.

use std::time::Instant;

use clusia_calls::{ChangeKind, Function, FunctionId};
use clusia_core::functions::{
    CASES_CHANGED, CaseInput, CaseOrigin, CaseScope, CasesAsk, FunctionChange, FunctionView,
    FunctionsActivity, FunctionsView, Support, case_hash, function_slug, view_hash,
};
use clusia_core::hunks::{FileHunks, FileStatus, Hunk};
use clusia_core::tests::{TestScope, TestsPhase};
use clusia_harness::testkit::{FakeClaude, Turn, function_block, recipe_block};
use clusia_protocol::{AgentLogEntry, ErrorCode, Event, Outcome, Reply};
use clusia_store::save_review;
use clusia_testrun::testkit::FakeReply;
use serde_json::json;

use super::lab::{Lab, Machine, built, generated, lab, no_setup, pr, recipe, refused, settled, wt};
use super::*;

const REFRESH_SIG: &str = "pub fn refresh(&self, now: u64) -> Result<u32, String> {";
const DRIVER: &str = "#[test]\nfn clusia_case() {}\n";
const RUST_COMMAND: &str =
    "cargo test --offline -p {package} --test {driver} -- --exact clusia_case --nocapture";

fn function(path: &str, qualified: &str, start: u32, end: u32) -> Function {
    Function {
        id: FunctionId(format!("{path}#{qualified}#{start}")),
        qualified: qualified.into(),
        name: qualified.rsplit("::").next().unwrap().into(),
        path: path.into(),
        start_line: start,
        end_line: end,
        signature_lines: 1,
        lang: "rust".into(),
    }
}

fn hunk(path: &str, start: u32, len: u32) -> FileHunks {
    FileHunks {
        old_path: Some(path.into()),
        new_path: Some(path.into()),
        status: FileStatus::Modified,
        hunks: vec![Hunk {
            old_start: start,
            old_len: len,
            new_start: start,
            new_len: len,
        }],
    }
}

/// The index of review `n` at `head`: `TokenStore::refresh` changed with `refresh_sig`,
/// `backoff_delay` new, a test function, a test file, a removed function and a Swift file.
fn index(lab: &Lab, n: u64, head: &str, refresh_sig: &str) {
    let built = crate::calls::tests::built_of(
        head,
        &[
            (
                function("src/store.rs", "TokenStore::refresh", 8, 12),
                ChangeKind::Changed,
                refresh_sig,
            ),
            (
                function("src/retry.rs", "backoff_delay", 1, 3),
                ChangeKind::New,
                "pub fn backoff_delay(attempt: u32) -> u64 {",
            ),
            (
                function("src/store.rs", "tests::refresh_works", 30, 33),
                ChangeKind::Changed,
                "fn refresh_works() {",
            ),
            (
                function("tests/api.rs", "api_works", 1, 4),
                ChangeKind::New,
                "fn api_works() {",
            ),
            (
                function("src/old.rs", "gone", 1, 2),
                ChangeKind::Removed,
                "fn gone() {",
            ),
        ],
        vec![
            hunk("src/store.rs", 9, 2),
            hunk("src/retry.rs", 1, 3),
            hunk("ui/badge.swift", 1, 4),
        ],
        &["ui/badge.swift"],
    );
    crate::calls::tests::install(&lab.shared, &pr(n), built);
}

fn refresh() -> String {
    function_slug("src/store.rs", "TokenStore::refresh")
}

fn backoff() -> String {
    function_slug("src/retry.rs", "backoff_delay")
}

fn functions_recipe() -> serde_json::Value {
    let mut recipe = no_setup(recipe());
    recipe["functions"] = json!({ "rust": RUST_COMMAND });
    recipe
}

fn refresh_block(cases: serde_json::Value) -> serde_json::Value {
    json!({
        "function": "TokenStore::refresh",
        "file": "src/store.rs",
        "state_label": "Store",
        "params": [
            {"name": "stored_token", "label": "stored token", "kind": "text"},
            {"name": "endpoint_status", "label": "endpoint status", "kind": "int"}
        ],
        "driver": DRIVER,
        "cases": cases
    })
}

fn two_cases() -> serde_json::Value {
    json!([
        {"name": "Endpoint fails, old token kept",
         "values": {"stored_token": "old", "endpoint_status": "500"},
         "expect": {"returned": "Err(Http(500))", "state": {"token": "old"}}},
        {"name": "Valid token, no exchange",
         "values": {"stored_token": "fresh", "endpoint_status": "200"},
         "expect": {"returned": "Ok(1)"}}
    ])
}

/// The recipe turn's answer: the recipe with its functions command, the cases of
/// `TokenStore::refresh`, and a block for a function that is not listed.
fn generate_answer() -> Turn {
    Turn::answer_blocks(
        "Here is the recipe and the cases.",
        &[
            recipe_block(functions_recipe()),
            function_block(refresh_block(two_cases())),
            function_block(
                json!({"function": "nowhere", "file": "src/x.rs", "params": [], "cases": []}),
            ),
        ],
    )
}

/// A running engine and an image already built: generate and setup need nothing more.
fn functions_lab(turns: Vec<Turn>) -> Lab {
    let lab = lab(Machine::Colima { running: true }, turns, &[7], |x, _| {
        built(x);
        x.on("docker", &["run"], FakeReply::ok(""));
    });
    index(&lab, 7, "h1", REFRESH_SIG);
    lab
}

fn of<'a>(view: &'a FunctionsView, slug: &str) -> &'a FunctionView {
    view.functions.iter().find(|f| f.slug == slug).expect(slug)
}

fn view_of(out: Outcome) -> FunctionsView {
    match out {
        Outcome::Ok(Reply::ChangedFunctions(view)) => *view,
        other => panic!("not a functions view: {other:?}"),
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

fn input(id: Option<&str>, name: &str, token: &str, status: &str) -> CaseInput {
    CaseInput {
        id: id.map(String::from),
        name: name.into(),
        values: [("stored_token", token), ("endpoint_status", status)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

/// The `FunctionsChanged` views told so far, waiting until one matches `done`.
async fn told(lab: &mut Lab, what: &str, done: impl Fn(&FunctionsView) -> bool) -> FunctionsView {
    let give_up = Instant::now() + clusia_core::testkit::HANG_BOUND;
    loop {
        for event in lab.drain() {
            if let Event::FunctionsChanged { view, .. } = event
                && done(&view)
            {
                return *view;
            }
        }
        assert!(Instant::now() < give_up, "{what}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn states(lab: &Lab) -> Vec<String> {
    crate::agent_log::read(&lab.shared.paths.agent_log(&pr(7)))
        .into_iter()
        .filter_map(|entry| match entry {
            AgentLogEntry::Tests { state, .. } => Some(state),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn generate_writes_the_cases_of_the_listed_functions() {
    let mut lab = functions_lab(vec![generate_answer()]);
    let started = generated(&lab, 7).await;
    assert_eq!(started.phase, TestsPhase::NeedsSetup);
    let prompt = FakeClaude::calls(lab.fake.path()).remove(0).argv;
    let prompt = &prompt[prompt.iter().position(|a| a == "-p").unwrap() + 1];
    assert!(prompt.contains("- `backoff_delay` in src/retry.rs, lines 1-3 (Rust, new): pub fn backoff_delay(attempt: u32) -> u64"));
    assert!(prompt.contains("- `TokenStore::refresh` in src/store.rs, lines 8-12 (Rust, changed)"));
    assert!(!prompt.contains("refresh_works") && !prompt.contains("api_works"));

    let view = told(&mut lab, "the view after generate", |v| {
        v.activity.is_none() && v.functions.len() == 2 && !v.functions[1].cases.is_empty()
    })
    .await;
    let slugs: Vec<&str> = view.functions.iter().map(|f| f.slug.as_str()).collect();
    assert_eq!(
        slugs,
        [backoff().as_str(), refresh().as_str()],
        "by path, then line"
    );
    assert_eq!(view.unsupported, ["ui/badge.swift"]);
    assert!(!view.reading && view.note.is_none() && view.failed.is_none());
    assert_eq!(view.hash, view_hash(&view.functions));

    let f = of(&view, &refresh());
    assert_eq!(
        f.signature,
        "pub fn refresh(&self, now: u64) -> Result<u32, String>"
    );
    assert_eq!(
        (f.change, f.start_line, f.diff_lines),
        (FunctionChange::Changed, 8, Some((9, 10)))
    );
    assert_eq!(f.state_label.as_deref(), Some("Store"));
    assert_eq!(f.support, Support::Ready);
    assert_eq!(
        f.driver,
        Some(format!(".clusia/functions/{}/driver.rs", refresh()))
    );
    assert_eq!(
        f.cases.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["a1", "a2"]
    );
    assert!(
        f.cases
            .iter()
            .all(|c| c.origin == CaseOrigin::Agent && !c.edited && c.result.is_none())
    );
    let driver_hash = clusia_testrun::names::sha256_hex(DRIVER.as_bytes());
    assert_eq!(
        f.cases[0].hash,
        case_hash(
            Some(&driver_hash),
            Some(RUST_COMMAND),
            &f.cases[0].values,
            &f.cases[0].expect
        )
    );
    assert_eq!(of(&view, &backoff()).support, Support::NoCases);
    assert_eq!(of(&view, &backoff()).driver, None);

    let folder = lab.worktree(7).join(".clusia/functions").join(refresh());
    assert_eq!(
        std::fs::read_to_string(folder.join("driver.rs")).unwrap(),
        DRIVER
    );
    assert!(folder.join("function.json").is_file());
    let mut made: Vec<String> = std::fs::read_dir(lab.worktree(7).join(".clusia/functions"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    made.sort();
    assert_eq!(made, [refresh()], "the unmatched block wrote nothing");
}

#[tokio::test]
async fn generate_tells_the_functions_view_at_its_start_and_end() {
    let mut lab = functions_lab(vec![generate_answer()]);
    assert!(matches!(
        generate(&lab.shared, &pr(7)).await,
        Outcome::Ok(Reply::Ack)
    ));
    told(&mut lab, "writing while generating", |v| {
        v.activity
            == Some(FunctionsActivity::Writing {
                function: None,
                ask: None,
            })
    })
    .await;
    told(&mut lab, "the end", |v| v.activity.is_none()).await;
}

#[tokio::test]
async fn an_index_still_building_reads_and_a_failed_one_says_why() {
    let mut lab = lab(Machine::NoEngine, Vec::new(), &[7], |_, _| {});
    let first = cases::view(&lab.shared, &pr(7)).await;
    assert!(first.reading && first.functions.is_empty() && first.note.is_none());
    // The lab's worktree is no repository, so the build fails and the waiting task tells it.
    let failed = told(&mut lab, "the failed index", |v| !v.reading).await;
    assert!(
        failed
            .note
            .as_deref()
            .is_some_and(|n| n.starts_with("The changed functions could not be read: ")),
        "{failed:?}"
    );
}

#[tokio::test]
async fn a_review_that_is_not_open_answers_an_empty_view() {
    let lab = functions_lab(Vec::new());
    let view = view_of(functions(&lab.shared, &pr(99)).await);
    assert_eq!(view.note.as_deref(), Some(cases::OPEN_FIRST));
    assert!(view.functions.is_empty() && view.head.is_empty());
    assert!(
        refused(save_case(&lab.shared, &pr(99), &refresh(), input(None, "x", "a", "1")).await)
            .starts_with("no review for acme/widgets#99")
    );
}

#[tokio::test]
async fn save_reset_and_remove_follow_the_case_rules() {
    let mut lab = functions_lab(vec![generate_answer()]);
    generated(&lab, 7).await;
    let name = crate::sessions::name_for(&lab.shared, &pr(7)).await;
    let before = view_of(functions(&lab.shared, &pr(7)).await);
    lab.drain();

    let edited = view_of(
        save_case(
            &lab.shared,
            &pr(7),
            &refresh(),
            input(Some("a1"), "Endpoint fails, old token kept", "old", "503"),
        )
        .await,
    );
    let a1 = &of(&edited, &refresh()).cases[0];
    assert!(a1.edited);
    assert_eq!(a1.values["endpoint_status"], "503");
    assert_ne!(of(&edited, &refresh()).hash, of(&before, &refresh()).hash);
    assert!(
        lab.drain()
            .iter()
            .any(|e| matches!(e, Event::FunctionsChanged { view, .. } if **view == edited))
    );
    let edits: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            lab.worktree(7)
                .join(".clusia/functions")
                .join(refresh())
                .join("edits.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(edits["values"], json!({"a1": {"endpoint_status": "503"}}));

    let back = view_of(
        save_case(
            &lab.shared,
            &pr(7),
            &refresh(),
            input(Some("a1"), "Endpoint fails, old token kept", "old", "500"),
        )
        .await,
    );
    assert!(
        !of(&back, &refresh()).cases[0].edited,
        "editing back clears edited"
    );
    assert_eq!(of(&back, &refresh()).hash, of(&before, &refresh()).hash);

    assert!(
        bad(save_case(
            &lab.shared,
            &pr(7),
            &refresh(),
            input(Some("a1"), "x", "old", "five")
        )
        .await)
        .contains("Enter a whole number")
    );

    let added = view_of(
        save_case(
            &lab.shared,
            &pr(7),
            &refresh(),
            input(None, "New case", "old", "500"),
        )
        .await,
    );
    let r1 = of(&added, &refresh()).cases.last().unwrap().clone();
    assert_eq!(
        (r1.id.as_str(), r1.origin, r1.edited),
        ("r1", CaseOrigin::Reviewer, true)
    );
    assert_eq!(r1.expect, Default::default());

    assert_eq!(
        refused(reset_case(&lab.shared, &pr(7), &refresh(), "r1").await),
        format!("Only {name}'s cases can be reset")
    );
    assert_eq!(
        refused(remove_case(&lab.shared, &pr(7), &refresh(), "a1").await),
        "Only your cases can be removed"
    );
    assert_eq!(
        refused(reset_case(&lab.shared, &pr(7), &refresh(), "a9").await),
        "No case a9 in TokenStore::refresh"
    );
    assert_eq!(
        refused(
            save_case(
                &lab.shared,
                &pr(7),
                "nothing_here-0123456789",
                input(None, "x", "a", "1")
            )
            .await
        ),
        "No changed function nothing_here-0123456789 in acme/widgets#7"
    );
    assert_eq!(
        refused(save_case(&lab.shared, &pr(7), &backoff(), input(None, "x", "a", "1")).await),
        format!("{name} has not written cases for this function yet.")
    );

    view_of(
        save_case(
            &lab.shared,
            &pr(7),
            &refresh(),
            input(Some("a2"), "Fresh token", "fresh", "200"),
        )
        .await,
    );
    let reset = view_of(reset_case(&lab.shared, &pr(7), &refresh(), "a2").await);
    assert_eq!(
        of(&reset, &refresh()).cases[1].name,
        "Valid token, no exchange"
    );
    assert!(!of(&reset, &refresh()).cases[1].edited);

    env::update_record(&lab.shared, &pr(7), |record| {
        let mut result = sample_result("r1");
        result.hash = r1.hash.clone();
        record
            .case_results
            .entry(refresh())
            .or_default()
            .insert("r1".into(), result);
    })
    .await;
    let removed = view_of(remove_case(&lab.shared, &pr(7), &refresh(), "r1").await);
    assert!(of(&removed, &refresh()).cases.iter().all(|c| c.id != "r1"));
    assert!(
        env::load(&lab.shared, &pr(7))
            .case_results
            .get(&refresh())
            .is_none_or(|r| !r.contains_key("r1"))
    );
    let again = view_of(
        save_case(
            &lab.shared,
            &pr(7),
            &refresh(),
            input(None, "New case", "old", "500"),
        )
        .await,
    );
    assert_eq!(
        of(&again, &refresh()).cases.last().unwrap().id,
        "r2",
        "ids are never reused"
    );
}

fn sample_result(case: &str) -> clusia_core::functions::CaseResult {
    clusia_core::functions::CaseResult {
        case: case.into(),
        head: "h1".into(),
        hash: String::new(),
        status: clusia_core::functions::CaseStatus::Passed,
        returned: Some("Ok(1)".into()),
        returned_expected: None,
        error: None,
        state: Vec::new(),
        output: String::new(),
        took_ms: 10,
        runner: clusia_core::tests::RunnerKind::Container,
        exit_code: Some(0),
        message: None,
        at: 1,
    }
}

#[tokio::test]
async fn invalid_function_ids_are_bad_requests() {
    let lab = functions_lab(vec![generate_answer()]);
    generated(&lab, 7).await;
    for slug in [
        "../../etc/x",
        "a/b",
        ".",
        "~",
        "x\0y",
        "tokenstore_refresh-0123456789\u{202e}",
        "TokenStore-0123456789",
        "",
    ] {
        let shared = &lab.shared;
        assert_eq!(
            bad(save_case(shared, &pr(7), slug, input(None, "x", "a", "1")).await),
            "invalid function id"
        );
        assert_eq!(
            bad(reset_case(shared, &pr(7), slug, "a1").await),
            "invalid function id"
        );
        assert_eq!(
            bad(remove_case(shared, &pr(7), slug, "r1").await),
            "invalid function id"
        );
        assert_eq!(
            bad(driver(shared, &pr(7), slug).await),
            "invalid function id"
        );
        assert_eq!(
            bad(write_cases(shared, &pr(7), slug, CasesAsk::Fresh).await),
            "invalid function id"
        );
    }
    let made: Vec<String> = std::fs::read_dir(lab.worktree(7).join(".clusia/functions"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(made, [refresh()]);
    assert!(!lab.worktree(7).join("etc").exists());
}

#[tokio::test]
async fn a_changed_signature_makes_the_cases_stale_and_a_body_change_does_not() {
    let lab = functions_lab(vec![generate_answer()]);
    generated(&lab, 7).await;
    let move_head = |head: &str| {
        let mut review = crate::reviews::load_stored(&lab.shared, &pr(7))
            .unwrap()
            .unwrap();
        review.head_sha = head.into();
        save_review(&lab.shared.paths, &review).unwrap();
    };
    // A new head that changes only the body.
    move_head("h2");
    index(&lab, 7, "h2", REFRESH_SIG);
    let body = cases::view(&lab.shared, &pr(7)).await;
    assert_eq!(body.head, "h2");
    assert_eq!(of(&body, &refresh()).support, Support::Ready);
    // A new head that changes the signature.
    move_head("h3");
    index(
        &lab,
        7,
        "h3",
        "pub fn refresh(&self, now: u64, force: bool) -> Result<u32, String> {",
    );
    let stale = cases::view(&lab.shared, &pr(7)).await;
    assert_eq!(
        of(&stale, &refresh()).support,
        Support::Stale {
            reason: "The signature changed since these cases were written".into()
        }
    );
    assert_ne!(of(&stale, &refresh()).hash, of(&body, &refresh()).hash);
}

#[tokio::test]
async fn a_run_with_an_old_hash_is_refused() {
    let lab = functions_lab(vec![generate_answer()]);
    generated(&lab, 7).await;
    let seen = cases::view(&lab.shared, &pr(7)).await;
    let now = view_of(
        save_case(
            &lab.shared,
            &pr(7),
            &refresh(),
            input(Some("a1"), "Endpoint fails, old token kept", "old", "503"),
        )
        .await,
    );
    let one = CaseScope::Case {
        function: refresh(),
        case: "a1".into(),
    };
    let whole = CaseScope::Function {
        function: refresh(),
    };
    for scope in [&one, &whole] {
        assert_eq!(
            refused(
                cases::check_seen(&now, &pr(7), scope, &of(&seen, &refresh()).hash).unwrap_err()
            ),
            CASES_CHANGED
        );
        assert!(cases::check_seen(&now, &pr(7), scope, &of(&now, &refresh()).hash).is_ok());
    }
    assert_eq!(
        refused(cases::check_seen(&now, &pr(7), &CaseScope::All, &seen.hash).unwrap_err()),
        CASES_CHANGED
    );
    assert!(cases::check_seen(&now, &pr(7), &CaseScope::All, &now.hash).is_ok());
    let missing = CaseScope::Case {
        function: refresh(),
        case: "a9".into(),
    };
    assert_eq!(
        refused(cases::check_seen(&now, &pr(7), &missing, &of(&now, &refresh()).hash).unwrap_err()),
        "No case a9 in TokenStore::refresh"
    );
    let unknown = CaseScope::Function {
        function: "nothing_here-0123456789".into(),
    };
    assert_eq!(
        refused(cases::check_seen(&now, &pr(7), &unknown, "x").unwrap_err()),
        "No changed function nothing_here-0123456789 in acme/widgets#7"
    );
}

#[tokio::test]
async fn write_cases_runs_one_forked_turn_and_writes_its_block() {
    let more = refresh_block(json!([
        {"name": "Expired token, one exchange",
         "values": {"stored_token": "expired", "endpoint_status": "200"},
         "expect": {"returned": "Ok(2)"}}
    ]));
    let mut lab = functions_lab(vec![
        generate_answer(),
        Turn::answer_blocks("One more.", &[function_block(more)]),
    ]);
    generated(&lab, 7).await;
    lab.drain();
    assert!(matches!(
        write_cases(&lab.shared, &pr(7), &refresh(), CasesAsk::More).await,
        Outcome::Ok(Reply::Ack)
    ));
    told(&mut lab, "writing", |v| {
        v.activity
            == Some(FunctionsActivity::Writing {
                function: Some(refresh()),
                ask: Some(CasesAsk::More),
            })
    })
    .await;
    let done = told(&mut lab, "written", |v| v.activity.is_none()).await;
    let f = of(&done, &refresh());
    assert_eq!(
        f.cases.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["a1", "a2", "a3"]
    );
    assert_eq!(f.cases[2].name, "Expired token, one exchange");
    assert!(done.failed.is_none());

    let calls = FakeClaude::calls(lab.fake.path());
    let argv = &calls[1].argv;
    let prompt = &argv[argv.iter().position(|a| a == "-p").unwrap() + 1];
    assert!(
        prompt.starts_with("Write more test cases for one changed function"),
        "{prompt}"
    );
    assert!(prompt.contains(
        "    a2 Valid token, no exchange: endpoint_status = 200, stored_token = fresh\n"
    ));
    assert!(prompt.contains(&format!("Command that runs the driver: {RUST_COMMAND}")));
    assert_eq!(
        states(&lab),
        ["waiting", "running", "done", "waiting", "running", "done"]
    );
}

#[tokio::test]
async fn writing_cases_takes_the_review_slot_and_keeps_the_tests_phase() {
    let mut lab = functions_lab(vec![generate_answer(), Turn::hanging()]);
    generated(&lab, 7).await;
    let function_json = lab
        .worktree(7)
        .join(".clusia/functions")
        .join(refresh())
        .join("function.json");
    let before = std::fs::read(&function_json).unwrap();
    assert!(matches!(
        write_cases(&lab.shared, &pr(7), &refresh(), CasesAsk::Fresh).await,
        Outcome::Ok(Reply::Ack)
    ));
    let busy = "The tests of acme/widgets#7 are busy writing cases; wait for it or stop it";
    assert_eq!(
        refused(run(&lab.shared, &pr(7), TestScope::All).await),
        busy
    );
    assert_eq!(
        refused(write_cases(&lab.shared, &pr(7), &refresh(), CasesAsk::Fresh).await),
        busy
    );
    assert_eq!(
        env::view(&lab.shared, &pr(7)).await.phase,
        TestsPhase::NeedsSetup
    );
    assert!(matches!(stop(&lab.shared, &pr(7)), Outcome::Ok(Reply::Ack)));
    let ended = told(&mut lab, "stopped", |v| v.activity.is_none()).await;
    assert!(ended.failed.is_none(), "a stop is no failure");
    assert_eq!(std::fs::read(&function_json).unwrap(), before);
    settled(&lab, 7).await;
}

#[tokio::test]
async fn an_invalid_case_answer_changes_nothing_and_says_why() {
    let bell = refresh_block(json!([
        {"name": "Bell", "values": {"stored_token": "a\u{7}", "endpoint_status": "200"}}
    ]));
    let mut lab = functions_lab(vec![
        generate_answer(),
        Turn::answer_blocks("x", &[function_block(bell)]),
        Turn::answer("I could not write cases."),
    ]);
    generated(&lab, 7).await;
    let name = crate::sessions::name_for(&lab.shared, &pr(7)).await;
    let function_json = lab
        .worktree(7)
        .join(".clusia/functions")
        .join(refresh())
        .join("function.json");
    let before = std::fs::read(&function_json).unwrap();
    lab.drain();
    write_cases(&lab.shared, &pr(7), &refresh(), CasesAsk::Fresh).await;
    let failed = told(&mut lab, "invalid", |v| {
        v.activity.is_none() && v.failed.is_some()
    })
    .await;
    assert!(
        failed
            .failed
            .as_deref()
            .unwrap()
            .starts_with(&format!("{name}'s cases are not valid: ")),
        "{:?}",
        failed.failed
    );
    assert_eq!(std::fs::read(&function_json).unwrap(), before);

    write_cases(&lab.shared, &pr(7), &refresh(), CasesAsk::Fresh).await;
    let none = told(&mut lab, "no block", |v| {
        v.activity.is_none() && v.failed.as_deref().is_some_and(|f| f.contains("holds no"))
    })
    .await;
    assert_eq!(
        none.failed,
        Some(format!("{name}'s answer holds no clusia-function block"))
    );
    assert_eq!(
        env::view(&lab.shared, &pr(7)).await.phase,
        TestsPhase::NeedsSetup,
        "a case turn's failure is not the tests' failure"
    );
}

#[tokio::test]
async fn new_values_replace_one_case_and_clear_its_overlay() {
    let values = refresh_block(json!([
        {"name": "x", "values": {"stored_token": "stale", "endpoint_status": "401"},
         "expect": {"returned": "Err(Http(401))"}}
    ]));
    let mut lab = functions_lab(vec![
        generate_answer(),
        Turn::answer_blocks("x", &[function_block(values)]),
    ]);
    generated(&lab, 7).await;
    view_of(
        save_case(
            &lab.shared,
            &pr(7),
            &refresh(),
            input(Some("a1"), "Endpoint fails, old token kept", "old", "503"),
        )
        .await,
    );
    lab.drain();
    write_cases(
        &lab.shared,
        &pr(7),
        &refresh(),
        CasesAsk::Values { case: "a1".into() },
    )
    .await;
    let done = told(&mut lab, "new values", |v| v.activity.is_none()).await;
    let a1 = &of(&done, &refresh()).cases[0];
    assert!(!a1.edited);
    assert_eq!(
        (
            a1.values["stored_token"].as_str(),
            a1.values["endpoint_status"].as_str()
        ),
        ("stale", "401")
    );
    assert_eq!(a1.expect.returned.as_deref(), Some("Err(Http(401))"));
    assert_eq!(
        refused(
            write_cases(
                &lab.shared,
                &pr(7),
                &refresh(),
                CasesAsk::Values { case: "a9".into() }
            )
            .await
        ),
        "No case a9 in TokenStore::refresh"
    );
}

#[tokio::test]
async fn the_driver_is_read_without_following_links() {
    let lab = functions_lab(vec![generate_answer()]);
    generated(&lab, 7).await;
    let Outcome::Ok(Reply::Driver(code)) = driver(&lab.shared, &pr(7), &refresh()).await else {
        panic!("a driver");
    };
    assert_eq!(code.function, refresh());
    assert_eq!(
        code.path,
        format!(".clusia/functions/{}/driver.rs", refresh())
    );
    assert_eq!(code.text, DRIVER);
    assert_eq!(
        code.hash,
        clusia_testrun::names::sha256_hex(DRIVER.as_bytes())
    );
    assert_eq!(
        refused(driver(&lab.shared, &pr(7), &backoff()).await),
        "backoff_delay has no driver yet"
    );

    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.rs"), "secret").unwrap();
    let file = lab
        .worktree(7)
        .join(".clusia/functions")
        .join(refresh())
        .join("driver.rs");
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.rs"), &file).unwrap();
    let message = refused(driver(&lab.shared, &pr(7), &refresh()).await);
    assert!(!message.contains("secret"), "{message}");
}

#[tokio::test]
async fn a_symlinked_functions_folder_is_unsupported_and_refuses_writes() {
    let lab = functions_lab(vec![generate_answer()]);
    generated(&lab, 7).await;
    let outside = tempfile::tempdir().unwrap();
    let functions_dir = lab.worktree(7).join(".clusia/functions");
    std::fs::remove_dir_all(&functions_dir).unwrap();
    std::os::unix::fs::symlink(outside.path(), &functions_dir).unwrap();
    let view = cases::view(&lab.shared, &pr(7)).await;
    assert_eq!(
        of(&view, &refresh()).support,
        Support::Unsupported {
            reason: cases::FOLDER_REFUSED.into()
        }
    );
    assert_eq!(
        refused(save_case(&lab.shared, &pr(7), &refresh(), input(None, "x", "a", "1")).await),
        cases::FOLDER_REFUSED
    );
    assert_eq!(
        refused(write_cases(&lab.shared, &pr(7), &refresh(), CasesAsk::Fresh).await),
        cases::FOLDER_REFUSED
    );
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn a_shipped_clusia_folder_empties_the_view_and_refuses_every_write() {
    let lab = lab(Machine::NoEngine, Vec::new(), &[7], |x, paths| {
        x.on(
            "git",
            &["-C", wt(paths, 7).as_str()],
            FakeReply::ok(".clusia/functions/x/function.json\0"),
        );
    });
    index(&lab, 7, "h1", REFRESH_SIG);
    let shipped = clusia_testrun::scan::SHIPPED_CLUSIA;
    let view = cases::view(&lab.shared, &pr(7)).await;
    assert_eq!(view.note.as_deref(), Some(shipped));
    assert!(view.functions.is_empty());
    assert_eq!(
        refused(save_case(&lab.shared, &pr(7), &refresh(), input(None, "x", "a", "1")).await),
        shipped
    );
    assert_eq!(
        refused(write_cases(&lab.shared, &pr(7), &refresh(), CasesAsk::Fresh).await),
        shipped
    );
    assert_eq!(
        refused(driver(&lab.shared, &pr(7), &refresh()).await),
        shipped
    );
}
