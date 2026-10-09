//! Permission requests end to end: a fake `claude` asks through the real `clusiad
//! permission-bridge`, the daemon decides, the reviewer answers over the socket.

mod common;

use std::time::Duration;

use clusia_core::notify::OpenTarget;
use clusia_harness::testkit::{FakeClaude, Script, Turn};
use clusia_protocol::{
    AgentLogEntry, Client, Command, ErrorCode, Event, PermissionAnswerKind, PermissionOutcome,
    Reply, topics,
};
use common::agent::{calls, ready, refused, send, until, use_fake, watching};
use common::review_world::{World, open, pr7, world};
use serde_json::json;

fn run_tests() -> Turn {
    Turn::answer("Ran them.").ask_permission(
        "Bash",
        json!({"command": "cargo test -p clusia-core", "description": "run the tests"}),
    )
}

/// The first permission request announced, as (id, event).
fn requested(events: &[Event]) -> Option<(String, Event)> {
    events.iter().find_map(|event| match event {
        Event::PermissionRequested { id, .. } => Some((id.clone(), event.clone())),
        _ => None,
    })
}

async fn waiting_request(watcher: &mut Client) -> String {
    let events = until(watcher, |e| matches!(e, Event::PermissionRequested { .. })).await;
    requested(&events).expect("a request").0
}

async fn answer(client: &mut Client, id: &str, answer: PermissionAnswerKind) {
    let reply = client
        .request(Command::PermissionAnswer {
            id: id.to_string(),
            answer,
        })
        .await
        .unwrap();
    assert_eq!(reply, Reply::Ack);
}

fn resolutions(events: &[Event]) -> Vec<PermissionOutcome> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::PermissionResolved { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .collect()
}

async fn log_of(c: &mut Client) -> Vec<AgentLogEntry> {
    match c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap() {
        Reply::AgentLog(entries) => entries,
        other => panic!("an agent log, got {other:?}"),
    }
}

/// A world whose agent waits for the first question, and a client that holds the review.
async fn started(script: Script) -> (World, std::path::PathBuf, Client, Client) {
    let w = world().await;
    let dir = use_fake(&w, script).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let watcher = watching(&w).await;
    (w, dir, c, watcher)
}

#[tokio::test]
async fn once_allows_the_request_and_the_turn_goes_on() {
    let (w, dir, mut c, mut watcher) = started(Script::one(run_tests())).await;
    c.request(send("run the tests")).await.unwrap();
    let events = until(&mut watcher, |e| {
        matches!(e, Event::PermissionRequested { .. })
    })
    .await;
    let (id, request) = requested(&events).unwrap();
    let Event::PermissionRequested {
        pr,
        turn,
        tool,
        summary,
        reason,
        prefix,
        sandbox,
        ..
    } = request
    else {
        unreachable!()
    };
    assert_eq!((pr, turn, tool.as_str()), (pr7(), 1, "Bash"));
    assert_eq!(summary, "cargo test -p clusia-core");
    assert_eq!(reason.as_deref(), Some("run the tests"));
    assert_eq!(prefix.as_deref(), Some("cargo test"));
    assert!(sandbox);

    // Another window answers; the first connection only watches.
    let mut other = w.daemon.client().await;
    answer(&mut other, &id, PermissionAnswerKind::Once).await;
    let events = until(&mut watcher, ready).await;
    assert_eq!(resolutions(&events), [PermissionOutcome::Allowed]);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::AgentDone { turn: 1, .. }))
    );

    let answers = FakeClaude::permission_answers(&dir);
    assert_eq!(answers.len(), 1);
    assert!(answers[0].allow);
    assert_eq!(answers[0].tool, "Bash");
    let log = log_of(&mut c).await;
    assert!(log.iter().any(|e| matches!(
        e,
        AgentLogEntry::Permission { turn: 1, outcome: PermissionOutcome::Allowed, summary, .. }
            if summary == "cargo test -p clusia-core"
    )));
}

#[tokio::test]
async fn allowing_for_the_review_covers_the_rest_of_the_review_but_never_the_command_line() {
    let first = Turn::answer("Ran them.")
        .ask_permission("Bash", json!({"command": "cargo test -p clusia-core"}))
        .ask_permission("Bash", json!({"command": "cargo test --workspace"}));
    let script = Script::turns(vec![
        first,
        Turn::answer("Again.").ask_permission("Bash", json!({"command": "cargo test"})),
        Turn::answer("Once more.").ask_permission("Bash", json!({"command": "cargo test"})),
    ]);
    let (w, dir, mut c, mut watcher) = started(script).await;

    c.request(send("run the tests")).await.unwrap();
    let id = waiting_request(&mut watcher).await;
    let mut other = w.daemon.client().await;
    answer(&mut other, &id, PermissionAnswerKind::Review).await;
    let events = until(&mut watcher, ready).await;
    assert_eq!(
        requested(&events),
        None,
        "the second command was covered, so it asked nothing"
    );
    assert_eq!(
        resolutions(&events),
        [
            PermissionOutcome::AllowedForReview,
            PermissionOutcome::AllowedForReview
        ],
        "and the chat still hears of it"
    );
    let answers = FakeClaude::permission_answers(&dir);
    assert_eq!(
        answers.iter().map(|a| a.allow).collect::<Vec<_>>(),
        [true, true]
    );

    assert_eq!(
        c.request(Command::GetRules { pr: pr7() }).await.unwrap(),
        Reply::Rules(vec!["Bash(cargo test:*)".into()])
    );
    // The next turn is covered too, by the daemon: the program still asks, nobody is asked.
    c.request(send("again")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert_eq!(requested(&events), None);
    assert_eq!(resolutions(&events), [PermissionOutcome::AllowedForReview]);

    // Revoked, the next turn no longer carries it.
    assert_eq!(
        c.request(Command::RevokeRule {
            pr: pr7(),
            rule: "Bash(cargo test:*)".into()
        })
        .await
        .unwrap(),
        Reply::Ack
    );
    assert_eq!(
        c.request(Command::GetRules { pr: pr7() }).await.unwrap(),
        Reply::Rules(vec![])
    );
    c.request(send("once more")).await.unwrap();
    let id = waiting_request(&mut watcher).await;
    answer(&mut other, &id, PermissionAnswerKind::Deny).await;
    until(&mut watcher, ready).await;
    assert_eq!(
        FakeClaude::permission_answers(&dir)
            .iter()
            .map(|a| a.allow)
            .collect::<Vec<_>>(),
        [true, true, true, false]
    );
    // A rule is never handed to the program, which would skip the daemon's checks.
    for n in 0..3 {
        assert!(
            !calls_argv(&dir, n).iter().any(|a| a.starts_with("Bash(")),
            "turn {n}"
        );
    }
}

fn calls_argv(dir: &std::path::Path, n: usize) -> Vec<String> {
    FakeClaude::calls(dir)[n].argv.clone()
}

#[tokio::test]
async fn a_denial_reaches_the_agent_and_the_chat_once() {
    let (_w, dir, mut c, mut watcher) = started(Script::one(run_tests())).await;
    c.request(send("run the tests")).await.unwrap();
    let id = waiting_request(&mut watcher).await;
    answer(&mut c, &id, PermissionAnswerKind::Deny).await;
    let events = until(&mut watcher, ready).await;
    assert_eq!(resolutions(&events), [PermissionOutcome::Denied]);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::AgentDenied { .. })),
        "the program's own report of the denial is not told again: {events:?}"
    );
    let answers = FakeClaude::permission_answers(&dir);
    assert!(!answers[0].allow);
    assert_eq!(
        answers[0].message.as_deref(),
        Some("The reviewer denied this request.")
    );
    let log = log_of(&mut c).await;
    assert!(log.iter().any(|e| matches!(
        e,
        AgentLogEntry::Permission {
            outcome: PermissionOutcome::Denied,
            ..
        }
    )));
    assert!(
        !log.iter()
            .any(|e| matches!(e, AgentLogEntry::Denied { .. }))
    );
}

#[tokio::test]
async fn stop_with_a_request_pending_cancels_it() {
    let (_w, dir, mut c, mut watcher) = started(Script::one(run_tests())).await;
    c.request(send("run the tests")).await.unwrap();
    let id = waiting_request(&mut watcher).await;
    assert_eq!(
        c.request(Command::AgentCancel { pr: pr7() }).await.unwrap(),
        Reply::Ack
    );
    let events = until(&mut watcher, ready).await;
    assert_eq!(resolutions(&events), [PermissionOutcome::Cancelled]);
    let call = calls(&dir, 1).await.remove(0);
    assert!(!FakeClaude::is_running(call.pid));
    // The request is gone: an answer that comes late changes nothing.
    let late = refused(
        c.request(Command::PermissionAnswer {
            id,
            answer: PermissionAnswerKind::Once,
        })
        .await,
    );
    assert_eq!(late.code, ErrorCode::NotFound);
    assert!(
        FakeClaude::permission_answers(&dir)
            .iter()
            .all(|a| !a.allow)
    );
}

#[tokio::test]
async fn the_first_of_two_windows_wins() {
    let (w, dir, mut c, mut watcher) = started(Script::one(run_tests())).await;
    c.request(send("run the tests")).await.unwrap();
    let id = waiting_request(&mut watcher).await;
    let mut second = w.daemon.client().await;
    answer(&mut c, &id, PermissionAnswerKind::Deny).await;
    let slow = refused(
        second
            .request(Command::PermissionAnswer {
                id,
                answer: PermissionAnswerKind::Once,
            })
            .await,
    );
    assert_eq!(slow.code, ErrorCode::NotFound);
    until(&mut watcher, ready).await;
    assert!(!FakeClaude::permission_answers(&dir)[0].allow);
}

#[tokio::test]
async fn a_window_that_connects_late_finds_the_request_and_answers_it() {
    let (w, dir, mut c, mut watcher) = started(Script::one(run_tests())).await;
    c.request(send("run the tests")).await.unwrap();
    let id = waiting_request(&mut watcher).await;
    // This window was not listening when the request was announced.
    let mut late = w.daemon.client().await;
    let Reply::Permissions(waiting) = late
        .request(Command::GetPermissions { pr: pr7() })
        .await
        .unwrap()
    else {
        panic!("a list of requests");
    };
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].id, id);
    assert_eq!(waiting[0].summary, "cargo test -p clusia-core");
    assert_eq!(waiting[0].prefix.as_deref(), Some("cargo test"));
    assert!(waiting[0].sandbox);
    answer(&mut late, &waiting[0].id, PermissionAnswerKind::Once).await;
    until(&mut watcher, ready).await;
    assert!(FakeClaude::permission_answers(&dir)[0].allow);
    assert_eq!(
        late.request(Command::GetPermissions { pr: pr7() })
            .await
            .unwrap(),
        Reply::Permissions(vec![])
    );
}

#[tokio::test]
async fn the_agent_may_wait_longer_than_its_own_tool_timeout() {
    let (_w, dir, mut c, mut watcher) = started(Script::one(Turn::answer("x"))).await;
    c.request(send("hello")).await.unwrap();
    until(&mut watcher, ready).await;
    let env = &FakeClaude::calls(&dir)[0].env;
    // The deadline is 120 s by default; the program waits 30 s more.
    assert_eq!(
        env.get("MCP_TOOL_TIMEOUT").map(String::as_str),
        Some("150000")
    );
}

#[tokio::test]
async fn the_daemon_refuses_an_ask_for_a_turn_that_is_not_running() {
    let (_w, _dir, mut c, _watcher) = started(Script::one(Turn::answer("x"))).await;
    let refusal = refused(
        c.request(Command::PermissionAsk {
            pr: pr7(),
            turn: 9,
            tool: "Bash".into(),
            input: json!({"command": "ls"}),
        })
        .await,
    );
    assert_eq!(refusal.code, ErrorCode::InvalidState);
    assert!(
        refusal.message.contains("not running"),
        "{}",
        refusal.message
    );
}

#[tokio::test]
async fn an_edit_outside_the_worktree_is_denied_with_no_prompt() {
    let turn = Turn::answer("Tried.").ask_permission(
        "Edit",
        json!({"file_path": "/etc/hosts", "old_string": "a", "new_string": "b"}),
    );
    let (_w, dir, mut c, mut watcher) = started(Script::one(turn)).await;
    c.request(send("change it")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert_eq!(requested(&events), None);
    assert!(events.iter().any(|e| matches!(
        e,
        Event::PermissionResolved { tool, summary, outcome: PermissionOutcome::Denied, .. }
            if tool == "Edit" && summary == "/etc/hosts"
    )));
    let answers = FakeClaude::permission_answers(&dir);
    assert!(!answers[0].allow);
    assert!(answers[0].message.as_deref().unwrap().contains("outside"));
    let log = log_of(&mut c).await;
    assert!(log.iter().any(|e| matches!(
        e,
        AgentLogEntry::Permission { tool, outcome: PermissionOutcome::Denied, .. } if tool == "Edit"
    )));
}

#[tokio::test]
async fn the_tray_is_told_when_no_window_holds_the_review() {
    let w = world().await;
    use_fake(&w, Script::one(run_tests())).await;
    let mut opener = w.daemon.client().await;
    open(&mut opener).await;
    let mut watcher = w.daemon.client().await;
    let connected = clients(&mut watcher).await;
    // The window that opened it goes away: nothing holds the review any more.
    drop(opener);
    let deadline = std::time::Instant::now() + common::EVENT_WAIT;
    while clients(&mut watcher).await >= connected {
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon notices the window left"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    watcher
        .request(Command::Subscribe {
            topics: vec![topics::AGENT.into(), topics::TRAY.into()],
        })
        .await
        .unwrap();
    let mut asker = w.daemon.client().await;
    asker.request(send("run the tests")).await.unwrap();
    let events = until(&mut watcher, |e| {
        matches!(e, Event::PermissionRequested { .. })
    })
    .await;
    let id = requested(&events).unwrap().0;
    let banner = loop {
        let (topic, event) = tokio::time::timeout(common::EVENT_WAIT, watcher.next_event())
            .await
            .expect("a banner arrives")
            .unwrap();
        if topic == topics::TRAY
            && let Event::Notify {
                title,
                time_sensitive,
                sound,
                open,
                ..
            } = event
        {
            break (title, time_sensitive, sound, open);
        }
    };
    assert_eq!(banner.0, "Claude Code needs your permission on #7");
    assert!(banner.1, "it bypasses quiet hours");
    assert!(banner.2.is_some(), "it makes a sound by default");
    assert_eq!(
        banner.3,
        OpenTarget::Review {
            pr: pr7(),
            thread: None
        }
    );
    answer(&mut asker, &id, PermissionAnswerKind::Deny).await;
    until(&mut watcher, ready).await;
}

#[tokio::test]
async fn stopping_the_daemon_does_not_wait_out_a_request() {
    let (w, dir, mut c, mut watcher) = started(Script::one(run_tests())).await;
    c.request(send("run the tests")).await.unwrap();
    waiting_request(&mut watcher).await;
    let log = w.daemon.paths.agent_log(&pr7());
    let started = std::time::Instant::now();
    let _home = tokio::time::timeout(Duration::from_secs(20), w.daemon.stop())
        .await
        .expect("the daemon stops");
    // Well under the grace the daemon gives its work before it kills what is left.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the request was denied, not waited for: {:?}",
        started.elapsed()
    );
    // The program is killed right after, so it may not get to read the denial; the log
    // says how the request ended.
    let text = std::fs::read_to_string(&log).unwrap();
    let ended: Vec<PermissionOutcome> = text
        .lines()
        .filter_map(|line| match serde_json::from_str(line) {
            Ok(AgentLogEntry::Permission { outcome, .. }) => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(ended, [PermissionOutcome::Cancelled]);
    assert!(
        FakeClaude::permission_answers(&dir).iter().all(|a| !a.allow
            && a.message.as_deref() == Some("The request was cancelled because the turn ended.")),
        "whatever the agent heard was the cancellation"
    );
    let call = FakeClaude::calls(&dir).remove(0);
    assert!(!FakeClaude::is_running(call.pid));
}

/// How many clients the daemon counts as connected.
async fn clients(c: &mut Client) -> usize {
    match c.request(Command::DaemonStatus).await.unwrap() {
        Reply::Status(status) => status.clients,
        other => panic!("a status, got {other:?}"),
    }
}
