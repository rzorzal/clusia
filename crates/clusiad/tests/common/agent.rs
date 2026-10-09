#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use clusia_harness::testkit::{Call, FakeClaude, Script};
use clusia_protocol::{Client, ClientError, Command, Event, Reply, SessionStateKind, topics};

use super::review_world::{World, pr7};

/// Installs a fake `claude` and points the daemon at it. The agent waits for the first question.
pub async fn use_fake(w: &World, script: Script) -> PathBuf {
    let dir = fake_program(w, script).await;
    w.daemon
        .client()
        .await
        .request(Command::SetConfigValue {
            key: "harness.on_open".into(),
            value: "wait".into(),
        })
        .await
        .unwrap();
    dir
}

/// Installs a fake `claude` and points the daemon at it, keeping the summary on open.
pub async fn fake_program(w: &World, script: Script) -> PathBuf {
    let dir = w.tmp.path().join("fake");
    std::fs::create_dir_all(&dir).unwrap();
    let program = FakeClaude::install(&dir, script);
    w.daemon
        .client()
        .await
        .request(Command::SetConfigValue {
            key: "harness.program".into(),
            value: program.display().to_string(),
        })
        .await
        .unwrap();
    dir
}

/// A client subscribed to the agent topic.
pub async fn watching(w: &World) -> Client {
    let mut c = w.daemon.client().await;
    c.request(Command::Subscribe {
        topics: vec![topics::AGENT.into()],
    })
    .await
    .unwrap();
    c
}

/// The events up to and including the first one `last` accepts.
pub async fn until(watcher: &mut Client, last: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let (_, event) = tokio::time::timeout(super::EVENT_WAIT, watcher.next_event())
            .await
            .expect("an agent event arrives")
            .expect("the connection stays open");
        let end = last(&event);
        seen.push(event);
        if end {
            return seen;
        }
    }
}

fn session_is(event: &Event, wanted: SessionStateKind) -> bool {
    matches!(event, Event::SessionState { state, .. } if *state == wanted)
}

pub fn ready(event: &Event) -> bool {
    session_is(event, SessionStateKind::Ready)
}

/// The session was ended: no turn, no queue, no slot.
pub fn session_ended(event: &Event) -> bool {
    session_is(event, SessionStateKind::None)
}

/// Waits for `count` runs of the fake without blocking the runtime the daemon runs on.
pub async fn calls(dir: &Path, count: usize) -> Vec<Call> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        FakeClaude::wait_for_calls(&dir, count, Duration::from_secs(5))
    })
    .await
    .unwrap()
}

pub fn refused(result: Result<Reply, ClientError>) -> clusia_protocol::ProtocolError {
    match result {
        Err(ClientError::Server(e)) => e,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

pub fn send(text: &str) -> Command {
    Command::AgentSend {
        pr: pr7(),
        text: text.into(),
    }
}

/// The argument that follows `flag` in a recorded command line.
pub fn value_of(argv: &[String], flag: &str) -> Option<String> {
    let at = argv.iter().position(|a| a == flag)?;
    argv.get(at + 1).cloned()
}
