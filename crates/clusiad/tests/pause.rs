//! Pausing background sync, and the `Stopping` event on a requested shutdown.

mod common;

use std::time::Duration;

use clusia_protocol::{Command, Event, Reply, topics};
use common::TestDaemon;

fn sync_reply(r: Reply) -> clusia_protocol::SyncStatus {
    match r {
        Reply::Sync(s) => s,
        other => panic!("expected a sync status, got {other:?}"),
    }
}

#[tokio::test]
async fn pause_and_resume_change_the_status() {
    let d = TestDaemon::start().await;
    let mut watcher = d.client().await;
    watcher
        .request(Command::Subscribe {
            topics: vec![topics::SYNC.into()],
        })
        .await
        .unwrap();
    let mut c = d.client().await;
    assert!(sync_reply(c.request(Command::PauseSync).await.unwrap()).paused);
    assert!(sync_reply(c.request(Command::GetSyncStatus).await.unwrap()).paused);
    let (_, event) = tokio::time::timeout(Duration::from_secs(5), watcher.next_event())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event, Event::SyncChanged(s) if s.paused));
    assert!(
        sync_reply(c.request(Command::SyncNow).await.unwrap()).paused,
        "a manual sync keeps the pause"
    );
    assert!(!sync_reply(c.request(Command::ResumeSync).await.unwrap()).paused);
    d.stop().await;
}

#[tokio::test]
async fn shutdown_publishes_stopping() {
    let d = TestDaemon::start().await;
    let mut watchers = Vec::new();
    for _ in 0..4 {
        let mut w = d.client().await;
        w.request(Command::Subscribe {
            topics: vec![topics::SYNC.into()],
        })
        .await
        .unwrap();
        watchers.push(w);
    }
    let mut c = d.client().await;
    assert_eq!(c.request(Command::Shutdown).await.unwrap(), Reply::Ack);
    // Every subscribed connection gets `Stopping` before its connection closes.
    for watcher in &mut watchers {
        let mut saw_stopping = false;
        while let Ok(Ok((_, event))) =
            tokio::time::timeout(Duration::from_secs(5), watcher.next_event()).await
        {
            if event == Event::Stopping {
                saw_stopping = true;
                break;
            }
        }
        assert!(saw_stopping, "a subscriber missed Stopping");
    }
    d.wait().await;
}
