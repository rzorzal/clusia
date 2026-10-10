//! "Open from cache": every successful open leaves a copy the window can show offline.

mod common;

use clusia_core::{DraftKind, Side, Verdict};
use clusia_protocol::{
    AnchorInput, CachedReview, ClientError, Command, ErrorCode, Reply, ReviewView,
};
use common::git_fixture::advance_pr;
use common::github_mock::{PrMock, mount_pr, mount_publish};
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
async fn closing_an_empty_review_keeps_the_cache() {
    let w = world().await;
    let file = w.daemon.paths.review_cache_file(&pr7());
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    assert!(
        file.exists(),
        "the pull request's data outlives the empty review"
    );

    let got = cached(
        c.request(Command::GetCachedReview { pr: pr7() })
            .await
            .unwrap(),
    );
    assert_eq!(got.view.pr.head_sha, w.head);
    assert!(got.view.review.draft.is_empty());
    w.daemon.stop().await;
}

fn count(requests: &[wiremock::Request], suffix: &str) -> usize {
    requests
        .iter()
        .filter(|r| r.method.as_str() == "GET" && r.url.path().ends_with(suffix))
        .count()
}

#[tokio::test]
async fn reopen_with_unchanged_head_skips_git_and_files() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    let first = open(&mut c).await;
    comment_on_line_2(&mut c).await;
    // A full open would fetch from the origin; with it gone, only the fast path can succeed.
    std::fs::remove_dir_all(&w.origin).unwrap();
    w.server.reset().await;
    mount_pr(&w.server, &PrMock::new(&w.head, &w.base, &w.origin)).await;

    let second = open(&mut c).await;
    let requests = w.server.received_requests().await.unwrap();
    assert_eq!(count(&requests, "/pulls/7"), 1, "one get_pr");
    assert_eq!(count(&requests, "/pulls/7/files"), 0);
    assert_eq!(
        count(&requests, "/issues/7/comments"),
        1,
        "the conversation is read again"
    );
    assert_eq!(count(&requests, "/check-runs"), 1, "so are the checks");
    assert_eq!(second.diff, first.diff);
    assert_eq!(second.worktree, first.worktree);
    assert_eq!(second.review.draft.items.len(), 1, "the draft is untouched");
    let diff = c.request(Command::GetDiff { pr: pr7() }).await.unwrap();
    assert!(
        matches!(diff, Reply::Diff(_)),
        "files_for still works: {diff:?}"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn reopen_with_new_head_takes_the_full_path() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    let new_head = advance_pr(w.tmp.path(), 7, "feature.txt", "zero\none\ntwo\nthree\n");
    w.server.reset().await;
    mount_pr(
        &w.server,
        &PrMock::new(&new_head, &w.base, &w.origin).adding_feature("zero\none\ntwo\nthree\n"),
    )
    .await;

    let view = open(&mut c).await;
    let requests = w.server.received_requests().await.unwrap();
    assert_eq!(count(&requests, "/pulls/7/files"), 1);
    assert_eq!(view.pr.head_sha, new_head);
    assert_eq!(
        view.review.head_sha, new_head,
        "the draft follows the new head"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn reopen_when_the_worktree_is_gone_takes_the_full_path() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    let first = open(&mut c).await;
    std::fs::remove_dir_all(first.worktree.as_deref().unwrap()).unwrap();
    w.server.reset().await;
    mount_pr(&w.server, &PrMock::new(&w.head, &w.base, &w.origin)).await;

    let again = open(&mut c).await;
    let requests = w.server.received_requests().await.unwrap();
    assert_eq!(count(&requests, "/pulls/7/files"), 1);
    assert!(std::path::Path::new(again.worktree.as_deref().unwrap()).exists());
    w.daemon.stop().await;
}

/// Opens again after GitHub changed to `mock`; the origin stays, so a full open can succeed.
async fn reopen_with(w: &common::review_world::World, mock: PrMock) -> (ReviewView, usize) {
    w.server.reset().await;
    mount_pr(&w.server, &mock).await;
    let mut c = w.daemon.client().await;
    let view = open(&mut c).await;
    let requests = w.server.received_requests().await.unwrap();
    (view, count(&requests, "/pulls/7/files"))
}

#[tokio::test]
async fn reopen_when_the_pull_request_closed_or_merged_takes_the_full_path() {
    let w = world().await;
    open(&mut w.daemon.client().await).await;
    let (view, files) = reopen_with(&w, PrMock::new(&w.head, &w.base, &w.origin).closed()).await;
    assert_eq!(files, 1, "closed");
    assert!(view.pr.closed && !view.pr.merged);
    let (view, files) = reopen_with(&w, PrMock::new(&w.head, &w.base, &w.origin).merged()).await;
    assert_eq!(files, 1, "merged");
    assert!(view.pr.merged);
    w.daemon.stop().await;
}

#[tokio::test]
async fn reopen_after_a_title_or_draft_change_takes_the_full_path() {
    let w = world().await;
    open(&mut w.daemon.client().await).await;
    let mock = || PrMock::new(&w.head, &w.base, &w.origin);
    let (view, files) = reopen_with(&w, mock().titled("Add the feature")).await;
    assert_eq!(files, 1, "title");
    assert_eq!(view.pr.summary.title, "Add the feature");
    let (view, files) = reopen_with(&w, mock().titled("Add the feature").drafted()).await;
    assert_eq!(files, 1, "draft");
    assert!(view.pr.summary.draft);
    w.daemon.stop().await;
}

#[tokio::test]
async fn reopen_when_the_worktree_moved_takes_the_full_path() {
    let w = world().await;
    let first = open(&mut w.daemon.client().await).await;
    let worktree = first.worktree.unwrap();
    common::git_fixture::sh(
        std::path::Path::new(&worktree),
        &["checkout", "--quiet", "--detach", &w.base],
    );
    let (view, files) = reopen_with(&w, PrMock::new(&w.head, &w.base, &w.origin)).await;
    assert_eq!(files, 1);
    let head = common::git_fixture::sh(std::path::Path::new(&worktree), &["rev-parse", "HEAD"]);
    assert_eq!(head.trim(), w.head, "the full path puts it back");
    assert_eq!(view.pr.head_sha, w.head);
    w.daemon.stop().await;
}

#[tokio::test]
async fn reopen_when_the_conversation_cannot_be_read_takes_the_full_path() {
    let w = world().await;
    open(&mut w.daemon.client().await).await;
    w.server.reset().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/repos/acme/widgets/issues/7/comments",
        ))
        .respond_with(wiremock::ResponseTemplate::new(500))
        .mount(&w.server)
        .await;
    mount_pr(&w.server, &PrMock::new(&w.head, &w.base, &w.origin)).await;
    let mut c = w.daemon.client().await;
    let result = c.request(Command::OpenReview { pr: pr7() }).await;
    let requests = w.server.received_requests().await.unwrap();
    assert_eq!(count(&requests, "/pulls/7/files"), 1, "fell through");
    assert!(result.is_err(), "the full path fails the way it always did");
    w.daemon.stop().await;
}

#[tokio::test]
async fn the_fast_path_carries_the_refreshed_conversation() {
    let w = world().await;
    let first = open(&mut w.daemon.client().await).await;
    assert!(first.conversation.unwrap().comments.is_empty());
    w.server.reset().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/repos/acme/widgets/issues/7/comments"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "id": 9, "user": { "login": "ana" }, "body": "Looks good", "created_at": "2026-10-01T12:00:00Z", "html_url": "u9" }
        ])))
        .mount(&w.server)
        .await;
    mount_pr(&w.server, &PrMock::new(&w.head, &w.base, &w.origin)).await;
    let view = open(&mut w.daemon.client().await).await;
    let requests = w.server.received_requests().await.unwrap();
    assert_eq!(count(&requests, "/pulls/7/files"), 0, "still the fast path");
    let comments = view.conversation.unwrap().comments;
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].body, "Looks good");
    let stored = clusia_store::load_review_cache(&w.daemon.paths, &pr7())
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.conversation.comments, comments,
        "the cache holds it too"
    );
    w.daemon.stop().await;
}
