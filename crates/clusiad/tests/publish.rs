mod common;

use clusia_core::{ActivityKind, DraftKind, ReviewState, Side, Verdict};
use clusia_protocol::{AnchorInput, ClientError, Command, ErrorCode, PublishResult, Reply};
use common::git_fixture::advance_pr;
use common::github_mock::{PrMock, mount_pr};
use common::review_world::{open, pr7, world};
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
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
    Mock::given(method("POST"))
        .and(path("/repos/acme/widgets/pulls/7/reviews"))
        .and(body_partial_json(json!({
            "commit_id": w.head, "event": "REQUEST_CHANGES", "body": "Please rename.",
            "comments": [{ "path": "feature.txt", "line": 2, "side": "RIGHT", "body": "rename" }]
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "id": 42, "html_url": REVIEW_URL })),
        )
        .expect(1)
        .mount(&w.server)
        .await;
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
            closed: false
        })
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
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(422).set_body_json(
            json!({ "message": "Unprocessable Entity", "errors": ["Line could not be resolved"] }),
        ))
        .mount(&w.server)
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
    mount_pr(&w.server, &PrMock::new(&new_head, &w.base, &w.origin)).await;

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
        .and(path("/repos/acme/widgets/pulls/7/reviews"))
        .and(body_partial_json(
            json!({ "event": "COMMENT", "body": "Superseded by #8" }),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "id": 43, "html_url": REVIEW_URL })),
        )
        .expect(1)
        .mount(&w.server)
        .await;
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
            closed: true
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
async fn posted_but_close_failed_returns_upstream_with_url_and_forgets_draft() {
    let w = world().await;
    w.server.reset().await;
    let mut mock = PrMock::new(&w.head, &w.base, &w.origin);
    mock.viewer = "maria".into();
    mount_pr(&w.server, &mock).await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/widgets/pulls/7/reviews"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "id": 43, "html_url": REVIEW_URL })),
        )
        .expect(1)
        .mount(&w.server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/acme/widgets/pulls/7"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({ "message": "boom" })))
        .mount(&w.server)
        .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    match c.request(publish(Verdict::ClosePr, "Superseded")).await {
        Err(ClientError::Server(e)) => {
            assert_eq!(e.code, ErrorCode::Upstream);
            assert!(e.message.contains(REVIEW_URL), "{}", e.message);
        }
        other => panic!("{other:?}"),
    }
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
    Mock::given(method("POST"))
        .and(path("/repos/acme/widgets/pulls/7/reviews"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "id": 44, "html_url": REVIEW_URL })),
        )
        .expect(0)
        .mount(&w.server)
        .await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    assert_eq!(
        code(c.request(publish(Verdict::ClosePr, "nope")).await),
        ErrorCode::BadRequest
    );
    w.daemon.stop().await;
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
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "id": 45, "html_url": REVIEW_URL })),
        )
        .expect(0)
        .mount(&w.server)
        .await;
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
    w.daemon.stop().await;
}

#[tokio::test]
async fn publish_of_an_interrupted_publish_asks_to_reopen() {
    let w = world().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "id": 46, "html_url": REVIEW_URL })),
        )
        .expect(0)
        .mount(&w.server)
        .await;
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
    w.daemon.stop().await;
}
