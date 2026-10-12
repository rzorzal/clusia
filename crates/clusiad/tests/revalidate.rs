mod common;

use std::time::Duration;

use clusia_core::revalidation::{Decision, RoundState, Verdict};
use clusia_core::{ItemStatus, ReviewState};
use clusia_harness::testkit::{FakeClaude, Script, Turn, revalidation_block};
use clusia_protocol::{Client, Command, Event, LoadStepKind, PermissionOutcome};
use common::agent::{calls, set_config};
use common::revalidation::*;
use common::review_world::{World, open, pr7, world};
use serde_json::json;

#[tokio::test]
async fn a_new_head_opens_a_round_and_holds_the_old_notification() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    use_script(&w, Script::one(answer(&ids))).await;
    let head = push(&w, &mut c, "zero\n").await;

    let review = review_of(&mut c).await;
    assert_eq!(review.state, ReviewState::Outdated);
    let round = review.revalidation.expect("a round");
    assert_eq!(
        (round.from_head.as_str(), round.to_head.as_str()),
        (w.head.as_str(), head.as_str())
    );
    assert!(matches!(round.state, RoundState::Scheduled { at } if at > 0));
    assert_eq!((round.sp1_moved, round.sp1_obsolete), (3, 0));
    assert_eq!(round.before.len(), 5);
    assert!(round.proposals.is_empty());
    assert_no_notice(&mut c).await;
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_finished_round_changes_no_item() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    let dir = use_script(&w, Script::one(answer(&ids))).await;
    push(&w, &mut c, "zero\n").await;
    let draft_before = review_of(&mut c).await.draft;
    let mut watcher = reviews_watcher(&w).await;

    c.request(Command::Revalidate { pr: pr7() }).await.unwrap();
    common::agent::until(&mut watcher, round_is(is_done)).await;

    let review = review_of(&mut c).await;
    assert_eq!(
        review.draft, draft_before,
        "nothing the agent said changed an item"
    );
    assert_eq!(review.state, ReviewState::Revalidated);
    let round = review.revalidation.expect("a round");
    assert_eq!(round.state, RoundState::Done);
    assert_eq!((round.asked, round.unreadable), (5, 4));
    assert!(round.finished_at.is_some());
    let verdict = |id: &str| {
        let p = round
            .proposals
            .iter()
            .find(|p| p.item == id)
            .expect("a proposal");
        assert_eq!(p.decision, Decision::Pending);
        p.verdict.clone()
    };
    match verdict(&ids.a) {
        Verdict::Moved { anchor, reason } => {
            assert_eq!((anchor.path.as_str(), anchor.line), (FILE, 1));
            assert_eq!(
                anchor.commit, round.to_head,
                "the daemon stamps the new head"
            );
            assert_eq!(reason, "The comment follows the same code.");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        verdict(&ids.b),
        Verdict::Obsolete {
            reason: "The line was deleted.".into()
        }
    );
    assert_eq!(
        verdict(&ids.n),
        Verdict::Reword {
            text: "Looks good; one nit.".into(),
            reason: Some("Sharper.".into())
        }
    );
    for id in [&ids.q, &ids.g] {
        assert_eq!(
            verdict(id),
            Verdict::NotChecked,
            "a move the diff or the item cannot take is dropped, and no block is never Kept"
        );
    }

    let call = calls(&dir, 1).await.remove(0);
    let prompt = common::agent::value_of(&call.argv, "-p").unwrap();
    assert!(
        prompt.contains(".clusia/review.md"),
        "a review with no session is told where the notes are"
    );
    assert!(
        prompt.contains("+zero"),
        "the prompt carries the old-to-new diff"
    );
    for id in [&ids.a, &ids.b, &ids.q, &ids.n, &ids.g] {
        assert!(prompt.contains(id.as_str()), "{id} is asked");
    }
    assert!(call.argv.iter().any(|a| a == "--session-id"));
    assert!(!call.argv.iter().any(|a| a == "--fork-session"));

    assert_eq!(
        notices(&mut c).await,
        ["Review #7 revalidated: 1 moved, 1 obsolete, 1 new wording"],
        "one notification for the round"
    );
    let (activity, _) = clusia_store::read_activity(&w.daemon.paths).unwrap();
    let row = activity
        .iter()
        .find(|a| a.kind == clusia_core::ActivityKind::ReviewRevalidated)
        .expect("an activity row");
    assert_eq!(
        row.note.as_deref(),
        Some("1 moved, 1 obsolete, 1 new wording")
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_new_head_drops_old_proposals_and_keeps_where_the_round_began() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    use_script(&w, Script::one(answer(&ids))).await;
    push(&w, &mut c, "zero\n").await;
    let mut watcher = reviews_watcher(&w).await;
    c.request(Command::Revalidate { pr: pr7() }).await.unwrap();
    common::agent::until(&mut watcher, round_is(is_done)).await;
    let first = review_of(&mut c).await.revalidation.unwrap();

    let head = push(&w, &mut c, "zero\nzero\n").await;

    let round = review_of(&mut c).await.revalidation.expect("a round");
    assert_eq!(
        round.from_head, w.head,
        "the round began at the head the user reviewed"
    );
    assert_eq!(round.to_head, head);
    assert!(
        round.proposals.is_empty(),
        "they were made against an older head"
    );
    assert!(matches!(round.state, RoundState::Scheduled { .. }));
    assert_eq!(
        round.before, first.before,
        "the anchors from before the first push stay"
    );
    assert_eq!(round.sp1_moved, first.sp1_moved * 2, "the counts add up");
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_push_in_the_quiet_period_restarts_it() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    let dir = use_script(&w, Script::one(answer(&ids))).await;
    push(&w, &mut c, "zero\n").await;
    let first = review_of(&mut c).await.revalidation.unwrap();
    let RoundState::Scheduled { at: first_at } = first.state else {
        panic!("scheduled");
    };

    let head = push(&w, &mut c, "zero\nzero\n").await;

    let second = review_of(&mut c).await.revalidation.unwrap();
    assert!(matches!(second.state, RoundState::Scheduled { at } if at >= first_at));
    assert_eq!(
        (second.from_head.as_str(), second.to_head.as_str()),
        (w.head.as_str(), head.as_str())
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        FakeClaude::calls(&dir).is_empty(),
        "nothing ran during the quiet period"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn the_switch_off_keeps_the_relocation_only() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    let dir = use_script(&w, Script::one(answer(&ids))).await;
    set_config(&w, "harness.revalidate", "false").await;
    push(&w, &mut c, "zero\n").await;

    let review = review_of(&mut c).await;
    assert!(review.revalidation.is_none());
    assert_eq!(review.state, ReviewState::Outdated);
    assert!(matches!(
        review.draft.items[0].status,
        ItemStatus::Moved { .. }
    ));
    assert_eq!(notices(&mut c).await, ["#7 is out of date"]);
    assert!(FakeClaude::calls(&dir).is_empty());
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_failed_run_sends_the_old_notification_and_a_retry_adds_no_second_one() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    use_script(
        &w,
        Script::turns(vec![
            Turn::lines(&[]).exit(1).stderr("boom\n"),
            answer(&ids),
        ]),
    )
    .await;
    push(&w, &mut c, "zero\n").await;
    let mut watcher = reviews_watcher(&w).await;

    c.request(Command::Revalidate { pr: pr7() }).await.unwrap();
    common::agent::until(&mut watcher, round_is(is_failed)).await;

    let round = review_of(&mut c).await.revalidation.unwrap();
    assert!(matches!(&round.state, RoundState::Failed { message } if message != "Stopped"));
    assert!(round.proposals.is_empty());
    assert!(round.notified);
    assert_eq!(notices(&mut c).await, ["#7 is out of date"]);

    // Try again: the round succeeds, and the user is not told a second time.
    c.request(Command::Revalidate { pr: pr7() }).await.unwrap();
    common::agent::until(&mut watcher, round_is(is_done)).await;
    assert_eq!(notices(&mut c).await, ["#7 is out of date"]);
    w.daemon.stop().await;
}

#[tokio::test]
async fn no_notification_while_a_window_holds_the_review() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    use_script(&w, Script::one(answer(&ids))).await;
    push(&w, &mut c, "zero\n").await;
    c.request(Command::HoldReview {
        pr: pr7(),
        held: true,
    })
    .await
    .unwrap();
    let mut watcher = reviews_watcher(&w).await;

    c.request(Command::Revalidate { pr: pr7() }).await.unwrap();
    common::agent::until(&mut watcher, round_is(is_done)).await;

    assert_no_notice(&mut c).await;
    w.daemon.stop().await;
}

#[tokio::test]
async fn stop_ends_the_run_and_cancels_its_permission_request() {
    let w = world().await;
    let (mut c, _ids) = saved(&w).await;
    let dir = use_script(
        &w,
        Script::one(
            Turn::answer("Ran.")
                .ask_permission("Bash", json!({"command": "cargo test -p clusia-core"}))
                .then_hang(),
        ),
    )
    .await;
    push(&w, &mut c, "zero\n").await;
    let mut agent = agent_watcher(&w).await;
    let mut watcher = reviews_watcher(&w).await;

    c.request(Command::Revalidate { pr: pr7() }).await.unwrap();
    let asked = common::agent::until(&mut agent, |e| {
        matches!(e, Event::PermissionRequested { .. })
    })
    .await;
    assert!(matches!(
        asked.last(),
        Some(Event::PermissionRequested {
            origin: clusia_protocol::TurnOrigin::Revalidation,
            ..
        })
    ));
    common::agent::until(&mut watcher, round_is(is_running)).await;
    let pid = calls(&dir, 1).await[0].pid;

    c.request(Command::StopRevalidation { pr: pr7() })
        .await
        .unwrap();
    common::agent::until(&mut watcher, round_is(is_failed)).await;
    common::agent::until(&mut agent, |e| {
        matches!(
            e,
            Event::PermissionResolved {
                outcome: PermissionOutcome::Cancelled,
                ..
            }
        )
    })
    .await;

    let round = review_of(&mut c).await.revalidation.unwrap();
    assert_eq!(
        round.state,
        RoundState::Failed {
            message: "Stopped".into()
        }
    );
    for _ in 0..60 {
        if !FakeClaude::is_running(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!FakeClaude::is_running(pid), "no claude is left");
    assert_no_notice(&mut c).await;
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_new_head_during_a_run_stops_it_and_starts_the_round_over() {
    let w = world().await;
    let (mut c, _ids) = saved(&w).await;
    let dir = use_script(&w, Script::one(Turn::hanging())).await;
    push(&w, &mut c, "zero\n").await;
    let mut watcher = reviews_watcher(&w).await;
    c.request(Command::Revalidate { pr: pr7() }).await.unwrap();
    common::agent::until(&mut watcher, round_is(is_running)).await;
    let pid = calls(&dir, 1).await[0].pid;

    let head = push(&w, &mut c, "zero\nzero\n").await;

    for _ in 0..60 {
        if !FakeClaude::is_running(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !FakeClaude::is_running(pid),
        "the run on the old head ended"
    );
    let round = review_of(&mut c).await.revalidation.unwrap();
    assert_eq!(round.to_head, head);
    assert!(
        matches!(round.state, RoundState::Scheduled { .. }),
        "{:?}",
        round.state
    );
    assert_no_notice(&mut c).await;
    w.daemon.stop().await;
}

#[tokio::test]
async fn discarding_a_review_stops_its_run() {
    let w = world().await;
    let (mut c, _ids) = saved(&w).await;
    let dir = use_script(&w, Script::one(Turn::hanging())).await;
    push(&w, &mut c, "zero\n").await;
    let mut watcher = reviews_watcher(&w).await;
    c.request(Command::Revalidate { pr: pr7() }).await.unwrap();
    common::agent::until(&mut watcher, round_is(is_running)).await;
    let pid = calls(&dir, 1).await[0].pid;

    c.request(Command::DiscardReview { pr: pr7() })
        .await
        .unwrap();

    for _ in 0..60 {
        if !FakeClaude::is_running(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!FakeClaude::is_running(pid), "no claude is left");
    assert!(c.request(Command::GetReview { pr: pr7() }).await.is_err());
    assert_no_notice(&mut c).await;
    w.daemon.stop().await;
}

#[tokio::test]
async fn opening_the_review_runs_the_round_at_once() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    use_script(&w, Script::one(answer(&ids))).await;
    push(&w, &mut c, "zero\n").await;
    let mut watcher = reviews_watcher(&w).await;

    let view = open(&mut c).await;
    assert_eq!(view.review.state, ReviewState::Active);
    let events = common::agent::until(&mut watcher, round_is(is_done)).await;

    let note = events.iter().rev().find_map(|e| match e {
        Event::LoadStep(s) if s.step == LoadStepKind::Agent => s.message.clone(),
        _ => None,
    });
    assert!(
        note.is_some_and(|n| n.contains("Revalidating your draft")),
        "the Agent row says it: {events:?}"
    );
    let round = review_of(&mut c).await.revalidation.unwrap();
    assert_eq!(round.state, RoundState::Done);
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_round_left_scheduled_by_a_restart_runs() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    let reword = Turn::answer_blocks(
        "Checked.",
        &[revalidation_block(json!({
            "item": ids.n, "verdict": "reword", "text": "Better."
        }))],
    );
    let dir = use_script(&w, Script::one(reword)).await;
    push(&w, &mut c, "zero\n").await;
    let World {
        daemon,
        tmp: _tmp,
        server: _server,
        ..
    } = w;
    let paths = daemon.paths.clone();
    let home = daemon.stop().await;

    // The crash left the round running; the next start runs it again.
    let clusia_store::ReviewLoad::Found(mut review) =
        clusia_store::load_review(&paths, &pr7()).unwrap()
    else {
        panic!("the saved review");
    };
    review.revalidation.as_mut().unwrap().state = RoundState::Running { activity: None };
    clusia_store::save_review(&paths, &review).unwrap();

    let daemon = common::TestDaemon::start_in(home).await;
    assert_eq!(
        calls(&dir, 1).await.len(),
        1,
        "the round ran without a command"
    );
    let mut c = daemon.client().await;
    for _ in 0..100 {
        if review_of(&mut c).await.revalidation.unwrap().state == RoundState::Done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let round = review_of(&mut c).await.revalidation.unwrap();
    assert_eq!(round.state, RoundState::Done);
    assert_eq!(round.proposals.len(), 5);
    daemon.stop().await;
}

#[tokio::test]
async fn one_notification_per_round() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    use_script(&w, Script::turns(vec![answer(&ids), answer(&ids)])).await;
    push(&w, &mut c, "zero\n").await;
    push(&w, &mut c, "zero\nzero\n").await;

    run_round(&w, &mut c).await;
    let first = review_of(&mut c).await.revalidation.unwrap();
    assert_eq!(first.from_head, w.head, "two pushes, one round");
    assert_eq!(
        notices(&mut c).await,
        ["Review #7 revalidated: 1 moved, 1 obsolete, 1 new wording"]
    );

    // A rerun replaces the proposals and says nothing more.
    run_round(&w, &mut c).await;
    let again = review_of(&mut c).await.revalidation.unwrap();
    assert_eq!(again.proposals.len(), 5);
    assert!(
        again
            .proposals
            .iter()
            .all(|p| p.decision == Decision::Pending)
    );
    assert_eq!(notices(&mut c).await.len(), 1);
    w.daemon.stop().await;
}

/// A saved review whose first comment lost its line to the new commit, and a run that hangs.
async fn with_a_hanging_round(w: &World) -> (Client, Ids, std::path::PathBuf) {
    let (mut c, ids) = saved(w).await;
    let dir = use_script(
        w,
        Script::turns(vec![
            Turn::hanging(),
            Turn::answer_blocks(
                "Checked.",
                &[revalidation_block(json!({
                    "item": ids.n, "verdict": "reword", "text": "Better."
                }))],
            ),
        ]),
    )
    .await;
    push_content(w, &mut c, "zero\none\nthree\n", None).await;
    (c, ids, dir)
}

fn publish_comment() -> Command {
    Command::Publish {
        pr: pr7(),
        verdict: clusia_core::Verdict::Comment,
        summary: String::new(),
    }
}

#[tokio::test]
async fn a_refused_publish_leaves_the_round_able_to_run() {
    let w = world().await;
    let (mut c, _ids, dir) = with_a_hanging_round(&w).await;

    // A scheduled round stays scheduled when the publish is refused.
    assert!(c.request(publish_comment()).await.is_err());
    let round = review_of(&mut c).await.revalidation.unwrap();
    assert!(
        matches!(round.state, RoundState::Scheduled { .. }),
        "{:?}",
        round.state
    );

    // A running round ends as a stop, and can run again.
    let mut watcher = reviews_watcher(&w).await;
    c.request(Command::Revalidate { pr: pr7() }).await.unwrap();
    common::agent::until(&mut watcher, round_is(is_running)).await;
    let pid = calls(&dir, 1).await[0].pid;
    assert!(c.request(publish_comment()).await.is_err());
    common::agent::until(&mut watcher, round_is(is_failed)).await;
    let round = review_of(&mut c).await.revalidation.unwrap();
    assert_eq!(
        round.state,
        RoundState::Failed {
            message: "Stopped".into()
        }
    );
    for _ in 0..60 {
        if !FakeClaude::is_running(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!FakeClaude::is_running(pid), "no claude is left");

    c.request(Command::Revalidate { pr: pr7() }).await.unwrap();
    common::agent::until(&mut watcher, round_is(is_done)).await;
    w.daemon.stop().await;
}

#[tokio::test]
async fn publishing_stops_a_running_round_and_forgets_it() {
    let w = world().await;
    let (mut c, _ids) = saved(&w).await;
    let dir = use_script(&w, Script::one(Turn::hanging())).await;
    push(&w, &mut c, "zero\n").await;
    let mut watcher = reviews_watcher(&w).await;
    // The window opens the review, which starts the round at once.
    open(&mut c).await;
    common::agent::until(&mut watcher, round_is(is_running)).await;
    let pid = calls(&dir, 1).await[0].pid;
    common::github_mock::mount_publish(&w.server, "https://github.com/acme/widgets/pull/7#r").await;

    c.request(publish_comment())
        .await
        .expect("the publish goes through");

    common::agent::until(&mut watcher, |e| {
        matches!(
            e,
            Event::RevalidationChanged {
                revalidation: None,
                ..
            }
        )
    })
    .await;
    for _ in 0..60 {
        if !FakeClaude::is_running(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!FakeClaude::is_running(pid), "no claude is left");
    assert!(c.request(Command::GetReview { pr: pr7() }).await.is_err());
    assert_no_notice(&mut c).await;
    w.daemon.stop().await;
}

#[tokio::test]
async fn shutdown_leaves_no_revalidation_behind() {
    let w = world().await;
    let (mut c, _ids) = saved(&w).await;
    let dir = use_script(&w, Script::one(Turn::hanging())).await;
    push(&w, &mut c, "zero\n").await;
    let mut watcher = reviews_watcher(&w).await;
    c.request(Command::Revalidate { pr: pr7() }).await.unwrap();
    common::agent::until(&mut watcher, round_is(is_running)).await;
    let pid = calls(&dir, 1).await[0].pid;
    let World {
        daemon,
        tmp: _tmp,
        server: _server,
        ..
    } = w;
    let paths = daemon.paths.clone();

    let _home = daemon.stop().await;

    for _ in 0..60 {
        if !FakeClaude::is_running(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!FakeClaude::is_running(pid), "no claude is left");
    let clusia_store::ReviewLoad::Found(review) =
        clusia_store::load_review(&paths, &pr7()).unwrap()
    else {
        panic!("the saved review");
    };
    assert!(
        matches!(
            review.revalidation.unwrap().state,
            RoundState::Running { .. }
        ),
        "the round stays as it was, for the next start to run again"
    );
}

#[tokio::test]
async fn a_timer_that_cannot_start_fails_the_round_and_sends_the_old_notification() {
    let w = world().await;
    let (mut c, _ids) = saved(&w).await;
    use_script(&w, Script::one(Turn::hanging())).await;
    push(&w, &mut c, "zero\n").await;
    let World {
        daemon,
        tmp: _tmp,
        server: _server,
        ..
    } = w;
    let paths = daemon.paths.clone();
    let home = daemon.stop().await;
    let clusia_store::ReviewLoad::Found(mut review) =
        clusia_store::load_review(&paths, &pr7()).unwrap()
    else {
        panic!("the saved review");
    };
    review.revalidation.as_mut().unwrap().state = RoundState::Scheduled { at: 0 };
    clusia_store::save_review(&paths, &review).unwrap();
    std::fs::remove_dir_all(paths.worktree_for(&pr7())).unwrap();

    let daemon = common::TestDaemon::start_in(home).await;

    let mut c = daemon.client().await;
    for _ in 0..100 {
        if is_failed(&review_of(&mut c).await.revalidation.unwrap().state) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let round = review_of(&mut c).await.revalidation.unwrap();
    assert!(
        matches!(&round.state, RoundState::Failed { message } if message.contains("worktree")),
        "{:?}",
        round.state
    );
    assert_eq!(notices(&mut c).await, ["#7 is out of date"]);
    daemon.stop().await;
}

#[tokio::test]
async fn a_window_opening_a_new_head_while_a_run_winds_down_still_runs_the_round() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    use_script(
        &w,
        Script::turns(vec![Turn::hanging().ignore_term(), answer(&ids)]),
    )
    .await;
    push(&w, &mut c, "zero\n").await;
    let mut watcher = reviews_watcher(&w).await;
    c.request(Command::Revalidate { pr: pr7() }).await.unwrap();
    common::agent::until(&mut watcher, round_is(is_running)).await;

    // The program ignores the first request to end, so the run winds down for a while.
    // The daemon has not polled the newer head: the window finds it while the run still lives.
    let head = serve_head(&w, "zero\nzero\none\ntwo\nthree\n", None).await;
    open(&mut c).await;
    common::agent::until(&mut watcher, round_is(is_done)).await;

    let round = review_of(&mut c).await.revalidation.unwrap();
    assert_eq!(round.to_head, head);
    assert_eq!(round.state, RoundState::Done);
    w.daemon.stop().await;
}

#[tokio::test]
async fn stopping_a_round_the_window_is_starting_keeps_it_stopped() {
    let w = world().await;
    let (mut c, _ids) = saved(&w).await;
    let dir = use_script(&w, Script::one(Turn::hanging())).await;
    push(&w, &mut c, "zero\n").await;
    // The start reads the commits before it places the run: slow them down.
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/repos/acme/widgets/pulls/7/commits",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(json!([]))
                .set_delay(Duration::from_millis(800)),
        )
        .with_priority(1)
        .mount(&w.server)
        .await;
    let mut watcher = reviews_watcher(&w).await;

    open(&mut c).await;
    c.request(Command::StopRevalidation { pr: pr7() })
        .await
        .unwrap();
    common::agent::until(&mut watcher, round_is(is_failed)).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let round = review_of(&mut c).await.revalidation.unwrap();
    assert_eq!(
        round.state,
        RoundState::Failed {
            message: "Stopped".into()
        }
    );
    // Opening the review starts the summary too: only a revalidation prompt would be wrong.
    let revalidations = FakeClaude::calls(&dir)
        .into_iter()
        .filter(|call| {
            common::agent::value_of(&call.argv, "-p")
                .is_some_and(|prompt| prompt.contains("clusia-revalidation"))
        })
        .count();
    assert_eq!(revalidations, 0, "the stopped round did not run");
    w.daemon.stop().await;
}
