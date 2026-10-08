mod common;

use clusia_harness::testkit::{FakeClaude, Script, Turn};
use clusia_protocol::{AgentErrorKind, AgentLogEntry, Command, ErrorCode, Event, Reply, topics};
use common::agent::{calls, ready, refused, send, until, use_fake, value_of, watching};
use common::review_world::{open, pr7, world};

#[tokio::test]
async fn a_message_streams_to_subscribers_and_the_log_replays_it() {
    let w = world().await;
    use_fake(&w, Script::one(Turn::answer("Hello from the agent"))).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let mut watcher = watching(&w).await;

    let reply = c
        .request(Command::AgentSend {
            pr: pr7(),
            text: "What changed?".into(),
        })
        .await
        .unwrap();
    assert_eq!(reply, Reply::AgentTurn { turn: 1 });
    let events = until(&mut watcher, ready).await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            Event::AgentChunk { turn: 1, text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello from the agent");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::AgentDone { turn: 1, .. }))
    );

    let Reply::AgentLog(entries) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap()
    else {
        panic!("an agent log");
    };
    assert!(matches!(&entries[0], AgentLogEntry::User { text, .. } if text == "What changed?"));
    assert!(
        matches!(&entries[1], AgentLogEntry::Text { text, .. } if text == "Hello from the agent")
    );
    assert!(matches!(&entries[2], AgentLogEntry::Done { turn: 1, .. }));
    assert!(w.daemon.paths.agent_log(&pr7()).exists());
}

#[tokio::test]
async fn a_message_needs_an_open_review_and_some_text() {
    let w = world().await;
    use_fake(&w, Script::one(Turn::answer("x"))).await;
    let mut c = w.daemon.client().await;
    let none = refused(
        c.request(Command::AgentSend {
            pr: pr7(),
            text: "hello".into(),
        })
        .await,
    );
    assert_eq!(none.code, ErrorCode::InvalidState);
    assert!(none.message.contains("open it first"), "{}", none.message);
    open(&mut c).await;
    let empty = refused(
        c.request(Command::AgentSend {
            pr: pr7(),
            text: "  ".into(),
        })
        .await,
    );
    assert_eq!(empty.code, ErrorCode::BadRequest);
}

#[tokio::test]
async fn a_third_message_is_busy_and_stop_ends_the_turn() {
    let w = world().await;
    let dir = use_fake(&w, Script::one(Turn::hanging())).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let mut watcher = watching(&w).await;
    let send = |text: &str| Command::AgentSend {
        pr: pr7(),
        text: text.into(),
    };
    assert_eq!(
        c.request(send("one")).await.unwrap(),
        Reply::AgentTurn { turn: 1 }
    );
    assert_eq!(
        c.request(send("two")).await.unwrap(),
        Reply::AgentTurn { turn: 2 }
    );
    let busy = refused(c.request(send("three")).await);
    assert_eq!(busy.code, ErrorCode::Busy);

    let call = calls(&dir, 1).await.remove(0);
    assert_eq!(
        c.request(Command::AgentCancel { pr: pr7() }).await.unwrap(),
        Reply::Ack
    );
    let events = until(&mut watcher, ready).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::AgentError { turn: 1, .. }))
    );
    assert!(!FakeClaude::is_running(call.pid));
    assert_eq!(
        FakeClaude::calls(&dir).len(),
        1,
        "the queued turn is dropped"
    );
}

#[tokio::test]
async fn the_probe_reports_the_program() {
    let w = world().await;
    use_fake(&w, Script::one(Turn::answer("x"))).await;
    let mut c = w.daemon.client().await;
    let Reply::Probe(probe) = c.request(Command::HarnessProbe).await.unwrap() else {
        panic!("a probe reply");
    };
    assert!(probe.ok, "{probe:?}");
    assert_eq!(probe.version.as_deref(), Some("2.1.294"));
    assert!(probe.program.ends_with("fake/claude"));
}

#[tokio::test]
async fn stopping_the_daemon_leaves_no_agent_process() {
    let w = world().await;
    let dir = use_fake(&w, Script::one(Turn::hanging().ignore_term())).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(Command::AgentSend {
        pr: pr7(),
        text: "think".into(),
    })
    .await
    .unwrap();
    let call = calls(&dir, 1).await.remove(0);
    assert!(FakeClaude::is_running(call.pid));
    w.daemon.stop().await;
    assert!(
        !FakeClaude::is_running(call.pid),
        "the agent outlived the daemon"
    );
}

#[tokio::test]
async fn a_restart_mid_turn_closes_the_turn_and_resumes_the_same_session() {
    let w = world().await;
    let script = Script::turns(vec![
        Turn::answer("one"),
        Turn::hanging(),
        Turn::answer("back"),
    ]);
    let dir = use_fake(&w, script).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let mut watcher = watching(&w).await;
    c.request(send("first")).await.unwrap();
    until(&mut watcher, ready).await;
    c.request(send("second")).await.unwrap();
    let before = calls(&dir, 2).await;
    let session = value_of(&before[0].argv, "--session-id").expect("a new session");

    drop((c, watcher));
    let home = w.daemon.stop().await;
    assert!(
        !FakeClaude::is_running(before[1].pid),
        "the turn did not outlive the daemon"
    );

    let daemon = common::TestDaemon::start_in(home).await;
    let mut c = daemon.client().await;
    c.request(Command::Subscribe {
        topics: vec![topics::AGENT.into()],
    })
    .await
    .unwrap();
    let Reply::AgentLog(entries) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap()
    else {
        panic!("an agent log");
    };
    let interrupted: Vec<(u64, String)> = entries
        .iter()
        .filter_map(|e| match e {
            AgentLogEntry::Error {
                turn,
                kind: AgentErrorKind::Interrupted,
                message,
                ..
            } => Some((*turn, message.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(interrupted.len(), 1, "{interrupted:?}");
    assert_eq!(interrupted[0].0, 2);
    assert!(
        interrupted[0].1.starts_with("The daemon stopped"),
        "{interrupted:?}"
    );

    assert_eq!(
        c.request(send("third")).await.unwrap(),
        Reply::AgentTurn { turn: 3 }
    );
    until(&mut c, ready).await;
    let third = calls(&dir, 3).await.remove(2);
    assert_eq!(value_of(&third.argv, "--resume"), Some(session));
    assert_eq!(value_of(&third.argv, "--session-id"), None);
    daemon.stop().await;
}
