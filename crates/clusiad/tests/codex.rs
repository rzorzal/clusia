mod common;

use std::time::Duration;

use clusia_core::checks::{CheckKind, CheckResult};
use clusia_core::config::HarnessKind;
use clusia_core::revalidation::{Decision, Verdict as Judged};
use clusia_core::{DraftKind, Review};
use clusia_harness::testkit::{
    CodexScript, CodexTurn, FakeCodex, Script, Turn, finding_block, revalidation_block,
};
use clusia_protocol::{
    AgentLogEntry, Client, Command, Event, PermissionAnswerKind, ProbeResult, Reply, TurnOrigin,
};
use common::agent::{calls, ready, send, set_config, until, use_fake, value_of, watching};
use common::codex::{codex_after, codex_calls, codex_gone, use_fake_codex};
use common::revalidation::{
    FILE, is_failed, is_running, push, review_of, reviews_watcher, round_is, run_round, saved,
};
use common::review_world::{open, pr7, world};
use serde_json::json;

async fn review_file(c: &mut Client) -> Review {
    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(review) => *review,
        other => panic!("expected the review file, got {other:?}"),
    }
}

fn check_done(kind: CheckKind) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::CheckDone { kind: k, .. } if *k == kind)
}

async fn results(c: &mut Client) -> Vec<CheckResult> {
    match c.request(Command::GetChecks { pr: pr7() }).await.unwrap() {
        Reply::Checks { results, .. } => results,
        other => panic!("expected the checks, got {other:?}"),
    }
}

#[tokio::test]
async fn a_codex_chat_streams_logs_and_the_next_message_reuses_its_thread() {
    let w = world().await;
    let dir = use_fake_codex(&w, CodexScript::one(CodexTurn::answer("Hello from Codex"))).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let mut watcher = watching(&w).await;

    let reply = c.request(send("What changed?")).await.unwrap();
    assert_eq!(reply, Reply::AgentTurn { turn: 1 });
    let events = until(&mut watcher, ready).await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            Event::AgentChunk { turn: 1, text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello from Codex");
    let Reply::AgentLog(entries) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap()
    else {
        panic!("an agent log");
    };
    assert!(matches!(&entries[0], AgentLogEntry::User { text, .. } if text == "What changed?"));
    assert!(matches!(&entries[1], AgentLogEntry::Text { text, .. } if text == "Hello from Codex"));
    assert!(matches!(&entries[2], AgentLogEntry::Done { turn: 1, .. }));

    let review = review_file(&mut c).await;
    assert_eq!(review.harness_session.as_deref(), Some("thr-1"));
    assert_eq!(review.harness_session_kind, Some(HarnessKind::Codex));

    c.request(send("And the tests?")).await.unwrap();
    until(&mut watcher, ready).await;
    let call = codex_after(&dir, 0, "turn/start", 2).await;
    assert_eq!(
        FakeCodex::calls(&dir).len(),
        1,
        "both messages ran in one process"
    );
    assert_eq!(
        call.methods(),
        [
            "initialize",
            "initialized",
            "thread/start",
            "turn/start",
            "turn/start"
        ]
    );
    assert_eq!(
        call.requests("turn/start")[1]["params"]["threadId"],
        "thr-1",
        "the second message goes to the thread the process already holds"
    );
    w.daemon.stop().await;
}

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
    ]
}

#[tokio::test]
async fn checks_fork_and_never_resume_the_chat_thread() {
    let w = world().await;
    let dir = use_fake_codex(
        &w,
        CodexScript::turns(vec![
            CodexTurn::answer("Chat one."),
            CodexTurn::answer_blocks("Two things.", &security_blocks()),
            CodexTurn::answer("Chat two."),
        ]),
    )
    .await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(send("hello")).await.unwrap();
    until(&mut watcher, ready).await;
    assert_eq!(
        review_file(&mut c).await.harness_session.as_deref(),
        Some("thr-1")
    );

    c.request(Command::RunCheck {
        pr: pr7(),
        kind: CheckKind::Security,
    })
    .await
    .unwrap();
    until(&mut watcher, check_done(CheckKind::Security)).await;
    let calls = codex_calls(&dir, 1).await;
    assert_eq!(calls.len(), 1, "the check shares the review's process");
    assert!(
        calls[0].requests("thread/resume").is_empty(),
        "{:?}",
        calls[0].methods()
    );
    let forks = calls[0].requests("thread/fork");
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0]["params"]["threadId"], "thr-1");
    let turns = calls[0].requests("turn/start");
    assert_eq!(
        turns[1]["params"]["threadId"], "thr-2",
        "the check runs on the copy"
    );
    assert_eq!(
        review_file(&mut c).await.harness_session.as_deref(),
        Some("thr-1"),
        "the fork never becomes the chat's thread"
    );
    let found = results(&mut c).await;
    let titles: Vec<&str> = found[0].findings.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(titles, ["Token in the log", "Loose parse"]);

    c.request(send("again")).await.unwrap();
    until(&mut watcher, ready).await;
    let call = codex_after(&dir, 0, "turn/start", 3).await;
    assert!(call.requests("thread/resume").is_empty());
    assert_eq!(
        call.requests("turn/start")[2]["params"]["threadId"],
        "thr-1",
        "the chat goes on in its own thread"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_codex_round_revalidates_the_draft_in_a_thread_of_its_own() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    let dir = use_fake_codex(
        &w,
        CodexScript::one(CodexTurn::answer_blocks(
            "Checked.",
            &[
                revalidation_block(json!({
                    "item": ids.a, "verdict": "moved", "path": FILE, "line": 1,
                    "reason": "The comment follows the same code."
                })),
                revalidation_block(json!({
                    "item": ids.b, "verdict": "obsolete", "reason": "The line was deleted."
                })),
            ],
        )),
    )
    .await;
    push(&w, &mut c, "zero\n").await;
    run_round(&w, &mut c).await;

    let round = review_of(&mut c).await.revalidation.expect("a round");
    let judged = |id: &str| {
        let proposal = round
            .proposals
            .iter()
            .find(|p| p.item == id)
            .expect("a proposal");
        assert_eq!(proposal.decision, Decision::Pending);
        proposal.verdict.clone()
    };
    assert!(matches!(judged(&ids.a), Judged::Moved { anchor, .. } if anchor.line == 1));
    assert!(matches!(judged(&ids.b), Judged::Obsolete { .. }));

    let calls = codex_calls(&dir, 1).await;
    let methods = calls[0].methods();
    assert!(methods.contains(&"thread/start".to_string()), "{methods:?}");
    assert!(
        !methods.contains(&"thread/resume".to_string()),
        "{methods:?}"
    );
    assert_eq!(
        review_of(&mut c).await.harness_session,
        None,
        "a round's thread is never the chat's"
    );
    assert!(
        codex_gone(calls[0].pid).await,
        "a round of a review that is not open leaves no process"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn reopening_a_saved_review_resumes_the_chat_thread_in_a_new_process() {
    let w = world().await;
    let dir = use_fake_codex(&w, CodexScript::one(CodexTurn::answer("ok"))).await;
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
    c.request(send("hi")).await.unwrap();
    until(&mut watcher, ready).await;
    let first = codex_calls(&dir, 1).await.remove(0);
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    assert!(codex_gone(first.pid).await);

    open(&mut c).await;
    c.request(send("and now?")).await.unwrap();
    until(&mut watcher, ready).await;
    let second = codex_after(&dir, 1, "turn/start", 1).await;
    assert_eq!(
        second.methods(),
        ["initialize", "initialized", "thread/resume", "turn/start"]
    );
    assert_eq!(
        second.requests("thread/resume")[0]["params"]["threadId"],
        "thr-1"
    );
    assert_eq!(
        review_file(&mut c).await.harness_session.as_deref(),
        Some("thr-1")
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn an_open_review_keeps_the_harness_it_started_with() {
    let w = world().await;
    let dir = use_fake(
        &w,
        Script::turns(vec![Turn::answer("One."), Turn::answer("Two.")]),
    )
    .await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(send("first")).await.unwrap();
    until(&mut watcher, ready).await;
    let session = review_file(&mut c)
        .await
        .harness_session
        .expect("a session");

    set_config(&w, "harness.kind", "codex").await;
    c.request(send("second")).await.unwrap();
    until(&mut watcher, ready).await;
    let second = calls(&dir, 2).await.remove(1);
    assert_eq!(value_of(&second.argv, "--resume"), Some(session));
    assert!(
        !w.tmp.path().join("fake-codex").exists(),
        "no Codex process was started"
    );
    let review = review_file(&mut c).await;
    assert_eq!(review.harness_session_kind, Some(HarnessKind::ClaudeCode));
    w.daemon.stop().await;
}

#[tokio::test]
async fn discarding_the_review_ends_the_codex_process() {
    let w = world().await;
    let dir = use_fake_codex(&w, CodexScript::one(CodexTurn::answer("ok"))).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(send("hi")).await.unwrap();
    until(&mut watcher, ready).await;
    let pid = codex_calls(&dir, 1).await[0].pid;
    assert!(FakeCodex::is_running(pid));

    assert_eq!(
        c.request(Command::DiscardReview { pr: pr7() })
            .await
            .unwrap(),
        Reply::Ack
    );
    assert!(codex_gone(pid).await, "the process outlived the review");
    w.daemon.stop().await;
}

#[tokio::test]
async fn closing_a_saved_review_ends_its_idle_codex_process() {
    let w = world().await;
    let dir = use_fake_codex(&w, CodexScript::one(CodexTurn::answer("ok"))).await;
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
    c.request(send("hi")).await.unwrap();
    until(&mut watcher, ready).await;
    let pid = codex_calls(&dir, 1).await[0].pid;

    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    assert!(codex_gone(pid).await, "a saved review keeps no process");
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_saved_review_closed_during_a_chat_turn_releases_its_process_when_the_turn_ends() {
    let w = world().await;
    let dir = use_fake_codex(&w, CodexScript::one(CodexTurn::hang())).await;
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
    c.request(send("hi")).await.unwrap();
    let pid = codex_after(&dir, 0, "turn/start", 1).await.pid;

    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    assert!(
        FakeCodex::is_running(pid),
        "the turn still runs, so does its process"
    );

    c.request(Command::AgentCancel { pr: pr7() }).await.unwrap();
    until(&mut watcher, ready).await;
    assert!(codex_gone(pid).await, "the closed review kept its process");
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_message_to_a_saved_review_cancels_the_close_that_waited_for_its_turn() {
    let w = world().await;
    let dir = use_fake_codex(
        &w,
        CodexScript::turns(vec![
            CodexTurn::answer("first").delay_ms(1500),
            CodexTurn::answer("second"),
        ]),
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
    c.request(send("hi")).await.unwrap();
    let pid = codex_after(&dir, 0, "turn/start", 1).await.pid;
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    c.request(send("again")).await.unwrap();

    until(&mut watcher, ready).await;
    codex_after(&dir, 0, "turn/start", 2).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        FakeCodex::is_running(pid),
        "the message brought the review back, so its process stays"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_saved_review_closed_during_a_round_releases_its_process_when_the_round_ends() {
    let w = world().await;
    let (mut c, _ids) = saved(&w).await;
    let dir = use_fake_codex(&w, CodexScript::one(CodexTurn::hang())).await;
    push(&w, &mut c, "zero\n").await;
    let mut watcher = reviews_watcher(&w).await;

    open(&mut c).await;
    until(&mut watcher, round_is(is_running)).await;
    let pid = codex_after(&dir, 0, "turn/start", 1).await.pid;

    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    assert!(
        FakeCodex::is_running(pid),
        "the round still runs, so does its process"
    );

    c.request(Command::StopRevalidation { pr: pr7() })
        .await
        .unwrap();
    until(&mut watcher, round_is(is_failed)).await;
    assert!(codex_gone(pid).await, "the closed review kept its process");
    w.daemon.stop().await;
}

#[tokio::test]
async fn the_daemon_stopping_ends_the_codex_process() {
    let w = world().await;
    let dir = use_fake_codex(&w, CodexScript::one(CodexTurn::answer("ok"))).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(send("hi")).await.unwrap();
    until(&mut watcher, ready).await;
    let pid = codex_calls(&dir, 1).await[0].pid;
    drop(c);
    drop(watcher);

    w.daemon.stop().await;
    assert!(codex_gone(pid).await, "the process outlived the daemon");
}

fn requested(e: &Event) -> bool {
    matches!(e, Event::PermissionRequested { .. })
}

#[tokio::test]
async fn a_codex_chat_asks_the_reviewer_before_it_runs_a_command() {
    let w = world().await;
    let dir = use_fake_codex(
        &w,
        CodexScript::one(CodexTurn::answer("Ran it.").ask_command("cargo test -p clusia-core")),
    )
    .await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(send("run the tests")).await.unwrap();

    let asked = until(&mut watcher, requested).await;
    let Some(Event::PermissionRequested {
        id,
        tool,
        summary,
        origin,
        sandbox,
        ..
    }) = asked.last()
    else {
        panic!("a request");
    };
    assert_eq!(
        (tool.as_str(), summary.as_str()),
        ("Bash", "cargo test -p clusia-core")
    );
    assert_eq!(*origin, TurnOrigin::Chat);
    assert!(!*sandbox);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        FakeCodex::calls(&dir)[0].decisions.is_empty(),
        "Codex waits for the reviewer"
    );

    c.request(Command::PermissionAnswer {
        id: id.clone(),
        answer: PermissionAnswerKind::Once,
    })
    .await
    .unwrap();
    until(&mut watcher, ready).await;
    let decisions = tokio::task::spawn_blocking({
        let dir = dir.clone();
        move || FakeCodex::wait_for_decisions(&dir, 1, Duration::from_secs(5))
    })
    .await
    .unwrap();
    assert_eq!(decisions, ["accept"]);
    w.daemon.stop().await;
}

async fn probe(w: &common::review_world::World) -> ProbeResult {
    match w
        .daemon
        .client()
        .await
        .request(Command::HarnessProbe)
        .await
        .unwrap()
    {
        Reply::Probe(found) => found,
        other => panic!("expected a probe, got {other:?}"),
    }
}

#[tokio::test]
async fn the_probe_reports_codex_with_its_version_and_login() {
    let w = world().await;
    let dir = use_fake_codex(&w, CodexScript::one(CodexTurn::answer("x"))).await;
    let found = probe(&w).await;
    assert!(found.ok, "{found:?}");
    assert_eq!(found.harness, "Codex");
    assert_eq!(found.version.as_deref(), Some("0.162.1"));
    assert_eq!(found.signed_in, Some(true));
    assert_eq!(found.account.as_deref(), Some("Logged in using ChatGPT"));
    assert!(found.error.is_none());
    assert!(FakeCodex::calls(&dir).is_empty(), "the probe is not a turn");
    w.daemon.stop().await;
}

#[tokio::test]
async fn probe_masks_api_keys() {
    let w = world().await;
    use_fake_codex(
        &w,
        CodexScript::one(CodexTurn::answer("x"))
            .account("Logged in using an API key - sk-proj-abcdef0123456789"),
    )
    .await;
    let found = probe(&w).await;
    assert!(found.ok, "{found:?}");
    assert_eq!(
        found.account.as_deref(),
        Some("Logged in using an API key - sk-…")
    );
    let wire = serde_json::to_string(&found).unwrap();
    assert!(!wire.contains("abcdef"), "{wire}");
    w.daemon.stop().await;
}

#[tokio::test]
async fn an_old_codex_is_refused_with_the_minimum() {
    let w = world().await;
    use_fake_codex(
        &w,
        CodexScript::one(CodexTurn::answer("x")).version("0.161.9"),
    )
    .await;
    let found = probe(&w).await;
    assert!(!found.ok);
    assert_eq!(found.version.as_deref(), Some("0.161.9"));
    assert_eq!(
        found.error.as_deref(),
        Some("Codex 0.161.9 is older than 0.162.0, which Clúsia needs")
    );
    assert_eq!(
        found.signed_in, None,
        "the login is not asked of a version it refuses"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_codex_that_is_not_signed_in_says_how_to_sign_in() {
    let w = world().await;
    use_fake_codex(&w, CodexScript::one(CodexTurn::answer("x")).signed_out()).await;
    let found = probe(&w).await;
    assert!(!found.ok);
    assert_eq!(found.signed_in, Some(false));
    assert_eq!(found.version.as_deref(), Some("0.162.1"));
    assert_eq!(
        found.error.as_deref(),
        Some("Codex is not signed in. Run `codex login` in a terminal")
    );
    assert_eq!(found.account, None);
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_missing_codex_is_not_installed_and_claude_code_keeps_its_own_probe() {
    let w = world().await;
    set_config(&w, "harness.kind", "codex").await;
    set_config(&w, "harness.program", "/nonexistent/codex").await;
    let found = probe(&w).await;
    assert!(!found.ok);
    assert_eq!(found.harness, "Codex");
    assert_eq!(
        found.error.as_deref(),
        Some("Install Codex or set its path")
    );

    set_config(&w, "harness.kind", "claude-code").await;
    set_config(&w, "harness.program", "/nonexistent/claude").await;
    let found = probe(&w).await;
    assert!(!found.ok);
    assert_eq!(found.harness, "Claude Code");
    assert_eq!(found.signed_in, None);
    assert_eq!(
        found.error.as_deref(),
        Some("Install Claude Code or set its path")
    );
    w.daemon.stop().await;
}
