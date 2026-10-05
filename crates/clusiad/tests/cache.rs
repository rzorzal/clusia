//! "Open from cache": every successful open leaves a copy the window can show offline.

mod common;

use clusia_core::{DraftKind, Side, Verdict};
use clusia_protocol::{AnchorInput, CachedReview, ClientError, Command, ErrorCode, Reply};
use common::github_mock::mount_publish;
use common::review_world::{open, pr7, world};

const REVIEW_URL: &str = "https://github.com/acme/widgets/pull/7#pullrequestreview-42";

fn cached(reply: Reply) -> CachedReview {
    match reply {
        Reply::Cached(c) => *c,
        other => panic!("expected Cached, got {other:?}"),
    }
}

async fn comment_on_line_2(c: &mut clusia_protocol::Client) {
    c.request(Command::AddDraftItem {
        pr: pr7(),
        kind: DraftKind::LineComment,
        anchor: Some(AnchorInput {
            path: "feature.txt".into(),
            line: 2,
            start_line: None,
            side: Side::Right,
        }),
        body: "rename".into(),
        thread: None,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn open_writes_the_cache() {
    let w = world().await;
    let file = w.daemon.paths.review_cache_file(&pr7());
    assert!(!file.exists());
    let mut c = w.daemon.client().await;
    let view = open(&mut c).await;

    let cache = clusia_store::load_review_cache(&w.daemon.paths, &pr7())
        .unwrap()
        .expect("a successful open writes the cache");
    assert_eq!(cache.pr, view.pr);
    assert_eq!(cache.files, view.diff);
    assert_eq!(Some(&cache.conversation), view.conversation.as_ref());
    assert_eq!(
        (
            cache.role,
            cache.viewer.as_deref(),
            cache.worktree.as_deref()
        ),
        (view.role, Some("me"), view.worktree.as_deref())
    );
    assert!(cache.fetched_at > 1_700_000_000);
    w.daemon.stop().await;
}

#[tokio::test]
async fn cached_review_needs_no_network() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    let view = open(&mut c).await;
    comment_on_line_2(&mut c).await;
    w.server.reset().await;

    let got = cached(
        c.request(Command::GetCachedReview { pr: pr7() })
            .await
            .unwrap(),
    );
    assert!(
        w.server.received_requests().await.unwrap().is_empty(),
        "GetCachedReview never calls GitHub"
    );
    assert_eq!(got.view.pr.head_sha, w.head);
    assert_eq!(got.view.files, view.files);
    assert_eq!(got.view.diff, view.diff);
    assert_eq!(got.view.conversation, view.conversation);
    assert_eq!(got.view.checks, view.checks);
    assert_eq!(
        got.view.review.draft.items.len(),
        1,
        "the draft comes from the review file, not the cache"
    );
    let stored = clusia_store::load_review_cache(&w.daemon.paths, &pr7())
        .unwrap()
        .unwrap();
    assert_eq!(got.fetched_at, stored.fetched_at);
    w.daemon.stop().await;
}

#[tokio::test]
async fn no_cache_is_not_found() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    let not_found = |r: Result<Reply, ClientError>| match r {
        Err(ClientError::Server(e)) => (e.code, e.message),
        other => panic!("expected an error, got {other:?}"),
    };
    assert_eq!(
        not_found(c.request(Command::GetCachedReview { pr: pr7() }).await),
        (
            ErrorCode::NotFound,
            "no cached copy of acme/widgets#7".into()
        )
    );
    open(&mut c).await;
    clusia_store::delete_review(&w.daemon.paths, &pr7()).unwrap();
    assert_eq!(
        not_found(c.request(Command::GetCachedReview { pr: pr7() }).await),
        (
            ErrorCode::NotFound,
            "no cached copy of acme/widgets#7".into()
        ),
        "a cache without its review file is not served"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn publish_and_discard_drop_the_cache() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let file = w.daemon.paths.review_cache_file(&pr7());
    let mut c = w.daemon.client().await;

    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    assert!(file.exists());
    c.request(Command::Publish {
        pr: pr7(),
        verdict: Verdict::Comment,
        summary: String::new(),
    })
    .await
    .unwrap();
    assert!(!file.exists(), "publish drops the cache");

    open(&mut c).await;
    assert!(file.exists());
    c.request(Command::DiscardReview { pr: pr7() })
        .await
        .unwrap();
    assert!(!file.exists(), "discard drops the cache");
    w.daemon.stop().await;
}

#[tokio::test]
async fn closing_an_empty_review_drops_the_cache() {
    let w = world().await;
    let file = w.daemon.paths.review_cache_file(&pr7());
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    assert!(file.exists());
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    assert!(!file.exists(), "a forgotten review leaves no cache behind");

    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    assert!(file.exists(), "a saved review keeps its cache");
    w.daemon.stop().await;
}
