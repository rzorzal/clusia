//! A stop request lets the work already running finish before the daemon exits.

mod common;

use std::time::{Duration, Instant};

use clusia_core::{DraftKind, Side, Verdict};
use clusia_protocol::{AnchorInput, Command, Reply};
use common::github_mock::{graphql_requests, mount_publish};
use common::review_world::{open, pr7, world};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

const REVIEW_URL: &str = "https://github.com/acme/widgets/pull/7#pullrequestreview-42";

#[tokio::test]
async fn shutdown_waits_for_a_publish() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    // GitHub takes a while to accept the submit, so the stop request lands mid-publish.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("submitPullRequestReview"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(1500))
                .set_body_json(json!({ "data": { "submitPullRequestReview": {
                    "pullRequestReview": {
                        "id": "PRR_1", "databaseId": 42, "url": REVIEW_URL, "state": "COMMENTED"
                    }
                } } })),
        )
        .with_priority(1)
        .mount(&w.server)
        .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
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
    let paths = w.daemon.paths.clone();

    let publishing = tokio::spawn(async move {
        c.request(Command::Publish {
            pr: pr7(),
            verdict: Verdict::Comment,
            summary: "Looks fine.".into(),
        })
        .await
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    // The pending review exists, so the publish is past its checks and waiting on the submit.
    while graphql_requests(&w.server, "addPullRequestReview(")
        .await
        .is_empty()
    {
        assert!(
            Instant::now() < deadline,
            "the publish never reached GitHub"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(
        paths.review_file(&pr7()).exists(),
        "the review is still being published"
    );
    // The folder goes away when this is dropped, so it is kept until the checks are done.
    let _home = w.daemon.stop().await;

    assert!(
        !paths.review_file(&pr7()).exists(),
        "the daemon left before the publish was recorded"
    );
    match publishing.await.unwrap().unwrap() {
        Reply::Published(result) => assert_eq!(result.url.as_deref(), Some(REVIEW_URL)),
        other => panic!("{other:?}"),
    }
}
