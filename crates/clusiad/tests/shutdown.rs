//! A stop request lets the work already running finish before the daemon exits.

mod common;

use std::time::{Duration, Instant};

use clusia_core::{DraftKind, Side, Verdict};
use clusia_protocol::{AnchorInput, Client, ClientError, Command, Reply, topics};
use common::github_mock::{graphql_requests, mount_publish};
use common::review_world::{World, open, pr7, world};
use serde_json::json;
use tokio::task::JoinHandle;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

const REVIEW_URL: &str = "https://github.com/acme/widgets/pull/7#pullrequestreview-42";

/// GitHub accepts the submit only after `delay`, so a stop request lands mid-publish.
async fn slow_submit(w: &World, delay: Duration) {
    mount_publish(&w.server, REVIEW_URL).await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("submitPullRequestReview"))
        .respond_with(ResponseTemplate::new(200).set_delay(delay).set_body_json(
            json!({ "data": { "submitPullRequestReview": {
                    "pullRequestReview": {
                        "id": "PRR_1", "databaseId": 42, "url": REVIEW_URL, "state": "COMMENTED"
                    }
                } } }),
        ))
        .with_priority(1)
        .mount(&w.server)
        .await;
}

async fn comment_on_line_2(c: &mut Client) {
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
}

/// Publishes on `c` in the background and returns once the publish is waiting on the submit.
async fn publish_in_background(w: &World, mut c: Client) -> JoinHandle<Result<Reply, ClientError>> {
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
        w.daemon.paths.review_file(&pr7()).exists(),
        "the review is still being published"
    );
    publishing
}

#[tokio::test]
async fn shutdown_waits_for_a_publish() {
    let w = world().await;
    slow_submit(&w, Duration::from_millis(1500)).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    let publishing = publish_in_background(&w, c).await;
    let paths = w.daemon.paths.clone();

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

#[tokio::test]
async fn shutdown_waits_for_a_publish_whose_client_left() {
    let w = world().await;
    slow_submit(&w, Duration::from_secs(3)).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    c.request(Command::Subscribe {
        topics: vec![topics::CONFIG.into()],
    })
    .await
    .unwrap();
    let publishing = publish_in_background(&w, c).await;

    // The client goes away mid-publish (Ctrl-C, the app quits) …
    publishing.abort();
    let _ = publishing.await;
    // … and the next event for it finds the socket closed, which ends its connection.
    let mut writer = w.daemon.client().await;
    for theme in ["dark", "light"] {
        writer
            .request(Command::SetConfigValue {
                key: "appearance.theme".into(),
                value: theme.into(),
            })
            .await
            .unwrap();
    }
    drop(writer);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let paths = w.daemon.paths.clone();

    let _home = w.daemon.stop().await;

    assert!(
        !paths.review_file(&pr7()).exists(),
        "the daemon left before the publish was recorded"
    );
}

#[tokio::test]
async fn shutdown_takes_no_new_work_while_it_waits() {
    let w = world().await;
    slow_submit(&w, Duration::from_secs(3)).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    let mut idle = w.daemon.client().await;
    let publishing = publish_in_background(&w, c).await;
    let socket = w.daemon.paths.socket();
    let paths = w.daemon.paths.clone();

    let stopping = tokio::spawn(w.daemon.stop());
    let deadline = Instant::now() + Duration::from_secs(5);
    // A new connection is refused once the daemon stops listening.
    while Client::connect(&socket, "test").await.is_ok() {
        assert!(
            Instant::now() < deadline,
            "the daemon kept accepting connections"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // A connected client that asks for more work gets no answer: its connection is closed.
    assert!(
        matches!(
            idle.request(Command::DaemonStatus).await,
            Err(ClientError::Codec(_) | ClientError::Closed)
        ),
        "a request was served during shutdown"
    );
    assert!(
        paths.review_file(&pr7()).exists(),
        "the publish is still running"
    );

    let _home = stopping.await.unwrap();
    assert!(!paths.review_file(&pr7()).exists());
    assert!(matches!(publishing.await.unwrap(), Ok(Reply::Published(_))));
}
