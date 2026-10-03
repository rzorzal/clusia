mod common;

use std::time::Duration;

use clusia_protocol::{Command, Event, LoadStepKind, StepStatus, topics};
use common::TestDaemon;
use common::git_fixture::advance_pr;
use common::github_mock::{PrMock, mount_pr};
use common::review_world::{open, pr7, world};

#[tokio::test]
async fn open_creates_the_review_and_reports_load_steps() {
    let w = world().await;
    let mut watcher = w.daemon.client().await;
    watcher
        .request(Command::Subscribe {
            topics: vec![topics::REVIEWS.into()],
        })
        .await
        .unwrap();
    let mut c = w.daemon.client().await;
    let view = open(&mut c).await;

    assert_eq!(view.review.state, clusia_core::ReviewState::Active);
    assert_eq!(
        (view.review.head_sha.as_str(), view.review.title.as_str()),
        (w.head.as_str(), "Add feature")
    );
    assert_eq!(view.role, clusia_core::Role::Reviewer);
    assert_eq!(view.viewer.as_deref(), Some("me"));
    assert_eq!(view.files.len(), 1);
    let worktree = view.worktree.clone().unwrap();
    assert!(std::path::Path::new(&worktree).join("feature.txt").exists());
    assert!(w.daemon.paths.review_file(&pr7()).exists());

    let mut steps = Vec::new();
    while let Ok(Ok((_, event))) =
        tokio::time::timeout(Duration::from_millis(300), watcher.next_event()).await
    {
        if let Event::LoadStep(s) = event {
            steps.push((s.step, s.status));
        }
    }
    for expected in [
        (LoadStepKind::Repo, StepStatus::Running),
        (LoadStepKind::Repo, StepStatus::Done),
        (LoadStepKind::Branch, StepStatus::Done),
        (LoadStepKind::Pr, StepStatus::Done),
        (LoadStepKind::Agent, StepStatus::Skipped),
    ] {
        assert!(
            steps.contains(&expected),
            "missing {expected:?} in {steps:?}"
        );
    }
    let (activity, _) = clusia_store::read_activity(&w.daemon.paths).unwrap();
    assert_eq!(
        activity
            .iter()
            .filter(|a| a.kind == clusia_core::ActivityKind::ReviewOpened)
            .count(),
        1
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn author_role_when_viewer_wrote_the_pr() {
    let w = world().await;
    w.server.reset().await;
    let mut mock = PrMock::new(&w.head, &w.base, &w.origin);
    mock.viewer = "Maria".into();
    mount_pr(&w.server, &mock).await;
    let view = open(&mut w.daemon.client().await).await;
    assert_eq!(view.role, clusia_core::Role::Author);
    w.daemon.stop().await;
}

#[tokio::test]
async fn reopen_relocates_comments_after_new_commits() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;

    // Put a line comment on "three" (line 3) straight into the stored review.
    let mut review = match clusia_store::load_review(&w.daemon.paths, &pr7()).unwrap() {
        clusia_store::ReviewLoad::Found(r) => r,
        other => panic!("{other:?}"),
    };
    let anchor = clusia_core::Anchor {
        path: "feature.txt".into(),
        line: 3,
        start_line: None,
        side: clusia_core::Side::Right,
        commit: w.head.clone(),
    };
    review
        .draft
        .add(
            clusia_core::DraftKind::LineComment,
            Some(anchor),
            "check this",
            1,
        )
        .unwrap();
    clusia_store::save_review(&w.daemon.paths, &review).unwrap();

    // The PR gains a line at the top.
    let new_head = advance_pr(w.tmp.path(), 7, "feature.txt", "zero\none\ntwo\nthree\n");
    w.server.reset().await;
    mount_pr(
        &w.server,
        &PrMock::new(&new_head, &w.base, &w.origin).adding_feature("zero\none\ntwo\nthree\n"),
    )
    .await;

    let view = open(&mut c).await;
    let item = &view.review.draft.items[0];
    assert_eq!(view.review.head_sha, new_head);
    assert_eq!(item.anchor.as_ref().unwrap().line, 4);
    assert_eq!(
        item.status,
        clusia_core::ItemStatus::Moved {
            from_path: "feature.txt".into(),
            from_line: 3
        }
    );
    let (activity, _) = clusia_store::read_activity(&w.daemon.paths).unwrap();
    let outdated = activity
        .iter()
        .find(|a| a.kind == clusia_core::ActivityKind::ReviewOutdated)
        .unwrap();
    assert_eq!(outdated.note.as_deref(), Some("1 moved, 0 obsolete"));
    w.daemon.stop().await;
}

#[tokio::test]
async fn open_without_token_is_unauthorized() {
    let d = TestDaemon::start().await;
    match d
        .client()
        .await
        .request(Command::OpenReview { pr: pr7() })
        .await
    {
        Err(clusia_protocol::ClientError::Server(e)) => {
            assert_eq!(e.code, clusia_protocol::ErrorCode::Unauthorized)
        }
        other => panic!("{other:?}"),
    }
    d.stop().await;
}

#[tokio::test]
async fn unreadable_review_file_is_not_overwritten() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let file = w.daemon.paths.review_file(&pr7());
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    match c.request(Command::OpenReview { pr: pr7() }).await {
        Err(clusia_protocol::ClientError::Server(e)) => {
            assert_eq!(e.code, clusia_protocol::ErrorCode::Internal)
        }
        other => panic!("{other:?}"),
    }
    assert!(file.is_dir());
    w.daemon.stop().await;
}

#[tokio::test]
async fn relocated_comment_outside_the_new_diff_is_obsolete() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    for line in [1, 3] {
        c.request(Command::AddDraftItem {
            pr: pr7(),
            kind: clusia_core::DraftKind::LineComment,
            anchor: Some(clusia_protocol::AnchorInput {
                path: "feature.txt".into(),
                line,
                start_line: None,
                side: clusia_core::Side::Right,
            }),
            body: format!("on {line}"),
        })
        .await
        .unwrap();
    }

    // A line is added at the top; GitHub's diff now only covers lines 1-2 of the file.
    let new_head = advance_pr(w.tmp.path(), 7, "feature.txt", "zero\none\ntwo\nthree\n");
    w.server.reset().await;
    let mut mock = PrMock::new(&new_head, &w.base, &w.origin);
    mock.files = serde_json::json!([{
        "filename": "feature.txt", "status": "added", "additions": 2, "deletions": 0,
        "patch": "@@ -0,0 +1,2 @@\n+zero\n+one"
    }]);
    mount_pr(&w.server, &mock).await;

    let view = open(&mut c).await;
    let items = &view.review.draft.items;
    assert_eq!(items[0].anchor.as_ref().unwrap().line, 2);
    assert!(matches!(
        items[0].status,
        clusia_core::ItemStatus::Moved { .. }
    ));
    assert_eq!(
        items[1].status,
        clusia_core::ItemStatus::Obsolete {
            reason: "no longer part of the pull request's diff".into()
        }
    );
    let (activity, _) = clusia_store::read_activity(&w.daemon.paths).unwrap();
    let outdated = activity
        .iter()
        .find(|a| a.kind == clusia_core::ActivityKind::ReviewOutdated)
        .unwrap();
    assert_eq!(outdated.note.as_deref(), Some("1 moved, 1 obsolete"));
    w.daemon.stop().await;
}

#[tokio::test]
async fn left_comments_are_obsolete_when_the_base_cannot_be_fetched() {
    let w = world().await;
    w.server.reset().await;
    let files = serde_json::json!([
        { "filename": "feature.txt", "status": "added", "additions": 3, "deletions": 0, "patch": "@@ -0,0 +1,3 @@\n+one\n+two\n+three" },
        { "filename": "README.md", "status": "modified", "additions": 1, "deletions": 1, "patch": "@@ -1 +1 @@\n-hello\n+hello!" }
    ]);
    let mut mock = PrMock::new(&w.head, &w.base, &w.origin);
    mock.files = files.clone();
    mount_pr(&w.server, &mock).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(Command::AddDraftItem {
        pr: pr7(),
        kind: clusia_core::DraftKind::LineComment,
        anchor: Some(clusia_protocol::AnchorInput {
            path: "README.md".into(),
            line: 1,
            start_line: None,
            side: clusia_core::Side::Left,
        }),
        body: "why?".into(),
    })
    .await
    .unwrap();

    // The PR was retargeted to a branch the remote does not have (any more).
    w.server.reset().await;
    let mut mock = PrMock::new(&w.head, &"f".repeat(40), &w.origin);
    mock.base_ref = "gone".into();
    mock.files = files;
    mount_pr(&w.server, &mock).await;
    let view = open(&mut c).await;
    assert_eq!(
        view.review.draft.items[0].status,
        clusia_core::ItemStatus::Obsolete {
            reason: "cannot fetch the base branch".into()
        }
    );
    w.daemon.stop().await;
}
