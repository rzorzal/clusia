mod common;

use std::time::Duration;

use clusia_core::checks::{AuditArea, CheckKind, CheckResult, Finding, default_areas, finding_id};
use clusia_core::{DraftKind, Origin, Verdict};
use clusia_harness::testkit::{FakeClaude, Script, Turn, finding_block, pass_block};
use clusia_protocol::{
    AgentLogEntry, CheckState, CheckStatus, Client, Command, ErrorCode, Event, LoadStepKind,
    PermissionOutcome, Reply, StepStatus, TurnOrigin, topics,
};
use common::agent::{
    calls, checks_on_open, fake_program, ready, refused, send, until, use_fake, value_of, watching,
};
use common::git_fixture::advance_pr;
use common::github_mock::{PrMock, mount_pr, mount_publish};
use common::review_world::{World, open, pr7, world};
use serde_json::json;

const REVIEW_URL: &str = "https://github.com/acme/widgets/pull/7#pullrequestreview-42";

/// Blocks a Security run answers with: a finding on a new line, one on a line the diff does not
/// have, one on a range of new lines, one with an area that does not exist, and a pass.
fn security_blocks() -> Vec<String> {
    vec![
        finding_block(json!({
            "area": "security", "severity": "high", "title": "Token in the log",
            "file": "feature.txt", "line": 2, "body": "The token reaches the log.",
            "comment": "Do not log the token.", "code": "- log(token)\n+ log(\"…\")"
        })),
        finding_block(json!({
            "area": "security", "severity": "low", "title": "Loose parse",
            "file": "feature.txt", "line": 99, "body": "Line 99 is not in the diff."
        })),
        finding_block(json!({
            "area": "security", "severity": "medium", "title": "Open range",
            "file": "feature.txt", "start_line": 2, "end_line": 3,
            "body": "Both lines are new.", "comment": "Check both lines."
        })),
        finding_block(json!({
            "area": "nope", "severity": "low", "title": "Unknown area",
            "file": "feature.txt", "line": 1, "body": "x"
        })),
        pass_block(json!({
            "area": "security", "text": "No secrets in the diff", "where": "checked 1 file"
        })),
    ]
}

fn security_answer() -> Turn {
    Turn::answer_blocks("Three things.", &security_blocks())
}

/// One finding for each kind of run: what a Security run keeps, and what an Audit run keeps.
fn both_kinds_answer() -> Turn {
    Turn::answer_blocks(
        "Findings.",
        &[
            finding_block(json!({
                "area": "security", "severity": "high", "title": "Token in the log",
                "file": "feature.txt", "line": 2, "body": "b"
            })),
            finding_block(json!({
                "area": "correctness", "title": "Off by one",
                "file": "feature.txt", "line": 3, "body": "b"
            })),
        ],
    )
}

fn run_check(kind: CheckKind) -> Command {
    Command::RunCheck { pr: pr7(), kind }
}

fn done(kind: CheckKind) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::CheckDone { kind: k, .. } if *k == kind)
}

fn state_is(kind: CheckKind, wanted: fn(&CheckState) -> bool) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::CheckState { kind: k, state, .. } if *k == kind && wanted(state))
}

fn is_running(state: &CheckState) -> bool {
    matches!(state, CheckState::Running { .. })
}

fn is_not_run(state: &CheckState) -> bool {
    *state == CheckState::NotRun
}

fn is_stale(state: &CheckState) -> bool {
    *state == CheckState::Stale
}

fn is_failed(state: &CheckState) -> bool {
    matches!(state, CheckState::Failed { .. })
}

/// A check that asks permission for a command and then waits: it is still running when the
/// review is closed, published or discarded.
fn asking_and_hanging() -> Turn {
    Turn::answer("Ran.")
        .ask_permission("Bash", json!({"command": "cargo test -p clusia-core"}))
        .then_hang()
}

fn is_request(e: &Event) -> bool {
    matches!(e, Event::PermissionRequested { .. })
}

fn is_cancelled(e: &Event) -> bool {
    matches!(
        e,
        Event::PermissionResolved {
            outcome: PermissionOutcome::Cancelled,
            ..
        }
    )
}

async fn no_request_waits(c: &mut Client) {
    let Reply::Permissions(waiting) = c
        .request(Command::GetPermissions { pr: pr7() })
        .await
        .unwrap()
    else {
        panic!("the waiting requests");
    };
    assert!(waiting.is_empty(), "{waiting:?}");
}

struct Checks {
    results: Vec<CheckResult>,
    states: Vec<CheckStatus>,
    accepted: Vec<String>,
    dismissed: Vec<String>,
}

impl Checks {
    fn state(&self, kind: CheckKind) -> CheckState {
        self.states
            .iter()
            .find(|s| s.kind == kind)
            .expect("both kinds have a state")
            .state
            .clone()
    }

    fn result(&self, kind: CheckKind) -> &CheckResult {
        self.results
            .iter()
            .find(|r| r.kind == kind)
            .expect("a stored result")
    }

    fn finding(&self, kind: CheckKind, title: &str) -> &Finding {
        self.result(kind)
            .findings
            .iter()
            .find(|f| f.title == title)
            .expect("a finding with that title")
    }
}

async fn checks_of(c: &mut Client) -> Checks {
    match c.request(Command::GetChecks { pr: pr7() }).await.unwrap() {
        Reply::Checks {
            results,
            states,
            accepted,
            dismissed,
        } => Checks {
            results,
            states,
            accepted,
            dismissed,
        },
        other => panic!("expected the checks, got {other:?}"),
    }
}

async fn session_of(c: &mut Client) -> Option<String> {
    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(review) => review.harness_session,
        other => panic!("expected the review file, got {other:?}"),
    }
}

/// Waits until the process is gone.
async fn gone(pid: u32) -> bool {
    for _ in 0..60 {
        if !FakeClaude::is_running(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Moves the pull request to a new head and serves it.
async fn advance(w: &World) -> String {
    let content = "zero\none\ntwo\nthree\n";
    let head = advance_pr(w.tmp.path(), 7, "feature.txt", content);
    w.server.reset().await;
    mount_pr(
        &w.server,
        &PrMock::new(&head, &w.base, &w.origin).adding_feature(content),
    )
    .await;
    head
}

/// The Agent row's last note, read from the loading steps of a review that was opened.
async fn agent_step(watcher: &mut Client) -> Option<(StepStatus, Option<String>)> {
    let mut step = None;
    while let Ok(Ok((_, event))) =
        tokio::time::timeout(Duration::from_millis(300), watcher.next_event()).await
    {
        if let Event::LoadStep(s) = event
            && s.step == LoadStepKind::Agent
        {
            step = Some((s.status, s.message));
        }
    }
    step
}

async fn watching_reviews(w: &World) -> Client {
    let mut c = w.daemon.client().await;
    c.request(Command::Subscribe {
        topics: vec![topics::REVIEWS.into()],
    })
    .await
    .unwrap();
    c
}

/// An until-predicate that holds at the second check that finishes.
fn both_done() -> impl Fn(&Event) -> bool {
    let seen = std::cell::Cell::new(0);
    move |e| {
        if matches!(e, Event::CheckDone { .. }) {
            seen.set(seen.get() + 1);
        }
        seen.get() == 2
    }
}

#[tokio::test]
async fn a_check_turns_its_blocks_into_findings_and_passes() {
    let w = world().await;
    let dir = use_fake(&w, Script::one(security_answer())).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    assert_eq!(
        c.request(run_check(CheckKind::Security)).await.unwrap(),
        Reply::Ack
    );

    let events = until(&mut watcher, done(CheckKind::Security)).await;
    assert!(
        matches!(
            events.first(),
            Some(Event::CheckState {
                state: CheckState::Waiting,
                ..
            })
        ),
        "{events:?}"
    );
    assert!(events.iter().any(state_is(CheckKind::Security, is_running)));
    let titles: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::CheckFinding { finding, .. } => Some(finding.title.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(titles, ["Token in the log", "Loose parse", "Open range"]);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::CheckPass { .. }))
            .count(),
        1
    );
    let after = until(&mut watcher, |e| matches!(e, Event::CheckState { .. })).await;
    assert!(
        matches!(
            after.last(),
            Some(Event::CheckState {
                state: CheckState::Done,
                ..
            })
        ),
        "the state follows the result"
    );

    let checks = checks_of(&mut c).await;
    assert_eq!(checks.state(CheckKind::Security), CheckState::Done);
    assert_eq!(checks.state(CheckKind::Audit), CheckState::NotRun);
    assert!(checks.accepted.is_empty() && checks.dismissed.is_empty());
    let result = checks.result(CheckKind::Security);
    assert_eq!((result.files, result.unreadable), (1, 1));
    assert_eq!(result.head, w.head);
    let anchored: Vec<(&str, bool)> = result
        .findings
        .iter()
        .map(|f| (f.title.as_str(), f.anchored))
        .collect();
    assert_eq!(
        anchored,
        [
            ("Token in the log", true),
            ("Loose parse", false),
            ("Open range", true)
        ]
    );
    assert_eq!(
        result.findings[0].id,
        finding_id("security", "feature.txt", Some(2), "Token in the log")
    );
    assert_eq!(result.findings[1].comment, "Line 99 is not in the diff.");
    assert_eq!(result.passes[0].place.as_deref(), Some("checked 1 file"));

    let call = calls(&dir, 1).await.remove(0);
    assert!(call.argv.iter().any(|a| a == "--session-id"));
    assert!(
        !call
            .argv
            .iter()
            .any(|a| a == "--resume" || a == "--fork-session"),
        "a review with no session has nothing to fork: {:?}",
        call.argv
    );
    assert!(
        value_of(&call.argv, "-p")
            .unwrap()
            .contains(".clusia/review.md"),
        "a new session is told where the review notes are"
    );
    assert_eq!(
        session_of(&mut c).await,
        None,
        "a check never becomes the chat's session"
    );
    let Reply::AgentLog(entries) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap()
    else {
        panic!("an agent log");
    };
    assert!(
        !entries
            .iter()
            .any(|e| matches!(e, AgentLogEntry::Check { .. })),
        "the chat does not replay checks"
    );
}

#[tokio::test]
async fn a_check_forks_the_chat_session_and_never_replaces_it() {
    let w = world().await;
    let dir = use_fake(
        &w,
        Script::turns(vec![
            Turn::answer("Chat one."),
            security_answer(),
            Turn::answer("Chat two."),
        ]),
    )
    .await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(send("hello")).await.unwrap();
    until(&mut watcher, ready).await;
    let chat = session_of(&mut c).await.expect("the chat has a session");

    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, done(CheckKind::Security)).await;
    let fork = calls(&dir, 2).await.remove(1);
    assert_eq!(value_of(&fork.argv, "--resume"), Some(chat.clone()));
    assert!(fork.argv.iter().any(|a| a == "--fork-session"));
    let fork_id = value_of(&fork.argv, "--session-id").expect("the fork has its own id");
    assert_ne!(fork_id, chat);
    assert!(
        !value_of(&fork.argv, "-p")
            .unwrap()
            .contains(".clusia/review.md"),
        "a fork already knows the review"
    );
    assert_eq!(session_of(&mut c).await, Some(chat.clone()));

    c.request(send("again")).await.unwrap();
    until(&mut watcher, ready).await;
    let third = calls(&dir, 3).await.remove(2);
    assert_eq!(value_of(&third.argv, "--resume"), Some(chat));
}

#[tokio::test]
async fn the_chat_answers_while_a_check_runs() {
    let w = world().await;
    let dir = use_fake(
        &w,
        Script::turns(vec![
            Turn::answer("Chat one."),
            Turn::hanging(),
            Turn::answer("Chat two."),
        ]),
    )
    .await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(send("hello")).await.unwrap();
    until(&mut watcher, ready).await;
    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, state_is(CheckKind::Security, is_running)).await;

    c.request(send("while it runs")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    let said: String = events
        .iter()
        .filter_map(|e| match e {
            Event::AgentChunk { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(said, "Chat two.");
    assert!(is_running(
        &checks_of(&mut c).await.state(CheckKind::Security)
    ));

    c.request(Command::StopCheck {
        pr: pr7(),
        kind: CheckKind::Security,
    })
    .await
    .unwrap();
    until(&mut watcher, state_is(CheckKind::Security, is_not_run)).await;
    let check = calls(&dir, 3).await.remove(1);
    assert!(gone(check.pid).await, "Stop ended the check's process");
}

#[tokio::test]
async fn the_checks_start_after_the_summary_and_the_agent_row_says_so() {
    let w = world().await;
    let dir = fake_program(
        &w,
        Script::turns(vec![
            Turn::answer("The summary.").delay_ms(300),
            both_kinds_answer(),
        ]),
    )
    .await;
    checks_on_open(&w, true, true).await;
    let mut watcher = watching(&w).await;
    let mut steps = watching_reviews(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let waiting = checks_of(&mut c).await;
    assert_eq!(
        waiting.state(CheckKind::Security),
        CheckState::Waiting,
        "a check that waits for the summary reports Waiting"
    );
    assert_eq!(waiting.state(CheckKind::Audit), CheckState::Waiting);
    until(&mut watcher, both_done()).await;

    assert_eq!(
        agent_step(&mut steps).await,
        Some((
            StepStatus::Done,
            Some("Summarizing · Checking security · Auditing (6 areas)".into())
        ))
    );
    let all = calls(&dir, 3).await;
    assert!(
        value_of(&all[0].argv, "-p")
            .unwrap()
            .starts_with("Summarize this pull request"),
        "the summary runs first"
    );
    let session = session_of(&mut c).await.expect("the summary's session");
    for fork in &all[1..] {
        assert_eq!(value_of(&fork.argv, "--resume"), Some(session.clone()));
        assert!(fork.argv.iter().any(|a| a == "--fork-session"));
    }
    let checks = checks_of(&mut c).await;
    assert_eq!(checks.state(CheckKind::Security), CheckState::Done);
    assert_eq!(checks.state(CheckKind::Audit), CheckState::Done);
    checks.finding(CheckKind::Security, "Token in the log");
    let off_by_one = checks.finding(CheckKind::Audit, "Off by one");
    assert_eq!(
        off_by_one.severity, None,
        "an audit finding has no severity"
    );
}

#[tokio::test]
async fn the_checks_run_without_a_summary() {
    let w = world().await;
    let dir = use_fake(&w, Script::one(both_kinds_answer())).await;
    checks_on_open(&w, true, true).await;
    let mut watcher = watching(&w).await;
    let mut steps = watching_reviews(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    until(&mut watcher, both_done()).await;

    assert_eq!(
        agent_step(&mut steps).await,
        Some((
            StepStatus::Done,
            Some("Checking security · Auditing (6 areas)".into())
        ))
    );
    for call in calls(&dir, 2).await {
        assert!(call.argv.iter().any(|a| a == "--session-id"));
        assert!(!call.argv.iter().any(|a| a == "--fork-session"));
    }
    assert_eq!(session_of(&mut c).await, None);
}

#[tokio::test]
async fn an_area_without_blocks_is_not_checked() {
    let w = world().await;
    let dir = use_fake(
        &w,
        Script::one(Turn::answer_blocks(
            "Audit.",
            &[
                finding_block(json!({
                    "area": "correctness", "title": "Off by one",
                    "file": "feature.txt", "line": 3, "body": "b"
                })),
                pass_block(json!({"area": "concurrency", "text": "No shared state"})),
            ],
        )),
    )
    .await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(run_check(CheckKind::Audit)).await.unwrap();
    until(&mut watcher, done(CheckKind::Audit)).await;

    let checks = checks_of(&mut c).await;
    let result = checks.result(CheckKind::Audit);
    assert_eq!(
        result.areas,
        [
            "correctness",
            "concurrency",
            "error-handling",
            "performance",
            "tests",
            "docs"
        ],
        "the result remembers which areas were asked, so the others read Not checked"
    );
    assert_eq!(result.findings.len(), 1);
    assert_eq!(result.passes.len(), 1);
    let prompt = value_of(&calls(&dir, 1).await[0].argv, "-p").unwrap();
    assert!(prompt.contains("error-handling"), "{prompt}");
}

#[tokio::test]
async fn a_dismissed_finding_does_not_return() {
    let w = world().await;
    use_fake(&w, Script::one(security_answer())).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, done(CheckKind::Security)).await;
    let id = checks_of(&mut c)
        .await
        .finding(CheckKind::Security, "Token in the log")
        .id
        .clone();

    let dismiss = || Command::DismissFinding {
        pr: pr7(),
        id: id.clone(),
    };
    assert_eq!(c.request(dismiss()).await.unwrap(), Reply::Ack);
    let settled = until(&mut watcher, |e| matches!(e, Event::FindingSettled { .. })).await;
    assert!(matches!(
        settled.last(),
        Some(Event::FindingSettled { id: settled_id, accepted: false, .. }) if *settled_id == id
    ));
    assert_eq!(
        refused(c.request(dismiss()).await).code,
        ErrorCode::NotFound,
        "a finding is dismissed once"
    );

    c.request(run_check(CheckKind::Security)).await.unwrap();
    let events = until(&mut watcher, done(CheckKind::Security)).await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            Event::CheckFinding { finding, .. } if finding.id == id
        )),
        "a dismissed id is never told again"
    );
    let checks = checks_of(&mut c).await;
    assert_eq!(checks.dismissed, [id]);
    assert_eq!(checks.result(CheckKind::Security).findings.len(), 2);
}

#[tokio::test]
async fn accepting_a_finding_adds_a_draft_item_with_its_origin() {
    let w = world().await;
    use_fake(&w, Script::one(security_answer())).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, done(CheckKind::Security)).await;
    let checks = checks_of(&mut c).await;
    let accept = |finding: &Finding, body: Option<&str>| Command::AcceptFinding {
        pr: pr7(),
        id: finding.id.clone(),
        body: body.map(str::to_string),
    };

    let Reply::DraftItem(item) = c
        .request(accept(
            checks.finding(CheckKind::Security, "Token in the log"),
            None,
        ))
        .await
        .unwrap()
    else {
        panic!("a draft item");
    };
    assert_eq!(item.origin, Origin::Security);
    assert_eq!(item.kind, DraftKind::LineComment);
    assert!(item.accepted);
    assert_eq!(item.body, "Do not log the token.");
    assert_eq!(item.anchor.as_ref().map(|a| a.line), Some(2));
    let settled = until(&mut watcher, |e| matches!(e, Event::FindingSettled { .. })).await;
    assert!(matches!(
        settled.last(),
        Some(Event::FindingSettled { accepted: true, .. })
    ));
    assert_eq!(
        refused(
            c.request(accept(
                checks.finding(CheckKind::Security, "Token in the log"),
                None
            ))
            .await
        )
        .code,
        ErrorCode::NotFound,
        "two accepts add one item"
    );

    let Reply::DraftItem(range) = c
        .request(accept(
            checks.finding(CheckKind::Security, "Open range"),
            Some("Both lines need the check."),
        ))
        .await
        .unwrap()
    else {
        panic!("a draft item");
    };
    assert_eq!(range.body, "Both lines need the check.");
    let anchor = range.anchor.expect("a range keeps its anchor");
    assert_eq!((anchor.start_line, anchor.line), (Some(2), 3));

    let Reply::DraftItem(general) = c
        .request(accept(
            checks.finding(CheckKind::Security, "Loose parse"),
            None,
        ))
        .await
        .unwrap()
    else {
        panic!("a draft item");
    };
    assert_eq!(general.kind, DraftKind::General);
    assert_eq!(general.anchor, None);
    assert_eq!(
        general.body, "feature.txt:99: Line 99 is not in the diff.",
        "a comment with no anchor names its place"
    );
    assert_eq!(general.origin, Origin::Security);
    assert_eq!(checks_of(&mut c).await.accepted.len(), 3);
}

#[tokio::test]
async fn an_accept_without_a_body_adds_printable_text() {
    let w = world().await;
    use_fake(
        &w,
        Script::one(Turn::answer_blocks(
            "Two things.",
            &[
                finding_block(json!({
                    "area": "security", "severity": "high", "title": "Token in the log",
                    "file": "feature.txt", "line": 2, "body": "b",
                    "comment": "Do not\u{1b}[31m log\u{202e} it."
                })),
                finding_block(json!({
                    "area": "security", "severity": "low", "title": "Loose parse",
                    "file": "feature.txt", "line": 99, "body": "b",
                    "comment": "Line\u{1b}[2J 99\nsecond line"
                })),
            ],
        )),
    )
    .await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, done(CheckKind::Security)).await;
    let checks = checks_of(&mut c).await;
    let mut bodies = Vec::new();
    for title in ["Token in the log", "Loose parse"] {
        let Reply::DraftItem(item) = c
            .request(Command::AcceptFinding {
                pr: pr7(),
                id: checks.finding(CheckKind::Security, title).id.clone(),
                body: None,
            })
            .await
            .unwrap()
        else {
            panic!("a draft item");
        };
        bodies.push(item.body);
    }
    for body in &bodies {
        assert!(
            !body.contains('\u{1b}') && !body.contains('\u{202e}'),
            "{body:?}"
        );
    }
    assert!(bodies[0].starts_with("Do not"), "{}", bodies[0]);
    assert!(
        bodies[1].starts_with("feature.txt:99: Line"),
        "the place stays: {}",
        bodies[1]
    );
    assert!(bodies[1].contains('\n'), "line breaks stay");
}

#[tokio::test]
async fn a_new_head_makes_results_stale() {
    let w = world().await;
    use_fake(&w, Script::one(security_answer())).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, done(CheckKind::Security)).await;
    let id = checks_of(&mut c)
        .await
        .finding(CheckKind::Security, "Token in the log")
        .id
        .clone();

    let head = advance(&w).await;
    open(&mut c).await;
    until(&mut watcher, state_is(CheckKind::Security, is_stale)).await;
    let checks = checks_of(&mut c).await;
    assert_eq!(checks.state(CheckKind::Security), CheckState::Stale);
    assert_eq!(
        checks.result(CheckKind::Security).head,
        w.head,
        "the old result is still there, marked by its head"
    );
    assert_eq!(
        refused(
            c.request(Command::AcceptFinding {
                pr: pr7(),
                id,
                body: None
            })
            .await
        )
        .code,
        ErrorCode::InvalidState,
        "a finding of an older head cannot join the draft"
    );

    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, done(CheckKind::Security)).await;
    let checks = checks_of(&mut c).await;
    assert_eq!(checks.state(CheckKind::Security), CheckState::Done);
    assert_eq!(checks.result(CheckKind::Security).head, head);
}

#[tokio::test]
async fn a_reopen_with_a_new_head_checks_again_when_the_setting_is_on() {
    let w = world().await;
    let dir = use_fake(&w, Script::one(security_answer())).await;
    checks_on_open(&w, true, false).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    until(&mut watcher, done(CheckKind::Security)).await;

    open(&mut c).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        FakeClaude::calls(&dir).len(),
        1,
        "the same head has its result"
    );

    advance(&w).await;
    open(&mut c).await;
    until(&mut watcher, done(CheckKind::Security)).await;
    assert_eq!(calls(&dir, 2).await.len(), 2);
    assert_eq!(
        checks_of(&mut c).await.state(CheckKind::Security),
        CheckState::Done
    );
}

#[tokio::test]
async fn a_stopped_rerun_gives_the_previous_result_back() {
    let w = world().await;
    use_fake(&w, Script::turns(vec![security_answer(), Turn::hanging()])).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, done(CheckKind::Security)).await;

    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, state_is(CheckKind::Security, is_running)).await;
    c.request(Command::StopCheck {
        pr: pr7(),
        kind: CheckKind::Security,
    })
    .await
    .unwrap();
    let events = until(
        &mut watcher,
        state_is(CheckKind::Security, |s| *s == CheckState::Done),
    )
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::CheckDone { result, .. } if result.findings.len() == 3
        )),
        "the result comes back before the state: {events:?}"
    );
}

#[tokio::test]
async fn a_failed_check_says_why_and_can_be_tried_again() {
    let w = world().await;
    use_fake(
        &w,
        Script::turns(vec![Turn::lines(&[]).exit(1), security_answer()]),
    )
    .await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, state_is(CheckKind::Security, is_failed)).await;
    assert!(is_failed(
        &checks_of(&mut c).await.state(CheckKind::Security)
    ));

    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, done(CheckKind::Security)).await;
    assert_eq!(
        checks_of(&mut c).await.state(CheckKind::Security),
        CheckState::Done
    );
}

#[tokio::test]
async fn stopping_a_check_cancels_its_requests() {
    let w = world().await;
    use_fake(
        &w,
        Script::one(
            Turn::answer("Ran.")
                .ask_permission("Bash", json!({"command": "cargo test -p clusia-core"})),
        ),
    )
    .await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(run_check(CheckKind::Security)).await.unwrap();
    let asked = until(&mut watcher, |e| {
        matches!(e, Event::PermissionRequested { .. })
    })
    .await;
    assert!(matches!(
        asked.last(),
        Some(Event::PermissionRequested {
            origin: TurnOrigin::Security,
            ..
        })
    ));

    c.request(Command::StopCheck {
        pr: pr7(),
        kind: CheckKind::Security,
    })
    .await
    .unwrap();
    let after = until(&mut watcher, |e| {
        matches!(e, Event::PermissionResolved { .. })
    })
    .await;
    assert!(matches!(
        after.last(),
        Some(Event::PermissionResolved {
            outcome: PermissionOutcome::Cancelled,
            ..
        })
    ));
    let Reply::Permissions(waiting) = c
        .request(Command::GetPermissions { pr: pr7() })
        .await
        .unwrap()
    else {
        panic!("the waiting requests");
    };
    assert!(waiting.is_empty());
}

#[tokio::test]
async fn publish_and_discard_drop_the_results() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    use_fake(&w, Script::one(security_answer())).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(Command::AddDraftItem {
        pr: pr7(),
        kind: DraftKind::General,
        anchor: None,
        thread: None,
        body: "Looks fine".into(),
    })
    .await
    .unwrap();
    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, done(CheckKind::Security)).await;
    let file = w.daemon.paths.checks_file(&pr7());
    assert!(file.exists(), "the result is stored");

    let published = c
        .request(Command::Publish {
            pr: pr7(),
            verdict: Verdict::Comment,
            summary: "Done.".into(),
        })
        .await
        .unwrap();
    assert!(matches!(published, Reply::Published(_)), "{published:?}");
    assert!(!file.exists(), "publishing drops the result");
    let after = checks_of(&mut c).await;
    assert!(after.results.is_empty());
    assert_eq!(after.state(CheckKind::Security), CheckState::NotRun);

    open(&mut c).await;
    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, done(CheckKind::Security)).await;
    assert!(file.exists());
    assert_eq!(
        c.request(Command::DiscardReview { pr: pr7() })
            .await
            .unwrap(),
        Reply::Ack
    );
    assert!(!file.exists(), "discarding drops the result");
}

#[tokio::test]
async fn discarding_stops_a_running_check_and_cancels_its_request() {
    let w = world().await;
    let dir = use_fake(&w, Script::one(asking_and_hanging())).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(run_check(CheckKind::Audit)).await.unwrap();
    until(&mut watcher, is_request).await;
    let call = calls(&dir, 1).await.remove(0);

    assert_eq!(
        c.request(Command::DiscardReview { pr: pr7() })
            .await
            .unwrap(),
        Reply::Ack
    );
    until(&mut watcher, is_cancelled).await;
    no_request_waits(&mut c).await;
    assert!(gone(call.pid).await, "the check was stopped");
    assert!(!w.daemon.paths.checks_file(&pr7()).exists());
}

#[tokio::test]
async fn publishing_stops_a_running_check_and_cancels_its_request() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let dir = use_fake(&w, Script::one(asking_and_hanging())).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(Command::AddDraftItem {
        pr: pr7(),
        kind: DraftKind::General,
        anchor: None,
        thread: None,
        body: "Looks fine".into(),
    })
    .await
    .unwrap();
    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, is_request).await;
    let call = calls(&dir, 1).await.remove(0);

    let published = c
        .request(Command::Publish {
            pr: pr7(),
            verdict: Verdict::Comment,
            summary: "Done.".into(),
        })
        .await
        .unwrap();
    assert!(matches!(published, Reply::Published(_)), "{published:?}");
    until(&mut watcher, is_cancelled).await;
    no_request_waits(&mut c).await;
    assert!(gone(call.pid).await, "the check was stopped");
}

#[tokio::test]
async fn closing_a_review_stops_its_checks_and_keeps_the_results() {
    let w = world().await;
    let dir = use_fake(
        &w,
        Script::turns(vec![security_answer(), asking_and_hanging()]),
    )
    .await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(Command::AddDraftItem {
        pr: pr7(),
        kind: DraftKind::General,
        anchor: None,
        thread: None,
        body: "Looks fine".into(),
    })
    .await
    .unwrap();
    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, done(CheckKind::Security)).await;
    c.request(run_check(CheckKind::Audit)).await.unwrap();
    until(&mut watcher, is_request).await;
    let audit = calls(&dir, 2).await.remove(1);

    assert_eq!(
        c.request(Command::CloseReview { pr: pr7() }).await.unwrap(),
        Reply::Ack
    );
    until(&mut watcher, is_cancelled).await;
    no_request_waits(&mut c).await;
    assert!(gone(audit.pid).await, "closing stopped the audit");
    assert!(
        w.daemon.paths.review_file(&pr7()).exists(),
        "the review is saved"
    );
    let checks = checks_of(&mut c).await;
    assert_eq!(checks.state(CheckKind::Security), CheckState::Done);
    assert_eq!(checks.state(CheckKind::Audit), CheckState::NotRun);
    assert_eq!(checks.result(CheckKind::Security).findings.len(), 3);
}

#[tokio::test]
async fn closing_an_empty_review_keeps_the_results_and_the_dismissed_ids() {
    let w = world().await;
    let dir = use_fake(&w, Script::one(security_answer())).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(run_check(CheckKind::Security)).await.unwrap();
    until(&mut watcher, done(CheckKind::Security)).await;
    let id = checks_of(&mut c)
        .await
        .finding(CheckKind::Security, "Loose parse")
        .id
        .clone();
    c.request(Command::DismissFinding {
        pr: pr7(),
        id: id.clone(),
    })
    .await
    .unwrap();

    assert_eq!(
        c.request(Command::CloseReview { pr: pr7() }).await.unwrap(),
        Reply::Ack
    );
    assert!(
        !w.daemon.paths.review_file(&pr7()).exists(),
        "an empty review is not kept"
    );
    assert!(
        w.daemon.paths.checks_file(&pr7()).exists(),
        "its results stay until retention"
    );
    open(&mut c).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        FakeClaude::calls(&dir).len(),
        1,
        "the same head is not checked again"
    );
    let checks = checks_of(&mut c).await;
    assert_eq!(checks.state(CheckKind::Security), CheckState::Done);
    assert_eq!(checks.dismissed, [id]);
}

#[tokio::test]
async fn the_checks_run_when_the_summary_fails() {
    let w = world().await;
    let dir = fake_program(
        &w,
        Script::turns(vec![Turn::lines(&[]).exit(1), both_kinds_answer()]),
    )
    .await;
    checks_on_open(&w, true, true).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    until(&mut watcher, both_done()).await;

    let all = calls(&dir, 3).await;
    assert!(
        value_of(&all[0].argv, "-p")
            .unwrap()
            .starts_with("Summarize this pull request"),
        "the summary ran first, and failed"
    );
    for check in &all[1..] {
        assert!(
            check.argv.iter().any(|a| a == "--session-id")
                && !check.argv.iter().any(|a| a == "--fork-session"),
            "with no session to copy a check starts its own: {:?}",
            check.argv
        );
    }
    assert_eq!(
        checks_of(&mut c).await.state(CheckKind::Audit),
        CheckState::Done
    );
}

#[tokio::test]
async fn a_run_that_read_an_old_head_runs_again_on_the_new_one() {
    let w = world().await;
    let dir = use_fake(
        &w,
        Script::turns(vec![security_answer().delay_ms(800), security_answer()]),
    )
    .await;
    checks_on_open(&w, true, false).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    until(&mut watcher, state_is(CheckKind::Security, is_running)).await;
    let head = advance(&w).await;
    open(&mut c).await;
    let seen = std::cell::Cell::new(0);
    until(&mut watcher, |e| {
        if matches!(e, Event::CheckDone { .. }) {
            seen.set(seen.get() + 1);
        }
        seen.get() == 2
    })
    .await;
    assert_eq!(calls(&dir, 2).await.len(), 2);
    let checks = checks_of(&mut c).await;
    assert_eq!(checks.state(CheckKind::Security), CheckState::Done);
    assert_eq!(checks.result(CheckKind::Security).head, head);
}

/// The areas as a TOML inline array, which is what `set_value` parses.
fn inline_areas(areas: &[AuditArea]) -> String {
    let items: Vec<String> = areas
        .iter()
        .map(|a| {
            format!(
                r#"{{id="{}",name="{}",instruction="{}",enabled={},builtin={}}}"#,
                a.id, a.name, a.instruction, a.enabled, a.builtin
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

#[tokio::test]
async fn a_custom_area_is_asked_and_kept_and_a_switched_off_one_is_not() {
    let w = world().await;
    let dir = use_fake(
        &w,
        Script::one(Turn::answer_blocks(
            "Audit.",
            &[
                finding_block(json!({
                    "area": "accessibility", "title": "No label",
                    "file": "feature.txt", "line": 2, "body": "b", "comment": "Add a label."
                })),
                pass_block(json!({"area": "concurrency", "text": "No shared state"})),
            ],
        )),
    )
    .await;
    let mut areas = default_areas();
    areas[1].enabled = false;
    areas.push(AuditArea {
        id: "accessibility".into(),
        name: "Accessibility".into(),
        instruction: "Labels and focus order.".into(),
        enabled: true,
        builtin: false,
    });
    common::agent::set_config(&w, "harness.audit_areas", &inline_areas(&areas)).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(run_check(CheckKind::Audit)).await.unwrap();
    until(&mut watcher, done(CheckKind::Audit)).await;

    let prompt = value_of(&calls(&dir, 1).await[0].argv, "-p").unwrap();
    assert!(prompt.contains("Labels and focus order."), "{prompt}");
    assert!(
        !prompt.contains("concurrency"),
        "a switched-off area is not asked: {prompt}"
    );
    let checks = checks_of(&mut c).await;
    let result = checks.result(CheckKind::Audit);
    assert_eq!(
        result.areas,
        [
            "correctness",
            "error-handling",
            "performance",
            "tests",
            "docs",
            "accessibility"
        ]
    );
    assert_eq!(
        result.unreadable, 1,
        "the pass for the switched-off area is unreadable"
    );
    let finding = checks.finding(CheckKind::Audit, "No label");
    let Reply::DraftItem(item) = c
        .request(Command::AcceptFinding {
            pr: pr7(),
            id: finding.id.clone(),
            body: None,
        })
        .await
        .unwrap()
    else {
        panic!("a draft item");
    };
    assert_eq!(
        (item.origin, item.body.as_str()),
        (Origin::Audit, "Add a label.")
    );
}

#[tokio::test]
async fn a_shutdown_leaves_no_check_process() {
    let w = world().await;
    let dir = use_fake(&w, Script::one(Turn::hanging())).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(run_check(CheckKind::Security)).await.unwrap();
    c.request(run_check(CheckKind::Audit)).await.unwrap();
    let all = calls(&dir, 2).await;
    until(&mut watcher, state_is(CheckKind::Security, is_running)).await;

    w.daemon.stop().await;
    for call in all {
        assert!(gone(call.pid).await, "pid {} outlived the daemon", call.pid);
    }
}

#[tokio::test]
async fn a_summary_dropped_from_the_queue_releases_the_checks_that_waited_for_it() {
    let w = world().await;
    let dir = use_fake(&w, Script::turns(vec![Turn::hanging(), Turn::hanging()])).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(send("a question that runs long")).await.unwrap();
    calls(&dir, 1).await;

    // The reopen on a new head asks for a summary, which queues behind the question.
    common::agent::set_config(&w, "harness.on_open", "summarize").await;
    checks_on_open(&w, true, false).await;
    advance(&w).await;
    open(&mut c).await;
    assert_eq!(
        checks_of(&mut c).await.state(CheckKind::Security),
        CheckState::Waiting,
        "the check waits for the summary"
    );

    c.request(Command::AgentCancel { pr: pr7() }).await.unwrap();
    until(&mut watcher, state_is(CheckKind::Security, is_running)).await;
}
