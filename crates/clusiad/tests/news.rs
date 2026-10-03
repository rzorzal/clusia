mod common;

use std::time::Duration;

use clusia_core::{ActivityKind, DraftKind, ItemStatus, ReviewState, Side};
use clusia_protocol::{Command, Event, NewsKind, Reply, topics};
use common::git_fixture::advance_pr;
use common::github_mock::{PrMock, mount_pr};
use common::review_world::{open, pr7, world};
use serde_json::json;

#[tokio::test]
async fn whats_new_is_empty_until_seen_then_lists_new_commits() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    assert_eq!(
        c.request(Command::GetWhatsNew { pr: pr7() }).await.unwrap(),
        Reply::WhatsNew(vec![])
    );
    c.request(Command::MarkSeen { pr: pr7() }).await.unwrap();

    w.server.reset().await;
    let mut mock = PrMock::new(&w.head, &w.base, &w.origin);
    mock.commits = json!([
        { "sha": "c9", "author": { "login": "maria" }, "commit": { "message": "more", "author": { "name": "Maria", "date": "2999-01-01T00:00:00Z" } } }
    ]);
    mount_pr(&w.server, &mock).await;
    match c.request(Command::GetWhatsNew { pr: pr7() }).await.unwrap() {
        Reply::WhatsNew(items) => {
            assert_eq!(items.len(), 1);
            assert_eq!(
                (items[0].kind, items[0].summary.as_str()),
                (NewsKind::Commits, "1 new commit")
            );
        }
        other => panic!("{other:?}"),
    }
    w.daemon.stop().await;
}

#[tokio::test]
async fn saved_review_goes_outdated_and_relocates() {
    let w = world().await;
    let mut watcher = w.daemon.client().await;
    watcher
        .request(Command::Subscribe {
            topics: vec![topics::REVIEWS.into()],
        })
        .await
        .unwrap();
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let anchor = Some(clusia_protocol::AnchorInput {
        path: "feature.txt".into(),
        line: 3,
        start_line: None,
        side: Side::Right,
    });
    c.request(Command::AddDraftItem {
        pr: pr7(),
        kind: DraftKind::LineComment,
        anchor,
        body: "x".into(),
    })
    .await
    .unwrap();
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();

    let new_head = advance_pr(w.tmp.path(), 7, "feature.txt", "zero\none\ntwo\nthree\n");
    w.server.reset().await;
    mount_pr(&w.server, &PrMock::new(&new_head, &w.base, &w.origin)).await;
    // The check only runs while GitHub is reachable, so the PR lists must sync too.
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/search/issues"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(json!({ "total_count": 0, "items": [] })),
        )
        .mount(&w.server)
        .await;
    c.request(Command::SyncNow).await.unwrap();

    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(r) => {
            assert_eq!(r.state, ReviewState::Outdated);
            assert_eq!(r.draft.items[0].anchor.as_ref().unwrap().line, 4);
            assert!(matches!(r.draft.items[0].status, ItemStatus::Moved { .. }));
        }
        other => panic!("{other:?}"),
    }
    let mut saw_outdated = false;
    while let Ok(Ok((_, event))) =
        tokio::time::timeout(Duration::from_millis(500), watcher.next_event()).await
    {
        if let Event::ReviewOutdated {
            moved, obsolete, ..
        } = event
        {
            assert_eq!((moved, obsolete), (1, 0));
            saw_outdated = true;
        }
    }
    assert!(saw_outdated);
    let (activity, _) = clusia_store::read_activity(&w.daemon.paths).unwrap();
    assert!(
        activity
            .iter()
            .any(|a| a.kind == ActivityKind::ReviewOutdated)
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn retention_removes_orphans_and_keeps_live_reviews() {
    let dir = tempfile::tempdir().unwrap();
    let paths = clusia_core::Paths::new(dir.path());
    std::fs::create_dir_all(paths.worktrees_dir().join("acme__widgets__1")).unwrap(); // old M2 key: orphan
    std::fs::create_dir_all(paths.worktrees_dir().join("acme~widgets~2")).unwrap(); // no review: orphan
    let live: clusia_core::PrRef = "acme/widgets#3".parse().unwrap();
    std::fs::create_dir_all(paths.worktree_for(&live)).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let review = clusia_core::Review::new(live.clone(), "t".into(), "b".into(), "h".into(), now);
    clusia_store::save_review(&paths, &review).unwrap();

    let d = common::TestDaemon::start_in(dir).await;
    let gone = |p: std::path::PathBuf| async move {
        for _ in 0..40 {
            if !p.exists() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    };
    assert!(gone(d.paths.worktrees_dir().join("acme__widgets__1")).await);
    assert!(gone(d.paths.worktrees_dir().join("acme~widgets~2")).await);
    assert!(d.paths.worktree_for(&live).exists());
    d.stop().await;
}
