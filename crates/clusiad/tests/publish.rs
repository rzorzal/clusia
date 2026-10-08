mod common;

use clusia_core::{ActivityKind, DraftKind, ReviewState, Side, Verdict};
use clusia_protocol::{AnchorInput, ClientError, Command, ErrorCode, PublishResult, Reply};
use common::git_fixture::advance_pr;
use common::github_mock::{PrMock, graphql_error, graphql_requests, mount_pr, mount_publish};
use common::review_world::{open, pr7, view_of, world};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

const REVIEW_URL: &str = "https://github.com/acme/widgets/pull/7#pullrequestreview-42";

fn code(r: Result<Reply, ClientError>) -> ErrorCode {
    match r {
        Err(ClientError::Server(e)) => e.code,
        other => panic!("expected an error, got {other:?}"),
    }
}

async fn comment_on_line_2(c: &mut clusia_protocol::Client) {
    let anchor = Some(AnchorInput {
        path: "feature.txt".into(),
        line: 2,
        start_line: None,
        side: Side::Right,
    });
    c.request(Command::AddDraftItem {
        pr: pr7(),
        thread: None,
        kind: DraftKind::LineComment,
        anchor,
        body: "rename".into(),
    })
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

#[tokio::test]
async fn publishes_one_review_and_cleans_up() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let mut c = w.daemon.client().await;
    let worktree = std::path::PathBuf::from(open(&mut c).await.worktree.unwrap());
    comment_on_line_2(&mut c).await;

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
    let started = graphql_requests(&w.server, "addPullRequestReview(").await;
    assert_eq!(started.len(), 1, "one pending review");
    assert_eq!(
        started[0]["variables"]["input"],
        json!({
            "pullRequestId": "PR_7", "commitOID": w.head,
            "threads": [{ "path": "feature.txt", "line": 2, "side": "RIGHT", "body": "rename" }]
        })
    );
    let submitted = graphql_requests(&w.server, "submitPullRequestReview").await;
    assert_eq!(
        submitted[0]["variables"]["input"],
        json!({ "pullRequestReviewId": "PRR_1", "event": "REQUEST_CHANGES", "body": "Please rename." })
    );
    assert!(
        graphql_requests(&w.server, "deletePullRequestReview")
            .await
            .is_empty()
    );
    assert!(!w.daemon.paths.review_file(&pr7()).exists());
    assert!(!worktree.exists());
    let (activity, _) = clusia_store::read_activity(&w.daemon.paths).unwrap();
    let published = activity
        .iter()
        .find(|a| a.kind == ActivityKind::ReviewPublished)
        .unwrap();
    assert_eq!(published.url.as_deref(), Some(REVIEW_URL));
    w.daemon.stop().await;
}

#[tokio::test]
async fn failed_publish_keeps_the_draft() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    graphql_error(
        &w.server,
        "addPullRequestReview(",
        "Line could not be resolved",
    )
    .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    match c.request(publish(Verdict::Comment, "")).await {
        Err(ClientError::Server(e)) => {
            assert_eq!(e.code, ErrorCode::Upstream);
            assert!(
                e.message.contains("Line could not be resolved"),
                "{}",
                e.message
            );
        }
        other => panic!("{other:?}"),
    }
    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(r) => assert_eq!((r.state, r.draft.items.len()), (ReviewState::Saved, 1)),
        other => panic!("{other:?}"),
    }
    assert!(
        graphql_requests(&w.server, "submitPullRequestReview")
            .await
            .is_empty()
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn failed_submit_deletes_the_pending_review() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    graphql_error(
        &w.server,
        "submitPullRequestReview",
        "Review body is too long",
    )
    .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    match c.request(publish(Verdict::Comment, "")).await {
        Err(ClientError::Server(e)) => {
            assert_eq!(e.code, ErrorCode::Upstream);
            assert!(
                e.message.contains("Review body is too long"),
                "{}",
                e.message
            );
        }
        other => panic!("{other:?}"),
    }
    let deleted = graphql_requests(&w.server, "deletePullRequestReview").await;
    assert_eq!(
        deleted[0]["variables"]["input"],
        json!({ "pullRequestReviewId": "PRR_1" })
    );
    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(r) => assert_eq!((r.state, r.draft.items.len()), (ReviewState::Saved, 1)),
        other => panic!("{other:?}"),
    }
    w.daemon.stop().await;
}

#[tokio::test]
async fn publish_after_new_commits_conflicts() {
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

    assert_eq!(
        code(c.request(publish(Verdict::Comment, "")).await),
        ErrorCode::Conflict
    );
    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(r) => {
            assert_eq!(r.head_sha, new_head);
            assert_eq!(
                r.draft.items[0].anchor.as_ref().unwrap().line,
                3,
                "line 2 moved to 3"
            );
        }
        other => panic!("{other:?}"),
    }
    w.daemon.stop().await;
}

#[tokio::test]
async fn author_cannot_approve_but_can_close() {
    let w = world().await;
    w.server.reset().await;
    let mut mock = PrMock::new(&w.head, &w.base, &w.origin);
    mock.viewer = "maria".into();
    mount_pr(&w.server, &mock).await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("submitPullRequestReview"))
        .and(body_partial_json(json!({
            "variables": { "input": { "event": "COMMENT", "body": "Superseded by #8" } }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {
            "submitPullRequestReview": { "pullRequestReview": {
                "id": "PRR_1", "databaseId": 43, "url": REVIEW_URL, "state": "COMMENTED"
            } }
        } })))
        .expect(1)
        .with_priority(1)
        .mount(&w.server)
        .await;
    mount_publish(&w.server, REVIEW_URL).await;
    Mock::given(method("PATCH"))
        .and(path("/repos/acme/widgets/pulls/7"))
        .and(body_partial_json(json!({ "state": "closed" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "state": "closed" })))
        .expect(1)
        .mount(&w.server)
        .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    assert_eq!(
        code(c.request(publish(Verdict::Approve, "")).await),
        ErrorCode::BadRequest
    );
    let reply = c
        .request(publish(Verdict::ClosePr, "Superseded by #8"))
        .await
        .unwrap();
    assert_eq!(
        reply,
        Reply::Published(PublishResult {
            url: Some(REVIEW_URL.into()),
            closed: true,
            unresolved: vec![],
            close_error: None,
        })
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn nothing_to_publish_is_a_bad_request() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    assert_eq!(
        code(c.request(publish(Verdict::Comment, " ")).await),
        ErrorCode::BadRequest
    );
    assert_eq!(
        code(c.request(publish(Verdict::RequestChanges, "")).await),
        ErrorCode::BadRequest
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn posted_but_close_failed_is_published_with_the_close_error() {
    let w = world().await;
    w.server.reset().await;
    let mut mock = PrMock::new(&w.head, &w.base, &w.origin);
    mock.viewer = "maria".into();
    mount_pr(&w.server, &mock).await;
    mount_publish(&w.server, REVIEW_URL).await;
    Mock::given(method("PATCH"))
        .and(path("/repos/acme/widgets/pulls/7"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({ "message": "boom" })))
        .mount(&w.server)
        .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    match c.request(publish(Verdict::ClosePr, "Superseded")).await {
        Ok(Reply::Published(result)) => {
            assert_eq!(result.url.as_deref(), Some(REVIEW_URL));
            assert!(!result.closed, "the pull request stays open");
            assert!(result.unresolved.is_empty());
            let error = result.close_error.expect("why closing failed");
            assert!(error.contains("boom"), "{error}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        graphql_requests(&w.server, "submitPullRequestReview")
            .await
            .len(),
        1
    );
    assert!(!w.daemon.paths.review_file(&pr7()).exists());
    assert_eq!(
        code(c.request(Command::GetReview { pr: pr7() }).await),
        ErrorCode::InvalidState
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn reviewer_cannot_close() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    assert_eq!(
        code(c.request(publish(Verdict::ClosePr, "nope")).await),
        ErrorCode::BadRequest
    );
    no_review_written(&w).await;
    w.daemon.stop().await;
}

async fn no_review_written(w: &common::review_world::World) {
    for op in ["addPullRequestReview(", "submitPullRequestReview"] {
        assert!(
            graphql_requests(&w.server, op).await.is_empty(),
            "{op} was sent"
        );
    }
}

fn stored_review(w: &common::review_world::World) -> clusia_core::Review {
    match clusia_store::load_review(&w.daemon.paths, &pr7()).unwrap() {
        clusia_store::ReviewLoad::Found(r) => r,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn obsolete_comments_block_publishing() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    comment_on_line_2(&mut c).await;
    comment_on_line_2(&mut c).await;
    let mut review = stored_review(&w);
    for item in review.draft.items.iter_mut().take(2) {
        item.status = clusia_core::ItemStatus::Obsolete {
            reason: "the commented lines changed".into(),
        };
    }
    clusia_store::save_review(&w.daemon.paths, &review).unwrap();

    match c.request(publish(Verdict::Comment, "")).await {
        Err(ClientError::Server(e)) => {
            assert_eq!(e.code, ErrorCode::BadRequest);
            assert_eq!(
                e.message,
                "2 comment(s) are obsolete; remove or re-add them before publishing"
            );
        }
        other => panic!("{other:?}"),
    }
    let after = stored_review(&w);
    assert_eq!(
        (after.state, after.draft.items.len()),
        (ReviewState::Active, 3)
    );
    no_review_written(&w).await;
    w.daemon.stop().await;
}

#[tokio::test]
async fn publish_of_an_interrupted_publish_asks_to_reopen() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    let mut review = stored_review(&w);
    review.state = ReviewState::Publishing;
    clusia_store::save_review(&w.daemon.paths, &review).unwrap();

    match c.request(publish(Verdict::Comment, "")).await {
        Err(ClientError::Server(e)) => {
            assert_eq!(e.code, ErrorCode::InvalidState);
            assert_eq!(
                e.message,
                "a publish is in progress or was interrupted; open the review again"
            );
        }
        other => panic!("{other:?}"),
    }
    no_review_written(&w).await;
    w.daemon.stop().await;
}

/// What GitHub lists for acme/widgets#7 (winning over the empty default).
async fn mount_reviews(w: &common::review_world::World, reviews: Value) {
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reviews))
        .with_priority(1)
        .mount(&w.server)
        .await;
}

/// A review `me` submitted on the PR's head after any Finalize moment.
fn my_review(w: &common::review_world::World) -> Value {
    json!({
        "id": 42, "user": { "login": "me" }, "state": "COMMENTED", "body": "Please rename.",
        "submitted_at": "2099-01-01T00:00:00Z", "html_url": REVIEW_URL, "commit_id": w.head
    })
}

fn activity_of(w: &common::review_world::World, kind: ActivityKind) -> Vec<clusia_core::Activity> {
    let (activity, _) = clusia_store::read_activity(&w.daemon.paths).unwrap();
    activity.into_iter().filter(|a| a.kind == kind).collect()
}

#[tokio::test]
async fn a_lost_answer_after_submit_is_recorded_not_posted_again() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    // GitHub takes the submit but the answer is unreadable, and the review can no longer be
    // deleted because it is not pending any more.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("submitPullRequestReview"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .with_priority(1)
        .mount(&w.server)
        .await;
    graphql_error(
        &w.server,
        "deletePullRequestReview",
        "Could not resolve to a node",
    )
    .await;
    mount_reviews(&w, json!([my_review(&w)])).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;

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
    assert_eq!(
        graphql_requests(&w.server, "addPullRequestReview(")
            .await
            .len(),
        1,
        "one pending review"
    );
    assert_eq!(
        graphql_requests(&w.server, "submitPullRequestReview")
            .await
            .len(),
        1,
        "one submit"
    );
    assert!(!w.daemon.paths.review_file(&pr7()).exists());
    let published = activity_of(&w, ActivityKind::ReviewPublished);
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].url.as_deref(), Some(REVIEW_URL));
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_lost_answer_with_no_review_on_github_fails_as_before() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("submitPullRequestReview"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .with_priority(1)
        .mount(&w.server)
        .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    assert_eq!(
        code(c.request(publish(Verdict::Comment, "")).await),
        ErrorCode::Upstream
    );
    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(r) => assert_eq!((r.state, r.draft.items.len()), (ReviewState::Saved, 1)),
        other => panic!("{other:?}"),
    }
    assert!(activity_of(&w, ActivityKind::ReviewPublished).is_empty());
    w.daemon.stop().await;
}

/// Leaves the stored review as a daemon that died mid-publish would: `publishing`.
fn die_during_publish(w: &common::review_world::World) {
    let mut review = stored_review(w);
    review.state = ReviewState::Publishing;
    clusia_store::save_review(&w.daemon.paths, &review).unwrap();
}

#[tokio::test]
async fn death_during_publish_does_not_post_again() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    die_during_publish(&w);
    mount_reviews(&w, json!([my_review(&w)])).await;

    let view = view_of(c.request(Command::OpenReview { pr: pr7() }).await.unwrap());
    assert_eq!(
        (view.review.state, view.review.draft.items.len()),
        (ReviewState::Active, 0),
        "the review GitHub already has is not a draft any more"
    );
    assert!(
        graphql_requests(&w.server, "addPullRequestReview(")
            .await
            .is_empty(),
        "nothing is posted again"
    );
    let published = activity_of(&w, ActivityKind::ReviewPublished);
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].url.as_deref(), Some(REVIEW_URL));
    w.daemon.stop().await;
}

#[tokio::test]
async fn an_interrupted_publish_that_never_reached_github_keeps_its_draft() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    die_during_publish(&w);
    // Someone else's review and one on an older commit are not ours to adopt.
    mount_reviews(
        &w,
        json!([
            { "id": 1, "user": { "login": "mona" }, "state": "APPROVED", "body": "",
              "submitted_at": "2099-01-01T00:00:00Z", "html_url": REVIEW_URL, "commit_id": w.head },
            { "id": 2, "user": { "login": "me" }, "state": "COMMENTED", "body": "",
              "submitted_at": "2099-01-01T00:00:00Z", "html_url": REVIEW_URL, "commit_id": "0000" }
        ]),
    )
    .await;

    let view = view_of(c.request(Command::OpenReview { pr: pr7() }).await.unwrap());
    assert_eq!(
        (view.review.state, view.review.draft.items.len()),
        (ReviewState::Active, 1)
    );
    assert!(activity_of(&w, ActivityKind::ReviewPublished).is_empty());
    w.daemon.stop().await;
}

#[tokio::test]
async fn an_unreadable_review_list_leaves_an_interrupted_publish_alone() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    die_during_publish(&w);
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({ "message": "forbidden" })))
        .with_priority(1)
        .mount(&w.server)
        .await;

    assert_eq!(
        code(c.request(Command::OpenReview { pr: pr7() }).await),
        ErrorCode::Upstream
    );
    assert_eq!(stored_review(&w).state, ReviewState::Publishing);
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_publish_whose_outcome_is_unknown_is_never_posted_twice() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    // The connection drops right after the submit: its answer, the cleanup and the check that
    // follows all fail, so nobody can tell whether GitHub took the review.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("submitPullRequestReview"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .with_priority(1)
        .mount(&w.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("deletePullRequestReview"))
        .respond_with(ResponseTemplate::new(502))
        .with_priority(1)
        .mount(&w.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&w.server)
        .await;
    // Once the network is back, GitHub lists the review it did take.
    mount_reviews(&w, json!([my_review(&w)])).await;

    assert_eq!(
        code(c.request(publish(Verdict::Comment, "Please rename.")).await),
        ErrorCode::Upstream
    );
    assert_eq!(
        stored_review(&w).state,
        ReviewState::Publishing,
        "an unknown outcome is not a failure to retry"
    );
    assert_eq!(
        code(c.request(publish(Verdict::Comment, "Please rename.")).await),
        ErrorCode::InvalidState,
        "publishing again is refused until GitHub has been asked"
    );
    let view = view_of(c.request(Command::OpenReview { pr: pr7() }).await.unwrap());
    assert_eq!(
        (view.review.state, view.review.draft.items.len()),
        (ReviewState::Active, 0),
        "the review GitHub took is recorded, not offered again"
    );
    assert_eq!(
        graphql_requests(&w.server, "addPullRequestReview(")
            .await
            .len(),
        1,
        "one review posted"
    );
    let published = activity_of(&w, ActivityKind::ReviewPublished);
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].url.as_deref(), Some(REVIEW_URL));
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_second_publish_never_takes_the_first_review_for_its_own() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    c.request(publish(Verdict::Comment, "")).await.unwrap();
    // A moment later, on the same head, a second review loses its answer; GitHub lists only the
    // first one, which is already recorded.
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("submitPullRequestReview"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .with_priority(1)
        .mount(&w.server)
        .await;
    graphql_error(
        &w.server,
        "deletePullRequestReview",
        "Could not resolve to a node",
    )
    .await;
    mount_reviews(&w, json!([my_review(&w)])).await;

    assert_eq!(
        code(c.request(publish(Verdict::Comment, "")).await),
        ErrorCode::Upstream
    );
    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(r) => assert_eq!((r.state, r.draft.items.len()), (ReviewState::Saved, 1)),
        other => panic!("{other:?}"),
    }
    assert_eq!(activity_of(&w, ActivityKind::ReviewPublished).len(), 1);
    w.daemon.stop().await;
}
