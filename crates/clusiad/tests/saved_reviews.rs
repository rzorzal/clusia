//! Saved reviews in the background loop: a pull request that ended is left alone.

mod common;

use std::time::{Duration, Instant};

use clusia_core::{DraftKind, PrState, Side};
use clusia_protocol::{AnchorInput, Command};
use common::review_world::{open, pr7, world_syncing};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// GitHub's answer for acme/widgets#7 once it is merged.
fn merged_pr(head: &str, base: &str, clone_url: &str) -> serde_json::Value {
    json!({
        "number": 7, "title": "Add feature", "html_url": "https://github.com/acme/widgets/pull/7",
        "user": { "login": "maria" }, "draft": false, "updated_at": "2026-10-01T12:00:00Z",
        "comments": 0, "review_comments": 0, "additions": 3, "deletions": 0, "changed_files": 1,
        "state": "closed", "merged": true,
        "base": { "ref": "main", "sha": base, "repo": { "clone_url": clone_url } },
        "head": { "ref": "feature", "sha": head, "repo": null }
    })
}

async fn requests_to(w: &common::review_world::World, url_path: &str) -> usize {
    w.server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == url_path)
        .count()
}

async fn until(what: &str, mut done: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_merged_pull_request_is_checked_once_then_left_alone() {
    let w = world_syncing().await;
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({ "total_count": 0, "incomplete_results": false, "items": [] }),
            ),
        )
        .mount(&w.server)
        .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    // A review with nothing in the draft is dropped on close; one with a comment is kept.
    c.request(Command::AddDraftItem {
        pr: pr7(),
        thread: None,
        kind: DraftKind::LineComment,
        anchor: Some(AnchorInput {
            path: "feature.txt".into(),
            line: 2,
            start_line: None,
            side: Side::Right,
        }),
        body: "rename".into(),
    })
    .await
    .unwrap();
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/7"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(merged_pr(&w.head, &w.base, &w.origin)),
        )
        .with_priority(1)
        .mount(&w.server)
        .await;

    // Resuming wakes the loop: one sync, then the saved reviews are checked.
    c.request(Command::ResumeSync).await.unwrap();
    until(
        "the merge to be recorded",
        async || match clusia_store::load_review(&w.daemon.paths, &pr7()) {
            Ok(clusia_store::ReviewLoad::Found(r)) => r.pr_state == PrState::Merged,
            _ => false,
        },
    )
    .await;

    let checked = requests_to(&w, "/repos/acme/widgets/pulls/7").await;
    let synced = requests_to(&w, "/search/issues").await;
    c.request(Command::ResumeSync).await.unwrap();
    until("another sync", async || {
        requests_to(&w, "/search/issues").await >= synced + 2
    })
    .await;
    // The check that follows a sync is local when nothing is left to ask GitHub.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        requests_to(&w, "/repos/acme/widgets/pulls/7").await,
        checked,
        "a merged pull request is not asked about again"
    );
    w.daemon.stop().await;
}
