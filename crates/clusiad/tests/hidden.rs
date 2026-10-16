//! Hidden comments: kept per pull request on this machine, past closing the review and
//! restarting the daemon, and never sent to GitHub.

mod common;

use clusia_core::HiddenComments;
use clusia_protocol::{Client, Command, Reply};
use common::TestDaemon;
use common::review_world::{open, pr7, world};

async fn hidden(c: &mut Client) -> HiddenComments {
    match c
        .request(Command::GetHiddenComments { pr: pr7() })
        .await
        .unwrap()
    {
        Reply::HiddenComments(set) => set,
        other => panic!("expected HiddenComments, got {other:?}"),
    }
}

async fn set_hidden(c: &mut Client, key: &str, posts: Option<u32>) -> HiddenComments {
    match c
        .request(Command::SetCommentHidden {
            pr: pr7(),
            key: key.into(),
            posts,
        })
        .await
        .unwrap()
    {
        Reply::HiddenComments(set) => set,
        other => panic!("expected HiddenComments, got {other:?}"),
    }
}

#[tokio::test]
async fn hiding_and_showing_a_comment_answers_with_the_set() {
    let daemon = TestDaemon::start().await;
    let mut c = daemon.client().await;
    assert!(hidden(&mut c).await.is_empty());
    let set = set_hidden(&mut c, "comment:11", Some(1)).await;
    assert_eq!(set.hidden.get("comment:11"), Some(&1));
    set_hidden(&mut c, "thread:PRRT_1", Some(3)).await;
    assert_eq!(hidden(&mut c).await.hidden.len(), 2);
    let set = set_hidden(&mut c, "comment:11", None).await;
    assert_eq!(set.hidden.keys().collect::<Vec<_>>(), ["thread:PRRT_1"]);
    set_hidden(&mut c, "thread:PRRT_1", None).await;
    assert!(hidden(&mut c).await.is_empty());
    assert!(!daemon.paths.hidden_file(&pr7()).exists());
}

#[tokio::test]
async fn a_hidden_comment_survives_closing_the_review_and_restarting_the_daemon() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    set_hidden(&mut c, "comment:11", Some(1)).await;
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    assert!(
        !w.daemon.paths.review_file(&pr7()).exists(),
        "an empty review is deleted on close"
    );
    assert!(hidden(&mut c).await.hidden.contains_key("comment:11"));
    drop(c);
    let dir = w.daemon.stop().await;
    let daemon = TestDaemon::start_in(dir).await;
    let mut c = daemon.client().await;
    assert!(hidden(&mut c).await.hidden.contains_key("comment:11"));
}

#[tokio::test]
async fn hiding_sends_nothing_to_github() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let before = w.server.received_requests().await.unwrap().len();
    set_hidden(&mut c, "thread:PRRT_1", Some(2)).await;
    hidden(&mut c).await;
    set_hidden(&mut c, "thread:PRRT_1", None).await;
    assert_eq!(w.server.received_requests().await.unwrap().len(), before);
}
