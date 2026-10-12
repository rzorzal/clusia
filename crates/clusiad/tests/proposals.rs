mod common;

use clusia_core::revalidation::{Decision, RoundState};
use clusia_core::{DraftItem, ItemStatus, Review, ReviewState, Side, Verdict as ReviewVerdict};
use clusia_harness::testkit::{Script, Turn, revalidation_block};
use clusia_protocol::{
    AnchorInput, Client, ClientError, Command, ErrorCode, Event, ProtocolError, Reply,
};
use common::github_mock::{graphql_requests, mount_publish};
use common::revalidation::*;
use common::review_world::{pr7, world};
use serde_json::json;

const REVIEW_URL: &str = "https://github.com/acme/widgets/pull/7#pullrequestreview-42";

fn item<'a>(review: &'a Review, id: &str) -> &'a DraftItem {
    review.draft.get(id).expect("the item is in the draft")
}

fn decision(review: &Review, id: &str) -> Decision {
    review
        .revalidation
        .as_ref()
        .and_then(|r| r.proposals.iter().find(|p| p.item == id))
        .map(|p| p.decision)
        .expect("a proposal")
}

async fn decide(c: &mut Client, id: &str, accept: bool) -> Result<Review, ProtocolError> {
    let reply = c
        .request(Command::DecideProposal {
            pr: pr7(),
            item: id.into(),
            accept,
        })
        .await;
    match reply {
        Ok(Reply::ReviewFile(review)) => Ok(*review),
        Err(ClientError::Server(e)) => Err(e),
        other => panic!("unexpected {other:?}"),
    }
}

async fn accept_all(c: &mut Client) -> Review {
    match c
        .request(Command::AcceptAllProposals { pr: pr7() })
        .await
        .unwrap()
    {
        Reply::ReviewFile(review) => *review,
        other => panic!("unexpected {other:?}"),
    }
}

fn line_input(line: u32) -> Option<AnchorInput> {
    Some(AnchorInput {
        path: FILE.into(),
        line,
        start_line: None,
        side: Side::Right,
    })
}

#[tokio::test]
async fn the_round_is_read_with_get_and_lists_as_revalidated() {
    let quiet = world().await;
    let (mut c, _ids) = saved(&quiet).await;
    assert_eq!(
        c.request(Command::GetRevalidation { pr: pr7() })
            .await
            .unwrap(),
        Reply::Revalidation(None)
    );
    quiet.daemon.stop().await;
    let w = world().await;
    let (mut c, _ids) = revalidated(&w).await;
    let Reply::Revalidation(Some(round)) = c
        .request(Command::GetRevalidation { pr: pr7() })
        .await
        .unwrap()
    else {
        panic!("a round");
    };
    assert_eq!(round.state, RoundState::Done);
    assert_eq!(round.proposals.len(), 5);
    let Reply::Reviews(list) = c.request(Command::ListReviews).await.unwrap() else {
        panic!("the reviews");
    };
    assert_eq!(
        list[0].state,
        ReviewState::Revalidated,
        "Home and the tray read revalidated"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn keeping_a_move_moves_the_comment_and_tells_the_windows() {
    let w = world().await;
    let (mut c, ids) = revalidated(&w).await;
    let mut watcher = reviews_watcher(&w).await;

    let review = decide(&mut c, &ids.a, true).await.unwrap();

    let a = item(&review, &ids.a);
    let anchor = a.anchor.as_ref().unwrap();
    assert_eq!((anchor.path.as_str(), anchor.line), (FILE, 1));
    assert_eq!(anchor.commit, review.head_sha);
    assert_eq!(
        a.status,
        ItemStatus::Moved {
            from_path: FILE.into(),
            from_line: 2
        },
        "moved from where it was before the round, not from where SP1 put it"
    );
    assert_eq!(decision(&review, &ids.a), Decision::Accepted);
    assert_eq!(
        review,
        review_of(&mut c).await,
        "the reply is the stored review"
    );
    let events = common::agent::until(&mut watcher, round_is(|_| true)).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ReviewChanged { .. }))
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn putting_a_move_back_restores_the_old_anchor() {
    let w = world().await;
    let (mut c, ids) = revalidated(&w).await;

    let review = decide(&mut c, &ids.a, false).await.unwrap();

    let a = item(&review, &ids.a);
    assert_eq!(a.anchor.as_ref().unwrap().line, 2);
    assert_eq!(a.status, ItemStatus::Ok);
    assert_eq!(decision(&review, &ids.a), Decision::Rejected);
    w.daemon.stop().await;
}

#[tokio::test]
async fn put_back_needs_a_line_in_the_diff() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    use_script(
        &w,
        Script::one(Turn::answer_blocks(
            "Checked.",
            &[revalidation_block(json!({
                "item": ids.q, "verdict": "moved", "path": FILE, "line": 3, "reason": "Moved down."
            }))],
        )),
    )
    .await;
    // Only lines 3 and 4 can carry a comment at the new head, so line 1 cannot come back.
    push_content(
        &w,
        &mut c,
        "zero\none\ntwo\nthree\n",
        Some("@@ -0,0 +3,2 @@\n+two\n+three"),
    )
    .await;
    run_round(&w, &mut c).await;

    let refused = decide(&mut c, &ids.q, false).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidState);
    assert!(
        refused.message.contains("feature.txt:1"),
        "{}",
        refused.message
    );
    let review = review_of(&mut c).await;
    assert_eq!(
        decision(&review, &ids.q),
        Decision::Pending,
        "nothing was decided"
    );

    let review = decide(&mut c, &ids.q, true).await.unwrap();
    assert_eq!(item(&review, &ids.q).anchor.as_ref().unwrap().line, 3);
    w.daemon.stop().await;
}

#[tokio::test]
async fn removing_a_comment_keeps_it_as_obsolete() {
    let w = world().await;
    let (mut c, ids) = revalidated(&w).await;

    let review = decide(&mut c, &ids.b, true).await.unwrap();

    assert_eq!(
        item(&review, &ids.b).status,
        ItemStatus::Obsolete {
            reason: "The line was deleted.".into()
        }
    );
    assert!(
        review.draft.get(&ids.b).is_some(),
        "nothing is ever deleted"
    );
    assert!(review.draft.publishable().all(|i| i.id != ids.b));
    w.daemon.stop().await;
}

#[tokio::test]
async fn rejecting_a_removal_or_a_new_wording_changes_nothing() {
    let w = world().await;
    let (mut c, ids) = revalidated(&w).await;
    let before = review_of(&mut c).await;

    let review = decide(&mut c, &ids.b, false).await.unwrap();
    let review = {
        drop(review);
        decide(&mut c, &ids.n, false).await.unwrap()
    };

    assert_eq!(item(&review, &ids.b), item(&before, &ids.b));
    assert_eq!(item(&review, &ids.n), item(&before, &ids.n));
    assert_eq!(decision(&review, &ids.b), Decision::Rejected);
    assert_eq!(decision(&review, &ids.n), Decision::Rejected);
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_new_wording_replaces_the_body_when_accepted() {
    let w = world().await;
    let (mut c, ids) = revalidated(&w).await;

    let review = decide(&mut c, &ids.n, true).await.unwrap();

    assert_eq!(item(&review, &ids.n).body, "Looks good; one nit.");
    w.daemon.stop().await;
}

#[tokio::test]
async fn kept_and_not_checked_rows_have_nothing_to_decide() {
    let w = world().await;
    let (mut c, ids) = revalidated(&w).await;

    for id in [&ids.q, &ids.g] {
        let refused = decide(&mut c, id, true).await.unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidState, "{id}");
    }
    let refused = decide(&mut c, "nope", true).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::NotFound);
    w.daemon.stop().await;
}

#[tokio::test]
async fn deciding_a_changed_item_is_refused() {
    let w = world().await;
    let (mut c, ids) = revalidated(&w).await;
    c.request(Command::UpdateDraftItem {
        pr: pr7(),
        id: ids.n.clone(),
        body: "My own words.".into(),
    })
    .await
    .unwrap();
    c.request(Command::RemoveDraftItem {
        pr: pr7(),
        id: ids.a.clone(),
    })
    .await
    .unwrap();

    for id in [&ids.n, &ids.a] {
        let refused = decide(&mut c, id, true).await.unwrap_err();
        assert_eq!(refused.code, ErrorCode::Conflict, "{id}");
        assert!(
            refused.message.contains("changed since the suggestion"),
            "{}",
            refused.message
        );
    }
    let review = review_of(&mut c).await;
    assert_eq!(
        item(&review, &ids.n).body,
        "My own words.",
        "the edit stays"
    );
    assert_eq!(decision(&review, &ids.n), Decision::Pending);
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_decision_cannot_be_taken_twice() {
    let w = world().await;
    let (mut c, ids) = revalidated(&w).await;
    decide(&mut c, &ids.b, true).await.unwrap();
    let again = decide(&mut c, &ids.b, false).await.unwrap_err();
    assert_eq!(again.code, ErrorCode::InvalidState);
    w.daemon.stop().await;
}

#[tokio::test]
async fn accept_all_applies_what_can_be_applied_and_skips_what_changed() {
    let w = world().await;
    let (mut c, ids) = revalidated(&w).await;
    c.request(Command::UpdateDraftItem {
        pr: pr7(),
        id: ids.n.clone(),
        body: "My own words.".into(),
    })
    .await
    .unwrap();

    let review = accept_all(&mut c).await;

    assert_eq!(item(&review, &ids.a).anchor.as_ref().unwrap().line, 1);
    assert!(matches!(
        item(&review, &ids.b).status,
        ItemStatus::Obsolete { .. }
    ));
    assert_eq!(
        item(&review, &ids.n).body,
        "My own words.",
        "a stale suggestion never lands"
    );
    assert_eq!(decision(&review, &ids.a), Decision::Accepted);
    assert_eq!(decision(&review, &ids.b), Decision::Accepted);
    assert_eq!(decision(&review, &ids.n), Decision::Pending);
    assert_eq!(
        item(&review, &ids.q).anchor.as_ref().unwrap().line,
        2,
        "not checked: untouched"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn publish_after_accept_all_sends_valid_anchors() {
    let w = world().await;
    let (mut c, ids) = revalidated(&w).await;
    // The push replaced GitHub's mocks, so the publish is served after it.
    mount_publish(&w.server, REVIEW_URL).await;
    accept_all(&mut c).await;

    c.request(Command::Publish {
        pr: pr7(),
        verdict: ReviewVerdict::Comment,
        summary: String::new(),
    })
    .await
    .expect("a draft with a removed comment publishes");

    let started = graphql_requests(&w.server, "addPullRequestReview(").await;
    let threads = started[0]["variables"]["input"]["threads"]
        .as_array()
        .unwrap();
    let lines: Vec<(u64, &str)> = threads
        .iter()
        .map(|t| (t["line"].as_u64().unwrap(), t["body"].as_str().unwrap()))
        .collect();
    assert_eq!(
        lines,
        [(1, "Rename this."), (2, "Why one?")],
        "the moved comment sits on a line of the new diff and the removed one is left out"
    );
    let submitted = graphql_requests(&w.server, "submitPullRequestReview").await;
    assert!(
        submitted[0]["variables"]["input"]["body"]
            .as_str()
            .unwrap()
            .contains("Looks good; one nit."),
        "the new wording is what goes out"
    );
    assert!(!ids.b.is_empty());
    assert!(
        c.request(Command::GetRevalidation { pr: pr7() })
            .await
            .is_err()
    );
    w.daemon.stop().await;
}

/// A review whose first comment loses its line to the new commit: the agent calls it obsolete.
async fn with_an_obsolete_comment(w: &common::review_world::World) -> (Client, Ids) {
    let (mut c, ids) = saved(w).await;
    use_script(
        w,
        Script::one(Turn::answer_blocks(
            "Checked.",
            &[revalidation_block(json!({
                "item": ids.a, "verdict": "obsolete", "reason": "The line was deleted."
            }))],
        )),
    )
    .await;
    push_content(w, &mut c, "zero\none\nthree\n", None).await;
    run_round(w, &mut c).await;
    mount_publish(&w.server, REVIEW_URL).await;
    assert!(matches!(
        item(&review_of(&mut c).await, &ids.a).status,
        ItemStatus::Obsolete { .. }
    ));
    (c, ids)
}

fn publish() -> Command {
    Command::Publish {
        pr: pr7(),
        verdict: ReviewVerdict::Comment,
        summary: String::new(),
    }
}

#[tokio::test]
async fn an_accepted_removal_stops_blocking_the_publish() {
    let w = world().await;
    let (mut c, ids) = with_an_obsolete_comment(&w).await;
    match c.request(publish()).await {
        Err(ClientError::Server(e)) => {
            assert_eq!(e.code, ErrorCode::BadRequest);
            assert!(e.message.contains("obsolete"), "{}", e.message);
        }
        other => panic!("an undecided obsolete comment blocks: {other:?}"),
    }

    decide(&mut c, &ids.a, true).await.unwrap();

    c.request(publish())
        .await
        .expect("a removed comment does not block");
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_rejected_removal_blocks_until_the_comment_is_anchored_again() {
    let w = world().await;
    let (mut c, ids) = with_an_obsolete_comment(&w).await;
    let review = decide(&mut c, &ids.a, false).await.unwrap();
    assert!(matches!(
        item(&review, &ids.a).status,
        ItemStatus::Obsolete { .. }
    ));
    assert!(matches!(
        c.request(publish()).await,
        Err(ClientError::Server(e)) if e.code == ErrorCode::BadRequest
    ));

    let Reply::DraftItem(moved) = c
        .request(Command::ReanchorDraftItem {
            pr: pr7(),
            id: ids.a.clone(),
            anchor: line_input(3),
        })
        .await
        .unwrap()
    else {
        panic!("the item");
    };

    assert_eq!(moved.status, ItemStatus::Ok);
    assert_eq!(moved.anchor.as_ref().unwrap().line, 3);
    c.request(publish())
        .await
        .expect("an anchored comment publishes");
    w.daemon.stop().await;
}

#[tokio::test]
async fn reanchoring_takes_a_commentable_line_or_makes_a_general_comment() {
    let w = world().await;
    let (mut c, ids) = revalidated(&w).await;
    let mut watcher = reviews_watcher(&w).await;
    let reanchor = |id: &str, anchor| Command::ReanchorDraftItem {
        pr: pr7(),
        id: id.into(),
        anchor,
    };

    let Reply::DraftItem(item) = c.request(reanchor(&ids.a, line_input(4))).await.unwrap() else {
        panic!("the item");
    };
    assert_eq!(item.anchor.as_ref().unwrap().line, 4);
    let events =
        common::agent::until(&mut watcher, |e| matches!(e, Event::ReviewChanged { .. })).await;
    assert!(!events.is_empty());

    match c.request(reanchor(&ids.a, line_input(99))).await {
        Err(ClientError::Server(e)) => assert_eq!(e.code, ErrorCode::BadRequest),
        other => panic!("a line outside the diff is refused: {other:?}"),
    }
    match c.request(reanchor(&ids.n, line_input(2))).await {
        Err(ClientError::Server(e)) => assert_eq!(e.code, ErrorCode::BadRequest),
        other => panic!("a general comment takes no line: {other:?}"),
    }

    let Reply::DraftItem(general) = c.request(reanchor(&ids.b, None)).await.unwrap() else {
        panic!("the item");
    };
    assert_eq!(general.anchor, None);
    assert_eq!(general.status, ItemStatus::Ok);
    w.daemon.stop().await;
}

#[tokio::test]
async fn accepting_new_wording_on_an_obsolete_comment_still_blocks_the_publish() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    use_script(
        &w,
        Script::one(Turn::answer_blocks(
            "Checked.",
            &[revalidation_block(json!({
                "item": ids.a, "verdict": "reword", "text": "Better words."
            }))],
        )),
    )
    .await;
    push_content(&w, &mut c, "zero\none\nthree\n", None).await;
    run_round(&w, &mut c).await;
    mount_publish(&w.server, REVIEW_URL).await;

    let review = decide(&mut c, &ids.a, true).await.unwrap();

    assert_eq!(item(&review, &ids.a).body, "Better words.");
    assert!(
        matches!(item(&review, &ids.a).status, ItemStatus::Obsolete { .. }),
        "new wording is not a removal"
    );
    assert!(review.removed.is_empty());
    assert!(matches!(
        c.request(publish()).await,
        Err(ClientError::Server(e)) if e.code == ErrorCode::BadRequest
    ));
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_removed_comment_stays_removed_when_the_round_runs_again() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    let obsolete = || {
        Turn::answer_blocks(
            "Checked.",
            &[revalidation_block(json!({
                "item": ids.a, "verdict": "obsolete", "reason": "The line was deleted."
            }))],
        )
    };
    use_script(&w, Script::turns(vec![obsolete(), obsolete()])).await;
    push_content(&w, &mut c, "zero\none\nthree\n", None).await;
    run_round(&w, &mut c).await;
    mount_publish(&w.server, REVIEW_URL).await;
    decide(&mut c, &ids.a, true).await.unwrap();

    run_round(&w, &mut c).await;

    let review = review_of(&mut c).await;
    assert_eq!(
        decision(&review, &ids.a),
        Decision::Pending,
        "the rerun made new proposals"
    );
    assert!(
        review.removed.contains(&ids.a),
        "what the user removed stays removed"
    );
    c.request(publish())
        .await
        .expect("the removed comment still does not block");
    w.daemon.stop().await;
}

#[tokio::test]
async fn restoring_or_deleting_a_removed_comment_takes_it_off_the_list() {
    let w = world().await;
    let (mut c, ids) = with_an_obsolete_comment(&w).await;
    decide(&mut c, &ids.a, true).await.unwrap();
    assert!(review_of(&mut c).await.removed.contains(&ids.a));

    c.request(Command::ReanchorDraftItem {
        pr: pr7(),
        id: ids.a.clone(),
        anchor: line_input(3),
    })
    .await
    .unwrap();
    assert!(review_of(&mut c).await.removed.is_empty(), "restored");
    w.daemon.stop().await;

    let w = world().await;
    let (mut c, ids) = with_an_obsolete_comment(&w).await;
    decide(&mut c, &ids.a, true).await.unwrap();
    c.request(Command::RemoveDraftItem {
        pr: pr7(),
        id: ids.a.clone(),
    })
    .await
    .unwrap();
    assert!(review_of(&mut c).await.removed.is_empty(), "deleted");
    w.daemon.stop().await;
}
