mod common;

use std::path::PathBuf;
use std::time::Duration;

use clusia_core::{DraftKind, EventKind, Origin, ReviewState, Side, Verdict};
use clusia_harness::testkit::{FakeClaude, Script, Turn};
use clusia_protocol::{
    AgentLogEntry, AnchorInput, Command, ErrorCode, Event, LoadStepKind, Reply, StepStatus, topics,
};
use common::agent::{
    calls, fake_program, ready, refused, send, session_ended, until, use_fake, value_of, watching,
};
use common::git_fixture::{advance_pr, sh};
use common::github_mock::{PrMock, mount_pr, mount_publish};
use common::review_world::{open, pr7, world};

const SUGGESTING: &str = "One thing.\n```clusia-suggestion\n{\"file\":\"feature.txt\",\"line\":2,\"body\":\"Rename this\"}\n```\n";
const REVIEW_URL: &str = "https://github.com/acme/widgets/pull/7#pullrequestreview-42";

fn suggestion_id(events: &[Event]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            Event::AgentSuggestion { suggestion, .. } => Some(suggestion.id.clone()),
            _ => None,
        })
        .expect("a suggestion arrived")
}

fn suggestions_in(entries: &[AgentLogEntry]) -> usize {
    entries
        .iter()
        .filter(|e| matches!(e, AgentLogEntry::Suggestion { .. }))
        .count()
}

#[tokio::test]
async fn the_review_notes_are_written_refreshed_and_kept_out_of_git() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    let worktree = PathBuf::from(open(&mut c).await.worktree.unwrap());
    let notes = worktree.join(".clusia/review.md");
    let text = std::fs::read_to_string(&notes).expect("the notes are written on open");
    assert!(text.contains("Repository: acme/widgets"), "{text}");
    assert!(text.contains("Pull request: #7 Add feature"), "{text}");
    c.request(Command::AddDraftItem {
        pr: pr7(),
        kind: DraftKind::General,
        anchor: None,
        thread: None,
        body: "Check the cache size".into(),
    })
    .await
    .unwrap();
    let text = std::fs::read_to_string(&notes).unwrap();
    assert!(text.contains("Check the cache size"), "{text}");
    assert_eq!(
        sh(&worktree, &["status", "--porcelain"]),
        "",
        "the notes never show up as a change"
    );
}

#[tokio::test]
async fn waiting_for_the_first_question_starts_no_turn() {
    let w = world().await;
    let dir = use_fake(&w, Script::one(Turn::answer("x"))).await;
    let mut watcher = w.daemon.client().await;
    watcher
        .request(Command::Subscribe {
            topics: vec![topics::REVIEWS.into()],
        })
        .await
        .unwrap();
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let mut agent_step = None;
    while let Ok(Ok((_, event))) =
        tokio::time::timeout(Duration::from_millis(300), watcher.next_event()).await
    {
        if let Event::LoadStep(step) = event
            && step.step == LoadStepKind::Agent
        {
            agent_step = Some((step.status, step.message));
        }
    }
    assert_eq!(
        agent_step,
        Some((
            StepStatus::Skipped,
            Some("Waiting for your first question".into())
        ))
    );
    assert!(FakeClaude::calls(&dir).is_empty());
}

#[tokio::test]
async fn reopen_resumes_and_resummarizes_on_new_head() {
    let w = world().await;
    let dir = fake_program(&w, Script::one(Turn::answer("The summary."))).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    until(&mut watcher, ready).await;
    let first = calls(&dir, 1).await.remove(0);
    let session = value_of(&first.argv, "--session-id").expect("the first turn starts a session");
    let prompt = value_of(&first.argv, "-p").unwrap();
    assert!(
        prompt.starts_with("Summarize this pull request"),
        "{prompt}"
    );
    let Reply::ReviewFile(review) = c.request(Command::GetReview { pr: pr7() }).await.unwrap()
    else {
        panic!("a review file");
    };
    assert_eq!(review.harness_session.as_deref(), Some(session.as_str()));
    let Reply::AgentLog(entries) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap()
    else {
        panic!("an agent log");
    };
    assert!(
        !entries
            .iter()
            .any(|e| matches!(e, AgentLogEntry::User { .. })),
        "the summary prompt is not something the user said"
    );

    // The same head: the agent already read it.
    open(&mut c).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(FakeClaude::calls(&dir).len(), 1, "no second summary");

    // A new head: one more summary, in the same session.
    let new_head = advance_pr(w.tmp.path(), 7, "feature.txt", "zero\none\ntwo\nthree\n");
    w.server.reset().await;
    mount_pr(
        &w.server,
        &PrMock::new(&new_head, &w.base, &w.origin).adding_feature("zero\none\ntwo\nthree\n"),
    )
    .await;
    open(&mut c).await;
    until(&mut watcher, ready).await;
    let second = calls(&dir, 2).await.remove(1);
    assert_eq!(value_of(&second.argv, "--resume"), Some(session));
    assert_eq!(value_of(&second.argv, "--session-id"), None);
    assert!(
        value_of(&second.argv, "-p")
            .unwrap()
            .contains("new commits")
    );
}

#[tokio::test]
async fn accepting_a_suggestion_adds_an_agent_item() {
    let w = world().await;
    use_fake(&w, Script::one(Turn::answer(SUGGESTING))).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let mut watcher = watching(&w).await;
    c.request(send("review it")).await.unwrap();
    let id = suggestion_id(&until(&mut watcher, ready).await);

    let Reply::DraftItem(item) = c
        .request(Command::AcceptSuggestion {
            pr: pr7(),
            id: id.clone(),
            body: Some("Please rename this".into()),
        })
        .await
        .unwrap()
    else {
        panic!("a draft item");
    };
    assert_eq!(item.origin, Origin::Agent);
    assert_eq!(item.kind, DraftKind::LineComment);
    assert!(item.accepted);
    assert_eq!(item.body, "Please rename this");
    assert_eq!(item.anchor.as_ref().map(|a| a.line), Some(2));

    let again = refused(
        c.request(Command::AcceptSuggestion {
            pr: pr7(),
            id: id.clone(),
            body: None,
        })
        .await,
    );
    assert_eq!(again.code, ErrorCode::NotFound);
    let Reply::AgentLog(entries) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap()
    else {
        panic!("an agent log");
    };
    assert_eq!(suggestions_in(&entries), 0, "accepted: no longer waiting");
    let Reply::ReviewFile(review) = c.request(Command::GetReview { pr: pr7() }).await.unwrap()
    else {
        panic!("a review file");
    };
    assert_eq!(review.draft.items.len(), 1);
}

#[tokio::test]
async fn a_suggestion_outside_the_diff_stays_waiting() {
    let w = world().await;
    let off =
        "```clusia-suggestion\n{\"file\":\"feature.txt\",\"line\":99,\"body\":\"Where?\"}\n```\n";
    use_fake(&w, Script::one(Turn::answer(off))).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let mut watcher = watching(&w).await;
    c.request(send("review it")).await.unwrap();
    let id = suggestion_id(&until(&mut watcher, ready).await);
    let refusal = refused(
        c.request(Command::AcceptSuggestion {
            pr: pr7(),
            id: id.clone(),
            body: None,
        })
        .await,
    );
    assert_eq!(refusal.code, ErrorCode::BadRequest);
    let Reply::AgentLog(entries) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap()
    else {
        panic!("an agent log");
    };
    assert_eq!(suggestions_in(&entries), 1, "still waiting");
}

#[tokio::test]
async fn dismissed_suggestion_never_returns() {
    let w = world().await;
    use_fake(&w, Script::one(Turn::answer(SUGGESTING))).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let mut watcher = watching(&w).await;
    c.request(send("review it")).await.unwrap();
    let id = suggestion_id(&until(&mut watcher, ready).await);

    assert_eq!(
        c.request(Command::DismissSuggestion {
            pr: pr7(),
            id: id.clone()
        })
        .await
        .unwrap(),
        Reply::Ack
    );
    let again = refused(
        c.request(Command::DismissSuggestion {
            pr: pr7(),
            id: id.clone(),
        })
        .await,
    );
    assert_eq!(again.code, ErrorCode::NotFound);

    c.request(send("look again")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::AgentSuggestion { .. })),
        "the agent said it again, the daemon did not show it"
    );
    let Reply::AgentLog(entries) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap()
    else {
        panic!("an agent log");
    };
    assert_eq!(suggestions_in(&entries), 0);

    // A restart changes nothing.
    drop((c, watcher));
    let home = w.daemon.stop().await;
    let daemon = common::TestDaemon::start_in(home).await;
    let mut c = daemon.client().await;
    c.request(Command::Subscribe {
        topics: vec![topics::AGENT.into()],
    })
    .await
    .unwrap();
    c.request(send("and now?")).await.unwrap();
    let events = until(&mut c, ready).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::AgentSuggestion { .. }))
    );
    let Reply::AgentLog(entries) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap()
    else {
        panic!("an agent log");
    };
    assert_eq!(suggestions_in(&entries), 0);
    daemon.stop().await;
}

async fn comment_on_line_2(c: &mut clusia_protocol::Client) {
    c.request(Command::AddDraftItem {
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
    .await
    .unwrap();
}

#[tokio::test]
async fn publish_ends_the_session() {
    let w = world().await;
    mount_publish(&w.server, REVIEW_URL).await;
    let dir = use_fake(&w, Script::one(Turn::hanging())).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    let mut watcher = watching(&w).await;
    c.request(send("think about it")).await.unwrap();
    let call = calls(&dir, 1).await.remove(0);

    let published = c
        .request(Command::Publish {
            pr: pr7(),
            verdict: Verdict::RequestChanges,
            summary: "Please rename.".into(),
        })
        .await
        .unwrap();
    assert!(matches!(published, Reply::Published(_)), "{published:?}");
    until(&mut watcher, session_ended).await;
    assert!(!FakeClaude::is_running(call.pid), "the turn was stopped");
    assert!(
        w.daemon.paths.agent_log(&pr7()).exists(),
        "the chat stays on disk"
    );
    assert!(!w.daemon.paths.review_file(&pr7()).exists());
    let refusal = refused(c.request(send("hello?")).await);
    assert_eq!(refusal.code, ErrorCode::InvalidState);
}

#[tokio::test]
async fn discarding_ends_the_session() {
    let w = world().await;
    let dir = use_fake(&w, Script::one(Turn::hanging())).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let mut watcher = watching(&w).await;
    c.request(send("think about it")).await.unwrap();
    let call = calls(&dir, 1).await.remove(0);
    assert_eq!(
        c.request(Command::DiscardReview { pr: pr7() })
            .await
            .unwrap(),
        Reply::Ack
    );
    until(&mut watcher, session_ended).await;
    assert!(!FakeClaude::is_running(call.pid));
    assert!(w.daemon.paths.agent_log(&pr7()).exists());
}

#[tokio::test]
async fn agent_finished_notifies_only_when_no_window_holds_the_review() {
    let w = world().await;
    use_fake(&w, Script::one(Turn::answer("Done reading."))).await;
    let mut window = w.daemon.client().await;
    open(&mut window).await;
    let mut watcher = watching(&w).await;
    let inbox = |reply: Reply| match reply {
        Reply::Inbox(items) => items,
        other => panic!("an inbox, got {other:?}"),
    };

    window.request(send("first")).await.unwrap();
    until(&mut watcher, ready).await;
    assert!(
        inbox(window.request(Command::GetInbox).await.unwrap()).is_empty(),
        "a window shows the review: no notification"
    );

    drop(window);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut cli = w.daemon.client().await;
    cli.request(send("second")).await.unwrap();
    until(&mut watcher, ready).await;
    let items = inbox(cli.request(Command::GetInbox).await.unwrap());
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].kind, EventKind::AgentFinished);
    assert_eq!(items[0].pr, Some(pr7()));
    assert_eq!(items[0].title, "Claude Code finished on #7");
}

#[tokio::test]
async fn the_first_prompt_carries_the_pull_request_description() {
    let w = world().await;
    w.server.reset().await;
    let mock =
        PrMock::new(&w.head, &w.base, &w.origin).described("Adds the feature flag and its docs.");
    mount_pr(&w.server, &mock).await;
    let dir = fake_program(&w, Script::one(Turn::answer("ok"))).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    until(&mut watcher, ready).await;
    let prompt = value_of(&calls(&dir, 1).await[0].argv, "-p").unwrap();
    assert!(prompt.contains("Title: Add feature"), "{prompt}");
    assert!(
        prompt.contains("Adds the feature flag and its docs."),
        "{prompt}"
    );
}

#[tokio::test]
async fn closing_a_review_with_draft_items_keeps_its_session_and_its_turn() {
    let w = world().await;
    let dir = use_fake(&w, Script::one(Turn::hanging())).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    comment_on_line_2(&mut c).await;
    c.request(send("think about it")).await.unwrap();
    let call = calls(&dir, 1).await.remove(0);
    let stored = |reply: Reply| match reply {
        Reply::ReviewFile(review) => *review,
        other => panic!("a review file, got {other:?}"),
    };
    let mut review = stored(c.request(Command::GetReview { pr: pr7() }).await.unwrap());
    for _ in 0..50 {
        if review.harness_session.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        review = stored(c.request(Command::GetReview { pr: pr7() }).await.unwrap());
    }
    let session = review
        .harness_session
        .clone()
        .expect("the CLI echoed the session");

    assert_eq!(
        c.request(Command::CloseReview { pr: pr7() }).await.unwrap(),
        Reply::Ack
    );

    let review = stored(c.request(Command::GetReview { pr: pr7() }).await.unwrap());
    assert_eq!(review.state, ReviewState::Saved);
    assert_eq!(review.harness_session, Some(session));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        FakeClaude::is_running(call.pid),
        "a saved review keeps its running turn"
    );
    assert_eq!(
        c.request(Command::DiscardReview { pr: pr7() })
            .await
            .unwrap(),
        Reply::Ack
    );
    assert!(!FakeClaude::is_running(call.pid), "discarding ends it");
}
