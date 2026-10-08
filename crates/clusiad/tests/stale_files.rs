//! The files GitHub lists belong to the head they were read for.

mod common;

use clusia_core::{DraftKind, Side};
use clusia_protocol::{AnchorInput, ClientError, Command, ErrorCode, Reply};
use common::git_fixture::advance_pr;
use common::github_mock::{PrMock, mount_pr};
use common::review_world::{open, pr7, world};
use common::{TestDaemon, test_options};

#[tokio::test]
async fn a_push_while_the_files_load_is_not_filed_under_the_old_head() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    drop(c);
    let home = w.daemon.stop().await;

    // The author pushes. GitHub now lists the new head's files, while the stored review is
    // still on the old head and the restarted daemon has nothing cached for it.
    let new_head = advance_pr(w.tmp.path(), 7, "feature.txt", "zero\none\ntwo\nthree\n");
    w.server.reset().await;
    mount_pr(
        &w.server,
        &PrMock::new(&new_head, &w.base, &w.origin).adding_feature("zero\none\ntwo\nthree\n"),
    )
    .await;
    let mut options = test_options();
    options.github_api = Some(w.server.uri());
    options.github_token = Some("tok".into());
    let daemon = TestDaemon::start_with(home, options).await;

    let mut c = daemon.client().await;
    let refused = c
        .request(Command::AddDraftItem {
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
        .await;
    match refused {
        Err(ClientError::Server(e)) => assert_eq!(e.code, ErrorCode::Conflict, "{}", e.message),
        other => panic!("expected a conflict, got {other:?}"),
    }
    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(r) => {
            assert_eq!(
                r.head_sha, w.head,
                "the review stays on the head it was opened at"
            );
            assert!(
                r.draft.items.is_empty(),
                "nothing was anchored to the wrong files"
            );
        }
        other => panic!("{other:?}"),
    }
    daemon.stop().await;
}
