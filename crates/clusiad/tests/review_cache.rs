//! The review cache lives in the daemon's memory: its file is parsed once, and what is written
//! is what a restart reads back.

mod common;

use std::path::PathBuf;

use clusia_core::DraftKind;
use clusia_protocol::{CachedReview, Command, Reply};
use clusia_store::review_cache_parses;
use common::review_world::{World, open, pr7, restart_after, world};

fn cached(reply: Reply) -> CachedReview {
    match reply {
        Reply::Cached(c) => *c,
        other => panic!("expected Cached, got {other:?}"),
    }
}

fn general(body: &str) -> Command {
    Command::AddDraftItem {
        pr: pr7(),
        kind: DraftKind::General,
        anchor: None,
        thread: None,
        body: body.into(),
    }
}

async fn restart(w: World) -> World {
    restart_after(w, |_| {}).await
}

#[tokio::test]
async fn an_open_and_the_draft_edits_after_it_never_parse_the_file_the_daemon_wrote() {
    let w = world().await;
    let paths = w.daemon.paths.clone();
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    assert_eq!(review_cache_parses(&paths, &pr7()), 0);

    c.request(general("Check the cache size")).await.unwrap();
    c.request(general("And its bound")).await.unwrap();
    c.request(Command::GetCachedReview { pr: pr7() })
        .await
        .unwrap();
    open(&mut c).await;
    assert_eq!(
        review_cache_parses(&paths, &pr7()),
        0,
        "the copy in memory serves every read"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn after_a_restart_an_open_parses_the_file_once() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    drop(c);
    let w = restart(w).await;
    let paths = w.daemon.paths.clone();
    let mut c = w.daemon.client().await;

    open(&mut c).await;
    assert_eq!(review_cache_parses(&paths, &pr7()), 1);
    c.request(general("Check the cache size")).await.unwrap();
    open(&mut c).await;
    assert_eq!(
        review_cache_parses(&paths, &pr7()),
        1,
        "a draft edit and a second open read the copy in memory"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_restart_reads_back_exactly_what_was_written() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    let view = open(&mut c).await;
    let before = cached(
        c.request(Command::GetCachedReview { pr: pr7() })
            .await
            .unwrap(),
    );
    drop(c);
    // Stopped right after the open: the writes it queued land before the daemon exits.
    let w = restart(w).await;
    let mut c = w.daemon.client().await;
    w.server.reset().await;

    let after = cached(
        c.request(Command::GetCachedReview { pr: pr7() })
            .await
            .unwrap(),
    );
    assert!(w.server.received_requests().await.unwrap().is_empty());
    assert_eq!(after.fetched_at, before.fetched_at);
    assert_eq!(after.view.pr, view.pr);
    assert_eq!(after.view.diff, view.diff);
    assert_eq!(after.view.files, view.files);
    assert_eq!(after.view.conversation, view.conversation);
    assert_eq!(after.view.checks, view.checks);
    assert_eq!(
        (after.view.role, &after.view.viewer, &after.view.worktree),
        (view.role, &view.viewer, &view.worktree)
    );
    let on_disk = clusia_store::load_review_cache(&w.daemon.paths, &pr7())
        .unwrap()
        .unwrap();
    assert_eq!(on_disk.fetched_at, before.fetched_at);
    assert_eq!(on_disk.files, view.diff);
    w.daemon.stop().await;
}

#[tokio::test]
async fn concurrent_edits_leave_the_last_draft_in_the_notes() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    let worktree = PathBuf::from(open(&mut c).await.worktree.unwrap());
    let mut edits = Vec::new();
    for i in 0..4 {
        let mut c = w.daemon.client().await;
        edits.push(tokio::spawn(async move {
            for j in 0..5 {
                c.request(general(&format!("note {i}-{j}"))).await.unwrap();
            }
        }));
    }
    for edit in edits {
        edit.await.unwrap();
    }

    let notes = std::fs::read_to_string(worktree.join(".clusia/review.md")).unwrap();
    for i in 0..4 {
        for j in 0..5 {
            assert!(notes.contains(&format!("note {i}-{j}")), "{notes}");
        }
    }
    let got = cached(
        c.request(Command::GetCachedReview { pr: pr7() })
            .await
            .unwrap(),
    );
    assert_eq!(got.view.review.draft.items.len(), 20);
    w.daemon.stop().await;
}
