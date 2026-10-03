mod common;

use clusia_core::{DraftKind, ItemStatus, ReviewState, Side};
use clusia_protocol::{AnchorInput, ClientError, Command, ErrorCode, Reply};
use common::github_mock::{PrMock, mount_pr};
use common::review_world::{open, pr7, world};

fn code(r: Result<Reply, ClientError>) -> ErrorCode {
    match r {
        Err(ClientError::Server(e)) => e.code,
        other => panic!("expected an error, got {other:?}"),
    }
}

fn line(path: &str, line: u32) -> Option<AnchorInput> {
    Some(AnchorInput {
        path: path.into(),
        line,
        start_line: None,
        side: Side::Right,
    })
}

fn add(pr_line: Option<AnchorInput>, kind: DraftKind, body: &str) -> Command {
    Command::AddDraftItem {
        pr: pr7(),
        kind,
        anchor: pr_line,
        body: body.into(),
    }
}

#[tokio::test]
async fn add_update_remove_items() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;

    let item = match c
        .request(add(
            line("feature.txt", 2),
            DraftKind::LineComment,
            "rename?",
        ))
        .await
        .unwrap()
    {
        Reply::DraftItem(i) => i,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        (
            item.id.as_str(),
            item.anchor.as_ref().unwrap().commit.as_str()
        ),
        ("i1", w.head.as_str())
    );
    c.request(add(None, DraftKind::General, "Looks good overall"))
        .await
        .unwrap();
    match c
        .request(Command::UpdateDraftItem {
            pr: pr7(),
            id: "i1".into(),
            body: "rename it".into(),
        })
        .await
        .unwrap()
    {
        Reply::DraftItem(i) => assert_eq!(i.body, "rename it"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        c.request(Command::RemoveDraftItem {
            pr: pr7(),
            id: "i2".into()
        })
        .await
        .unwrap(),
        Reply::Ack
    );
    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(r) => {
            assert_eq!(r.draft.items.len(), 1);
            assert_eq!(r.draft.items[0].status, ItemStatus::Ok);
        }
        other => panic!("{other:?}"),
    }
    let (activity, _) = clusia_store::read_activity(&w.daemon.paths).unwrap();
    let added: Vec<_> = activity
        .iter()
        .filter(|a| a.kind == clusia_core::ActivityKind::ItemAdded)
        .collect();
    assert_eq!(added.len(), 2);
    assert_eq!(added[0].client, "test");
    w.daemon.stop().await;
}

#[tokio::test]
async fn comment_outside_diff_is_rejected() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    assert_eq!(
        code(
            c.request(add(line("feature.txt", 40), DraftKind::LineComment, "x"))
                .await
        ),
        ErrorCode::BadRequest
    );
    assert_eq!(
        code(
            c.request(add(line("README.md", 1), DraftKind::LineComment, "x"))
                .await
        ),
        ErrorCode::BadRequest
    );
    assert_eq!(
        code(c.request(add(None, DraftKind::General, "   ")).await),
        ErrorCode::BadRequest
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn items_need_an_open_review() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    assert_eq!(
        code(c.request(add(None, DraftKind::General, "x")).await),
        ErrorCode::InvalidState
    );
    assert_eq!(
        code(c.request(Command::GetReview { pr: pr7() }).await),
        ErrorCode::InvalidState
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn close_saves_or_forgets_and_list_shows_saved() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    assert!(
        !w.daemon.paths.review_file(&pr7()).exists(),
        "an empty draft is forgotten"
    );

    open(&mut c).await;
    c.request(add(None, DraftKind::General, "keep me"))
        .await
        .unwrap();
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    match c.request(Command::ListReviews).await.unwrap() {
        Reply::Reviews(list) => {
            assert_eq!(list.len(), 1);
            assert_eq!(
                (list[0].state, list[0].items, list[0].title.as_str()),
                (ReviewState::Saved, 1, "Add feature")
            );
        }
        other => panic!("{other:?}"),
    }
    w.daemon.stop().await;
}

#[tokio::test]
async fn discard_removes_review_and_worktree() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    let view = open(&mut c).await;
    let worktree = std::path::PathBuf::from(view.worktree.unwrap());
    assert!(worktree.exists());
    assert_eq!(
        c.request(Command::DiscardReview { pr: pr7() })
            .await
            .unwrap(),
        Reply::Ack
    );
    assert!(!worktree.exists());
    assert!(!w.daemon.paths.review_file(&pr7()).exists());
    match c.request(Command::ListReviews).await.unwrap() {
        Reply::Reviews(list) => assert!(list.is_empty()),
        other => panic!("{other:?}"),
    }
    w.daemon.stop().await;
}

#[tokio::test]
async fn diff_and_conversation_come_from_github() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    match c.request(Command::GetDiff { pr: pr7() }).await.unwrap() {
        Reply::Diff(files) => assert_eq!(files[0].path, "feature.txt"),
        other => panic!("{other:?}"),
    }
    match c
        .request(Command::GetConversation { pr: pr7() })
        .await
        .unwrap()
    {
        Reply::Conversation(conv) => assert!(conv.threads.is_empty() && conv.reviews.is_empty()),
        other => panic!("{other:?}"),
    }
    w.daemon.stop().await;
}

#[tokio::test]
async fn range_and_left_side_validation() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let ranged = |start: u32, end: u32| {
        Some(AnchorInput {
            path: "feature.txt".into(),
            line: end,
            start_line: Some(start),
            side: Side::Right,
        })
    };
    c.request(add(ranged(1, 3), DraftKind::LineComment, "range"))
        .await
        .unwrap();
    assert_eq!(
        code(
            c.request(add(ranged(1, 9), DraftKind::LineComment, "x"))
                .await
        ),
        ErrorCode::BadRequest
    );
    let left = Some(AnchorInput {
        path: "feature.txt".into(),
        line: 1,
        start_line: None,
        side: Side::Left,
    });
    assert_eq!(
        code(c.request(add(left, DraftKind::LineComment, "x")).await),
        ErrorCode::BadRequest
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn read_error_is_not_reported_as_missing() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let file = w.daemon.paths.review_file(&pr7());
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    assert_eq!(
        code(c.request(Command::GetReview { pr: pr7() }).await),
        ErrorCode::Internal
    );
    assert!(file.is_dir());
    w.daemon.stop().await;
}

#[tokio::test]
async fn range_spanning_two_hunks_is_rejected() {
    let w = world().await;
    w.server.reset().await;
    let mut mock = PrMock::new(&w.head, &w.base, &w.origin);
    mock.files = serde_json::json!([{
        "filename": "feature.txt", "status": "modified", "additions": 2, "deletions": 2,
        "patch": "@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -10,2 +10,2 @@\n x\n-y\n+Y"
    }]);
    mount_pr(&w.server, &mock).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let ranged = |start: u32, end: u32| {
        Some(AnchorInput {
            path: "feature.txt".into(),
            line: end,
            start_line: Some(start),
            side: Side::Right,
        })
    };
    c.request(add(ranged(1, 2), DraftKind::LineComment, "inside one hunk"))
        .await
        .unwrap();
    assert_eq!(
        code(
            c.request(add(ranged(2, 10), DraftKind::LineComment, "spans the gap"))
                .await
        ),
        ErrorCode::BadRequest
    );
    w.daemon.stop().await;
}
