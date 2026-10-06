//! Replies, resolves and the GraphQL publish sequence: one review with line comments,
//! replies and the summary, then the resolves; nothing half-done stays on GitHub.

mod common;

use clusia_core::{ChecksSummary, DraftKind, ReviewState, Side, ThreadRef, Verdict};
use clusia_protocol::{AnchorInput, Client, ClientError, Command, ErrorCode, PublishResult, Reply};
use common::github_mock::{PrMock, graphql_error, mount_pr, mount_publish};
use common::review_world::{World, open, pr7, world};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

const REVIEW_URL: &str = "https://github.com/acme/widgets/pull/7#pullrequestreview-42";

fn thread(id: &str, author: &str) -> ThreadRef {
    ThreadRef {
        id: id.into(),
        author: author.into(),
        path: Some("feature.txt".into()),
        line: Some(1),
    }
}

fn add(
    kind: DraftKind,
    anchor: Option<AnchorInput>,
    thread: Option<ThreadRef>,
    body: &str,
) -> Command {
    Command::AddDraftItem {
        pr: pr7(),
        kind,
        anchor,
        body: body.into(),
        thread,
    }
}

async fn comment_on_line_2(c: &mut Client) {
    let anchor = Some(AnchorInput {
        path: "feature.txt".into(),
        line: 2,
        start_line: None,
        side: Side::Right,
    });
    c.request(add(DraftKind::LineComment, anchor, None, "rename"))
        .await
        .unwrap();
}

async fn reply_to(c: &mut Client, id: &str, body: &str) {
    c.request(add(DraftKind::Reply, None, Some(thread(id, "mona")), body))
        .await
        .unwrap();
}

async fn resolve(c: &mut Client, id: &str) {
    c.request(add(DraftKind::Resolve, None, Some(thread(id, "mona")), ""))
        .await
        .unwrap();
}

fn publish(verdict: Verdict, summary: &str) -> Command {
    Command::Publish {
        pr: pr7(),
        verdict,
        summary: summary.into(),
    }
}

fn server_error(r: Result<Reply, ClientError>) -> clusia_protocol::ProtocolError {
    match r {
        Err(ClientError::Server(e)) => e,
        other => panic!("expected an error, got {other:?}"),
    }
}

/// The GraphQL writes GitHub received, in order, with their request bodies.
async fn writes(w: &World) -> Vec<(&'static str, String)> {
    let requests = w.server.received_requests().await.unwrap();
    requests
        .iter()
        .filter(|r| r.url.path() == "/graphql")
        .filter_map(|r| {
            let body = String::from_utf8_lossy(&r.body).into_owned();
            // The reply mutation's name starts with the start mutation's: test it first.
            let op = [
                ("addPullRequestReviewThreadReply", "reply"),
                ("addPullRequestReview(", "start"),
                ("submitPullRequestReview", "submit"),
                ("resolveReviewThread", "resolve"),
                ("deletePullRequestReview", "delete"),
            ]
            .into_iter()
            .find(|(needle, _)| body.contains(needle))?
            .1;
            Some((op, body))
        })
        .collect()
}

fn ops(writes: &[(&'static str, String)]) -> Vec<&'static str> {
    writes.iter().map(|(op, _)| *op).collect()
}

fn stored(w: &World) -> clusia_core::Review {
    match clusia_store::load_review(&w.daemon.paths, &pr7()).unwrap() {
        clusia_store::ReviewLoad::Found(r) => r,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn reply_and_resolve_items_round_trip() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;

    let reply = match c
        .request(add(
            DraftKind::Reply,
            None,
            Some(thread("PRRT_a", "mona")),
            " Agreed, 30 seconds. ",
        ))
        .await
        .unwrap()
    {
        Reply::DraftItem(i) => i,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        (
            reply.kind,
            reply.body.as_str(),
            reply.thread.as_ref().unwrap().id.as_str()
        ),
        (DraftKind::Reply, "Agreed, 30 seconds.", "PRRT_a")
    );
    let resolve_item = match c
        .request(add(
            DraftKind::Resolve,
            None,
            Some(thread("PRRT_a", "mona")),
            "ignored",
        ))
        .await
        .unwrap()
    {
        Reply::DraftItem(i) => i,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        (resolve_item.kind, resolve_item.body.as_str()),
        (DraftKind::Resolve, "")
    );

    let again = server_error(
        c.request(add(
            DraftKind::Resolve,
            None,
            Some(thread("PRRT_a", "mona")),
            "",
        ))
        .await,
    );
    assert_eq!(
        (again.code, again.message.as_str()),
        (
            ErrorCode::BadRequest,
            "this thread is already marked to resolve"
        )
    );
    let no_thread = server_error(c.request(add(DraftKind::Reply, None, None, "x")).await);
    assert_eq!(
        (no_thread.code, no_thread.message.as_str()),
        (ErrorCode::BadRequest, "a reply or resolve needs a thread")
    );
    let edit = server_error(
        c.request(Command::UpdateDraftItem {
            pr: pr7(),
            id: resolve_item.id.clone(),
            body: "text".into(),
        })
        .await,
    );
    assert_eq!(
        (edit.code, edit.message.as_str()),
        (ErrorCode::BadRequest, "a resolve has no text to edit")
    );

    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(r) => {
            let threads: Vec<_> = r
                .draft
                .items
                .iter()
                .map(|i| (i.kind, i.thread.clone().unwrap().id))
                .collect();
            assert_eq!(
                threads,
                [
                    (DraftKind::Reply, "PRRT_a".to_string()),
                    (DraftKind::Resolve, "PRRT_a".to_string())
                ]
            );
        }
        other => panic!("{other:?}"),
    }
    w.daemon.stop().await;
}

#[tokio::test]
async fn publishes_one_review_with_replies_then_resolves() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    reply_to(&mut c, "PRRT_a", "Agreed, 30 seconds.").await;
    resolve(&mut c, "PRRT_b").await;

    let reply = c
        .request(publish(Verdict::RequestChanges, "Please rename."))
        .await
        .unwrap();
    assert_eq!(
        reply,
        Reply::Published(PublishResult {
            url: Some(REVIEW_URL.into()),
            closed: false,
            unresolved: vec![],
            close_error: None,
        })
    );
    let sent = writes(&w).await;
    assert_eq!(ops(&sent), ["start", "reply", "submit", "resolve"]);
    let [(_, start), (_, reply), (_, submit), (_, resolve)] = &sent[..] else {
        unreachable!()
    };
    assert!(start.contains("PR_7") && start.contains(&w.head), "{start}");
    assert!(
        start.contains("feature.txt") && start.contains("rename"),
        "{start}"
    );
    assert!(
        reply.contains("PRR_1") && reply.contains("PRRT_a"),
        "{reply}"
    );
    assert!(reply.contains("Agreed, 30 seconds."), "{reply}");
    assert!(
        submit.contains("PRR_1") && submit.contains("REQUEST_CHANGES"),
        "{submit}"
    );
    assert!(submit.contains("Please rename."), "{submit}");
    assert!(resolve.contains("PRRT_b"), "{resolve}");
    assert!(!w.daemon.paths.review_file(&pr7()).exists());
    w.daemon.stop().await;
}

#[tokio::test]
async fn pending_review_on_github_blocks_publishing() {
    let w = world().await;
    w.server.reset().await;
    let mut mock = PrMock::new(&w.head, &w.base, &w.origin);
    let pending_url = "https://github.com/acme/widgets/pull/7#pullrequestreview-9";
    mock.pending = Some(("PRR_web".into(), pending_url.into()));
    mount_pr(&w.server, &mock).await;
    mount_publish(&w.server, REVIEW_URL).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;

    let e = server_error(c.request(publish(Verdict::Comment, "")).await);
    assert_eq!(e.code, ErrorCode::InvalidState);
    assert_eq!(
        e.message,
        format!(
            "You already have a pending review on GitHub for this pull request. Submit or discard it there, then publish again: {pending_url}"
        )
    );
    assert!(
        writes(&w).await.is_empty(),
        "nothing written, nothing deleted"
    );
    let review = stored(&w);
    assert_eq!(
        (review.state, review.draft.items.len()),
        (ReviewState::Active, 1)
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn reply_failure_deletes_the_pending_review_and_keeps_the_draft() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    graphql_error(
        &w.server,
        "addPullRequestReviewThreadReply",
        "Could not resolve to a node with the global id of 'PRRT_gone'",
    )
    .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    reply_to(&mut c, "PRRT_gone", "Still relevant?").await;

    let e = server_error(c.request(publish(Verdict::Comment, "")).await);
    assert_eq!(e.code, ErrorCode::Upstream);
    assert!(e.message.contains("PRRT_gone"), "{}", e.message);
    let sent = writes(&w).await;
    assert_eq!(ops(&sent), ["start", "reply", "delete"]);
    assert!(sent[2].1.contains("PRR_1"), "the pending review is deleted");
    let review = stored(&w);
    assert_eq!(
        (review.state, review.draft.items.len()),
        (ReviewState::Saved, 2)
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn submit_failure_keeps_the_draft() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    graphql_error(
        &w.server,
        "submitPullRequestReview",
        "Review cannot be submitted",
    )
    .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    resolve(&mut c, "PRRT_b").await;

    let e = server_error(c.request(publish(Verdict::Approve, "Ship it")).await);
    assert_eq!(e.code, ErrorCode::Upstream);
    assert!(
        e.message.contains("Review cannot be submitted"),
        "{}",
        e.message
    );
    assert_eq!(ops(&writes(&w).await), ["start", "submit", "delete"]);
    let review = stored(&w);
    assert_eq!(
        (review.state, review.draft.items.len()),
        (ReviewState::Saved, 2)
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn resolve_failure_still_publishes_and_reports() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    graphql_error(
        &w.server,
        "PRRT_locked",
        "Resource not accessible by integration",
    )
    .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    resolve(&mut c, "PRRT_b").await;
    resolve(&mut c, "PRRT_locked").await;

    let reply = c.request(publish(Verdict::Comment, "")).await.unwrap();
    assert_eq!(
        reply,
        Reply::Published(PublishResult {
            url: Some(REVIEW_URL.into()),
            closed: false,
            unresolved: vec!["PRRT_locked".into()],
            close_error: None,
        })
    );
    assert_eq!(
        ops(&writes(&w).await),
        ["start", "submit", "resolve", "resolve"]
    );
    assert!(!w.daemon.paths.review_file(&pr7()).exists());
    w.daemon.stop().await;
}

#[tokio::test]
async fn resolve_only_publish() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    resolve(&mut c, "PRRT_b").await;

    let reply = c.request(publish(Verdict::Comment, "")).await.unwrap();
    assert_eq!(
        reply,
        Reply::Published(PublishResult {
            url: None,
            closed: false,
            unresolved: vec![],
            close_error: None,
        })
    );
    assert_eq!(ops(&writes(&w).await), ["resolve"], "no review is created");
    assert!(!w.daemon.paths.review_file(&pr7()).exists());
    let (activity, _) = clusia_store::read_activity(&w.daemon.paths).unwrap();
    assert!(
        activity
            .iter()
            .any(|a| a.kind == clusia_core::ActivityKind::ReviewPublished)
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn resolve_only_then_close_failure_reports_both() {
    let w = world().await;
    w.server.reset().await;
    let mut mock = PrMock::new(&w.head, &w.base, &w.origin);
    mock.viewer = "maria".into();
    mount_pr(&w.server, &mock).await;
    mount_publish(&w.server, REVIEW_URL).await;
    graphql_error(
        &w.server,
        "PRRT_locked",
        "Resource not accessible by integration",
    )
    .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/acme/widgets/pulls/7"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({ "message": "boom" })))
        .mount(&w.server)
        .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    resolve(&mut c, "PRRT_b").await;
    resolve(&mut c, "PRRT_locked").await;

    let reply = c.request(publish(Verdict::ClosePr, "")).await.unwrap();
    let Reply::Published(result) = reply else {
        panic!("published: {reply:?}")
    };
    assert_eq!(
        (result.url, result.closed, result.unresolved),
        (None, false, vec!["PRRT_locked".to_string()])
    );
    let error = result.close_error.expect("why closing failed");
    assert!(error.contains("boom"), "{error}");
    assert!(!w.daemon.paths.review_file(&pr7()).exists());
    w.daemon.stop().await;
}

#[tokio::test]
async fn resolve_only_failure_keeps_the_draft() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    graphql_error(
        &w.server,
        "resolveReviewThread",
        "Resource not accessible by integration",
    )
    .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    resolve(&mut c, "PRRT_b").await;

    let e = server_error(c.request(publish(Verdict::Comment, "")).await);
    assert_eq!(e.code, ErrorCode::Upstream);
    assert!(
        e.message.contains("Resource not accessible"),
        "{}",
        e.message
    );
    let review = stored(&w);
    assert_eq!(
        (review.state, review.draft.items.len()),
        (ReviewState::Saved, 1)
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn open_view_carries_checks_and_conversation() {
    let w = world().await;
    w.server.reset().await;
    let mut mock = PrMock::new(&w.head, &w.base, &w.origin);
    mock.threads = json!([{
        "id": "PRRT_a", "isResolved": false, "isOutdated": false, "path": "feature.txt",
        "line": 2, "startLine": null, "diffSide": "RIGHT",
        "viewerCanReply": true, "viewerCanResolve": true,
        "comments": { "nodes": [{
            "databaseId": 11, "author": { "login": "mona" }, "body": "Why two?",
            "createdAt": "2026-10-01T12:00:00Z",
            "url": "https://github.com/acme/widgets/pull/7#discussion_r11"
        }] }
    }]);
    mount_pr(&w.server, &mock).await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/acme/widgets/commits/{}/check-runs",
            w.head
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count": 2,
            "check_runs": [
                { "status": "completed", "conclusion": "success" },
                { "status": "in_progress", "conclusion": null }
            ]
        })))
        .with_priority(1)
        .mount(&w.server)
        .await;
    let mut c = w.daemon.client().await;
    let view = open(&mut c).await;

    assert_eq!(
        view.checks,
        Some(ChecksSummary {
            total: 2,
            passed: 1,
            failed: 0,
            pending: 1
        })
    );
    let conversation = view
        .conversation
        .expect("the conversation comes with the view");
    let [t] = &conversation.review_threads[..] else {
        panic!("{:?}", conversation.review_threads)
    };
    assert_eq!(
        (t.id.as_str(), t.line, t.side),
        ("PRRT_a", Some(2), Side::Right)
    );
    assert!(t.viewer_can_resolve);
    assert_eq!(
        (t.comments[0].author.as_str(), t.comments[0].body.as_str()),
        ("mona", "Why two?")
    );
    let [file] = &view.diff[..] else {
        panic!("{:?}", view.diff)
    };
    assert_eq!(file.path, "feature.txt");
    assert!(
        file.patch
            .as_deref()
            .unwrap()
            .starts_with("@@ -0,0 +1,3 @@")
    );
    assert_eq!(view.files.len(), 1, "the summaries stay for the CLI");
    w.daemon.stop().await;
}

#[tokio::test]
async fn checks_failure_does_not_fail_the_open() {
    let w = world().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/acme/widgets/commits/{}/check-runs",
            w.head
        )))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({ "message": "boom" })))
        .with_priority(1)
        .mount(&w.server)
        .await;
    let mut c = w.daemon.client().await;
    let view = open(&mut c).await;
    assert_eq!(view.checks, None);
    assert!(view.conversation.is_some());
    w.daemon.stop().await;
}

#[tokio::test]
async fn conversation_failure_fails_the_pr_step() {
    let w = world().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/7/comments"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({ "message": "boom" })))
        .with_priority(1)
        .mount(&w.server)
        .await;
    let mut c = w.daemon.client().await;
    let e = server_error(c.request(Command::OpenReview { pr: pr7() }).await);
    assert_eq!(e.code, ErrorCode::Upstream);
    assert!(e.message.contains("boom"), "{}", e.message);
    w.daemon.stop().await;
}

// Ambiguous answers and cleanup failures during publish.

const ORPHAN_URL: &str = "https://github.com/acme/widgets/pull/7#pullrequestreview-77";
const LEFTOVER: &str =
    "a pending review may remain on GitHub; discard it there before publishing again";

/// Answers `body_contains` GraphQL requests with `data` (HTTP 200), winning over the defaults.
async fn answer(w: &World, body_contains: &str, data: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains(body_contains))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": data })))
        .with_priority(1)
        .mount(&w.server)
        .await;
}

/// The start answers with a payload the daemon cannot read; the pending-review check sees
/// nothing, but a later read of the pull request shows `PRR_orphan` when `orphan` is set.
async fn ambiguous_start(w: &World, orphan: bool) {
    ambiguous_start_answering(
        w,
        orphan,
        json!({ "addPullRequestReview": { "pullRequestReview": { "id": 1 } } }),
    )
    .await;
}

/// Like [`ambiguous_start`], with the start answering `start` (HTTP 200).
async fn ambiguous_start_answering(w: &World, orphan: bool, start: serde_json::Value) {
    w.server.reset().await;
    let mut mock = PrMock::new(&w.head, &w.base, &w.origin);
    if orphan {
        mock.pending = Some(("PRR_orphan".into(), ORPHAN_URL.into()));
    }
    mount_pr(&w.server, &mock).await;
    mount_publish(&w.server, REVIEW_URL).await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("headRefOid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {
            "repository": { "pullRequest": {
                "id": "PR_7", "headRefOid": w.head, "reviews": { "nodes": [] }
            } }
        } })))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&w.server)
        .await;
    answer(w, "addPullRequestReview(", start).await;
}

#[tokio::test]
async fn ambiguous_start_deletes_the_orphan_pending_review() {
    let w = world().await;
    ambiguous_start(&w, true).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    reply_to(&mut c, "PRRT_a", "Agreed.").await;

    let e = server_error(c.request(publish(Verdict::Comment, "")).await);
    assert_eq!(e.code, ErrorCode::Upstream);
    assert!(!e.message.contains(LEFTOVER), "{}", e.message);
    let sent = writes(&w).await;
    assert_eq!(ops(&sent), ["start", "delete"]);
    assert!(sent[1].1.contains("PRR_orphan"), "{}", sent[1].1);
    let review = stored(&w);
    assert_eq!(
        (review.state, review.draft.items.len()),
        (ReviewState::Saved, 2)
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn ambiguous_start_without_an_orphan_deletes_nothing() {
    let w = world().await;
    ambiguous_start(&w, false).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;

    let e = server_error(c.request(publish(Verdict::Comment, "")).await);
    assert_eq!(e.code, ErrorCode::Upstream);
    assert!(!e.message.contains(LEFTOVER), "{}", e.message);
    assert_eq!(ops(&writes(&w).await), ["start"]);
    assert_eq!(stored(&w).state, ReviewState::Saved);
    w.daemon.stop().await;
}

#[tokio::test]
async fn ambiguous_start_with_a_failed_delete_warns_about_the_leftover() {
    let w = world().await;
    ambiguous_start(&w, true).await;
    graphql_error(&w.server, "deletePullRequestReview", "Something went wrong").await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;

    let e = server_error(c.request(publish(Verdict::Comment, "")).await);
    assert_eq!(e.code, ErrorCode::Upstream);
    assert!(e.message.contains(LEFTOVER), "{}", e.message);
    assert_eq!(ops(&writes(&w).await), ["start", "delete"]);
    assert_eq!(stored(&w).state, ReviewState::Saved);
    w.daemon.stop().await;
}

#[tokio::test]
async fn ambiguous_submit_is_not_posted_once_the_pending_review_is_deleted() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    answer(
        &w,
        "submitPullRequestReview",
        json!({ "submitPullRequestReview": { "pullRequestReview": { "id": 1 } } }),
    )
    .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;

    let e = server_error(c.request(publish(Verdict::Comment, "")).await);
    assert_eq!(e.code, ErrorCode::Upstream);
    assert!(!e.message.contains("may have been posted"), "{}", e.message);
    assert!(!e.message.contains(LEFTOVER), "{}", e.message);
    let sent = writes(&w).await;
    assert_eq!(ops(&sent), ["start", "submit", "delete"]);
    assert!(sent[2].1.contains("PRR_1"), "{}", sent[2].1);
    assert_eq!(
        (stored(&w).state, stored(&w).draft.items.len()),
        (ReviewState::Saved, 1)
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn ambiguous_submit_with_a_failed_delete_may_have_been_posted() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    answer(
        &w,
        "submitPullRequestReview",
        json!({ "submitPullRequestReview": { "pullRequestReview": { "id": 1 } } }),
    )
    .await;
    graphql_error(
        &w.server,
        "deletePullRequestReview",
        "Could not resolve to a node",
    )
    .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;

    let e = server_error(c.request(publish(Verdict::Comment, "")).await);
    assert_eq!(e.code, ErrorCode::Upstream);
    assert!(
        e.message.starts_with(
            "the review may have been posted; check the pull request on GitHub before publishing again ("
        ),
        "{}",
        e.message
    );
    assert!(e.message.contains(LEFTOVER), "{}", e.message);
    assert_eq!(ops(&writes(&w).await), ["start", "submit", "delete"]);
    assert_eq!(stored(&w).state, ReviewState::Saved);
    w.daemon.stop().await;
}

#[tokio::test]
async fn reply_failure_with_a_failed_delete_warns_about_the_leftover() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    graphql_error(
        &w.server,
        "addPullRequestReviewThreadReply",
        "Thread is locked",
    )
    .await;
    graphql_error(&w.server, "deletePullRequestReview", "Something went wrong").await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    reply_to(&mut c, "PRRT_a", "Still relevant?").await;

    let e = server_error(c.request(publish(Verdict::Comment, "")).await);
    assert_eq!(e.code, ErrorCode::Upstream);
    assert!(e.message.contains("Thread is locked"), "{}", e.message);
    assert!(e.message.contains(LEFTOVER), "{}", e.message);
    assert_eq!(ops(&writes(&w).await), ["start", "reply", "delete"]);
    assert_eq!(
        (stored(&w).state, stored(&w).draft.items.len()),
        (ReviewState::Saved, 2)
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn empty_start_payload_deletes_the_orphan_pending_review() {
    let w = world().await;
    ambiguous_start_answering(
        &w,
        true,
        json!({ "addPullRequestReview": { "pullRequestReview": null } }),
    )
    .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    reply_to(&mut c, "PRRT_a", "Agreed.").await;

    let e = server_error(c.request(publish(Verdict::Comment, "")).await);
    assert_eq!(e.code, ErrorCode::Upstream);
    assert!(e.message.contains("came back empty"), "{}", e.message);
    assert!(!e.message.contains(LEFTOVER), "{}", e.message);
    let sent = writes(&w).await;
    assert_eq!(ops(&sent), ["start", "delete"]);
    assert!(sent[1].1.contains("PRR_orphan"), "{}", sent[1].1);
    let review = stored(&w);
    assert_eq!(
        (review.state, review.draft.items.len()),
        (ReviewState::Saved, 2)
    );
    w.daemon.stop().await;
}
