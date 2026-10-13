mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use clusia_core::Review;
use clusia_core::checks::CheckKind;
use clusia_core::config::HarnessKind;
use clusia_core::revalidation::{Decision, Verdict as Judged};
use clusia_harness::testkit::{
    CustomScript, CustomTurnScript, FakeCustom, finding_block, revalidation_block,
};
use clusia_protocol::{
    AgentErrorKind, AgentLogEntry, Client, Command, Event, PermissionAnswerKind, PermissionOutcome,
    Reply,
};
use common::agent::{ready, send, set_config, until, watching};
use common::custom::{
    custom_calls, custom_decisions, custom_hung, group_alive, group_gone, use_fake_custom,
};
use common::revalidation::{FILE, push, review_of, run_round, saved};
use common::review_world::{World, open, pr7, world};
use serde_json::json;

fn turn() -> CustomTurnScript {
    CustomTurnScript::new()
}

fn chunks(events: &[Event]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            Event::AgentChunk { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn error_of(events: &[Event]) -> Option<(AgentErrorKind, String)> {
    events.iter().find_map(|e| match e {
        Event::AgentError { kind, message, .. } => Some((*kind, message.clone())),
        _ => None,
    })
}

fn requested(e: &Event) -> bool {
    matches!(e, Event::PermissionRequested { .. })
}

fn resolved(e: &Event) -> bool {
    matches!(e, Event::PermissionResolved { .. })
}

fn check_done(kind: CheckKind) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::CheckDone { kind: k, .. } if *k == kind)
}

async fn review_file(c: &mut Client) -> Review {
    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(review) => *review,
        other => panic!("expected the review file, got {other:?}"),
    }
}

/// The id of the last permission request in `events`.
fn request_id(events: &[Event]) -> String {
    match events.last() {
        Some(Event::PermissionRequested { id, .. }) => id.clone(),
        other => panic!("a permission request, got {other:?}"),
    }
}

async fn answer(c: &mut Client, id: &str, answer: PermissionAnswerKind) {
    c.request(Command::PermissionAnswer {
        id: id.into(),
        answer,
    })
    .await
    .unwrap();
}

/// A world with the fake installed, the review open and a watcher on the agent topic.
async fn started(script: CustomScript) -> (World, std::path::PathBuf, Client, Client) {
    let w = world().await;
    let dir = use_fake_custom(&w, script).await;
    let watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    (w, dir, c, watcher)
}

#[tokio::test]
async fn a_custom_chat_streams_logs_and_the_next_message_resumes_its_session() {
    let (w, dir, mut c, mut watcher) = started(CustomScript::new().turns(vec![
        turn()
            .session("sess-1")
            .tool("Read the prompt")
            .text("Hello ")
            .text("from Echo")
            .final_("Hello from Echo"),
        turn().session("sess-1").final_("Second answer"),
    ]))
    .await;

    assert_eq!(
        c.request(send("What changed?")).await.unwrap(),
        Reply::AgentTurn { turn: 1 }
    );
    let events = until(&mut watcher, ready).await;
    assert_eq!(chunks(&events), "Hello from Echo");
    let Reply::AgentLog(entries) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap()
    else {
        panic!("an agent log");
    };
    assert!(matches!(&entries[0], AgentLogEntry::User { text, .. } if text == "What changed?"));
    assert!(entries.iter().any(
        |e| matches!(e, AgentLogEntry::ToolUse { summary, .. } if summary == "Read the prompt")
    ));
    assert!(
        entries
            .iter()
            .any(|e| matches!(e, AgentLogEntry::Done { turn: 1, .. }))
    );

    let review = review_file(&mut c).await;
    assert_eq!(review.harness_session.as_deref(), Some("sess-1"));
    assert_eq!(review.harness_session_kind, Some(HarnessKind::Custom));
    assert_eq!(review.harness_session_name.as_deref(), Some("Echo"));
    let Reply::Value(program) = c
        .request(Command::GetConfigValue {
            key: "harness.program".into(),
        })
        .await
        .unwrap()
    else {
        panic!("a value");
    };
    assert_eq!(
        review.harness_session_program.as_deref(),
        Some(program.as_str())
    );

    c.request(send("And the tests?")).await.unwrap();
    until(&mut watcher, ready).await;
    let calls = custom_calls(&dir, 2).await;
    for token in ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN"] {
        assert_eq!(
            calls[0].env_value(token),
            None,
            "{token} reached the program"
        );
    }
    assert!(
        !calls[0]
            .env
            .iter()
            .any(|(key, _)| key.starts_with("CLUSIA_")),
        "a CLUSIA_ variable reached the program: {:?}",
        calls[0].env
    );
    let first = calls[0].turn_line().expect("a turn line");
    assert_eq!(first["type"], "turn");
    assert_eq!(first["protocol"], 1);
    assert_eq!(first["prompt"], "What changed?");
    assert!(first["session_id"].is_null(), "{first}");
    assert!(!first["role"].as_str().unwrap().is_empty());
    assert!(first["timeout_secs"].is_u64());
    let worktree = first["worktree"].as_str().unwrap();
    assert_eq!(
        Path::new(&calls[0].cwd).canonicalize().unwrap(),
        Path::new(worktree).canonicalize().unwrap(),
        "the program runs in the worktree"
    );
    assert_eq!(
        calls[1].turn_line().unwrap()["session_id"],
        "sess-1",
        "the second message resumes the session"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn an_open_review_keeps_the_custom_program_it_started_with() {
    let (w, dir, mut c, mut watcher) = started(CustomScript::new().turns(vec![
        turn().session("s1").final_("One"),
        turn().session("s1").final_("Two"),
    ]))
    .await;
    c.request(send("first")).await.unwrap();
    until(&mut watcher, ready).await;

    set_config(&w, "harness.program", "/nonexistent/other-harness").await;
    c.request(send("second")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert!(error_of(&events).is_none(), "{events:?}");
    assert_eq!(chunks(&events), "Two");
    assert_eq!(custom_calls(&dir, 2).await.len(), 2);
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_garbage_is_text_and_unknown_lines_are_ignored() {
    let (w, _dir, mut c, mut watcher) = started(
        CustomScript::new().turn(
            turn()
                .raw("this is not json")
                .raw(r#"{"type":"mystery","x":1}"#)
                .raw(r#"{"type":"text"}"#)
                .final_("done"),
        ),
    )
    .await;
    c.request(send("go")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert_eq!(chunks(&events), "this is not json\n");
    assert!(error_of(&events).is_none(), "{events:?}");
    let Reply::AgentLog(entries) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap()
    else {
        panic!("an agent log");
    };
    assert!(
        entries
            .iter()
            .any(|e| matches!(e, AgentLogEntry::Done { turn: 1, .. }))
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_program_that_prints_no_json_at_all_is_unparsed_and_the_next_turn_works() {
    let (w, _dir, mut c, mut watcher) = started(CustomScript::new().turns(vec![
        turn().garbage(),
        turn().session("s1").final_("Back again"),
    ]))
    .await;
    c.request(send("one")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert_eq!(
        error_of(&events),
        Some((
            AgentErrorKind::Unparsed,
            "Echo answered in a format Clúsia does not know".to_string()
        ))
    );

    c.request(send("two")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert_eq!(chunks(&events), "Back again");
    assert!(error_of(&events).is_none());
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_crash_after_some_json_names_the_last_stderr_line() {
    let (w, _dir, mut c, mut watcher) = started(CustomScript::new().turns(vec![
        turn().session("s1").text("Half an ans").stderr("first").stderr("boom").crash(3),
        turn().session("s1").final_("Fine"),
    ]))
    .await;
    c.request(send("one")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert_eq!(
        error_of(&events),
        Some((
            AgentErrorKind::Crashed,
            "Echo stopped unexpectedly: boom".to_string()
        ))
    );
    c.request(send("two")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert_eq!(chunks(&events), "Fine", "the next message still works");
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_errors_have_their_own_lines() {
    let (w, _dir, mut c, mut watcher) = started(CustomScript::new().turns(vec![
        turn().error("auth", "please log in"),
        turn().error("limit", "slow down"),
        turn().error("other", "disk full"),
    ]))
    .await;
    let mut seen = Vec::new();
    for text in ["one", "two", "three"] {
        c.request(send(text)).await.unwrap();
        seen.push(error_of(&until(&mut watcher, ready).await).unwrap());
    }
    assert_eq!(
        seen,
        [
            (
                AgentErrorKind::NotSignedIn,
                "Echo: please log in".to_string()
            ),
            (
                AgentErrorKind::UsageLimit,
                "Echo reached its usage limit".to_string()
            ),
            (AgentErrorKind::Crashed, "Echo: disk full".to_string()),
        ]
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn an_error_of_kind_session_forgets_the_chat_session() {
    let (w, dir, mut c, mut watcher) = started(CustomScript::new().turns(vec![
        turn().session("old-1").final_("One"),
        turn().error("session", "unknown session"),
        turn().session("new-1").final_("Three"),
    ]))
    .await;
    c.request(send("one")).await.unwrap();
    until(&mut watcher, ready).await;
    assert_eq!(
        review_file(&mut c).await.harness_session.as_deref(),
        Some("old-1")
    );

    c.request(send("two")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert_eq!(
        error_of(&events),
        Some((
            AgentErrorKind::Crashed,
            "Echo no longer knows this session; the next turn starts a new one".to_string()
        ))
    );
    let review = review_file(&mut c).await;
    assert_eq!(review.harness_session, None);
    assert_eq!(review.harness_session_kind, None);

    c.request(send("three")).await.unwrap();
    until(&mut watcher, ready).await;
    let calls = custom_calls(&dir, 3).await;
    assert_eq!(calls[1].turn_line().unwrap()["session_id"], "old-1");
    assert!(
        calls[2].turn_line().unwrap()["session_id"].is_null(),
        "a new session starts"
    );
    assert_eq!(
        review_file(&mut c).await.harness_session.as_deref(),
        Some("new-1")
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_custom_command_without_a_usable_program_says_what_to_set() {
    // The review opens with no program, so its pin holds none and gives way to every program set
    // afterwards: the program of an open review can be fixed without reopening it.
    let w = world().await;
    set_config(&w, "harness.kind", "custom").await;
    set_config(&w, "harness.on_open", "wait").await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;

    let mut said = Vec::new();
    for program in ["", "/nonexistent/echo", "bin/echo.py", "echo-harness"] {
        set_config(&w, "harness.program", program).await;
        c.request(send("hi")).await.unwrap();
        said.push(error_of(&until(&mut watcher, ready).await).unwrap());
    }
    assert_eq!(
        said,
        [
            (
                AgentErrorKind::NotInstalled,
                "Set the custom command's program in Config › Harness".to_string()
            ),
            (
                AgentErrorKind::NotInstalled,
                "/nonexistent/echo was not found. Set it in Config › Harness".to_string()
            ),
            (
                AgentErrorKind::NotInstalled,
                "bin/echo.py is not an absolute path. Set it in Config › Harness".to_string()
            ),
            (
                AgentErrorKind::NotInstalled,
                "echo-harness is not an absolute path. Set it in Config › Harness".to_string()
            ),
        ]
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_permissions_command_allow_once_then_deny() {
    let (w, dir, mut c, mut watcher) = started(CustomScript::new().turns(vec![
        turn()
            .session("s1")
            .ask_command("p1", "cargo test -p clusia-core")
            .final_("Ran it."),
        turn().session("s1").ask_command("p2", "cargo build").final_("Not run."),
    ]))
    .await;
    c.request(send("run the tests")).await.unwrap();
    let asked = until(&mut watcher, requested).await;
    let Some(Event::PermissionRequested {
        tool,
        summary,
        sandbox,
        prefix,
        ..
    }) = asked.last()
    else {
        panic!("a request");
    };
    assert_eq!(
        (tool.as_str(), summary.as_str()),
        ("Bash", "cargo test -p clusia-core")
    );
    assert_eq!(prefix.as_deref(), Some("cargo test"));
    assert!(!*sandbox, "a custom command has no sandbox");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        FakeCustom::decisions(&dir).is_empty(),
        "the program waits for the reviewer"
    );
    answer(&mut c, &request_id(&asked), PermissionAnswerKind::Once).await;
    until(&mut watcher, ready).await;
    assert_eq!(
        custom_decisions(&dir, 1).await,
        [("p1".to_string(), "allow".to_string())]
    );

    c.request(send("build it")).await.unwrap();
    let asked = until(&mut watcher, requested).await;
    answer(&mut c, &request_id(&asked), PermissionAnswerKind::Deny).await;
    until(&mut watcher, ready).await;
    assert_eq!(
        custom_decisions(&dir, 2).await[1],
        ("p2".to_string(), "deny".to_string())
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_permissions_always_for_this_review_covers_the_next_request() {
    let (w, dir, mut c, mut watcher) = started(CustomScript::new().turns(vec![
        turn()
            .session("s1")
            .ask_command("p1", "cargo test -p clusia-core")
            .final_("One."),
        turn()
            .session("s1")
            .ask_command("p2", "cargo test -p clusia-core")
            .final_("Two."),
    ]))
    .await;
    c.request(send("first")).await.unwrap();
    let asked = until(&mut watcher, requested).await;
    answer(&mut c, &request_id(&asked), PermissionAnswerKind::Review).await;
    until(&mut watcher, ready).await;
    assert_eq!(
        custom_decisions(&dir, 1).await,
        [("p1".to_string(), "allow".to_string())]
    );

    c.request(send("second")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert!(
        !events.iter().any(requested),
        "the rule of the review answers the same command: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::PermissionResolved {
                outcome: PermissionOutcome::AllowedForReview,
                ..
            }
        )),
        "{events:?}"
    );
    assert_eq!(
        custom_decisions(&dir, 2).await[1],
        ("p2".to_string(), "allow".to_string())
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_permissions_other_offers_only_once_and_deny() {
    let (w, dir, mut c, mut watcher) = started(
        CustomScript::new().turn(
            turn()
                .session("s1")
                .ask_other("p1", "Delete the build cache")
                .final_("Done."),
        ),
    )
    .await;
    c.request(send("clean up")).await.unwrap();
    let asked = until(&mut watcher, requested).await;
    let Some(Event::PermissionRequested {
        tool,
        summary,
        prefix,
        ..
    }) = asked.last()
    else {
        panic!("a request");
    };
    assert_eq!(tool, "Other");
    assert_eq!(summary, "Delete the build cache");
    assert_eq!(*prefix, None, "no rule can cover an \"other\" request");
    answer(&mut c, &request_id(&asked), PermissionAnswerKind::Once).await;
    until(&mut watcher, ready).await;
    assert_eq!(
        custom_decisions(&dir, 1).await,
        [("p1".to_string(), "allow".to_string())]
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_permissions_edit_inside_the_worktree_is_asked() {
    let (w, dir, mut c, mut watcher) = started(
        CustomScript::new().turn(
            turn()
                .session("s1")
                .ask_edit("p1", &["feature.txt", "src/new.rs"])
                .final_("Edited."),
        ),
    )
    .await;
    c.request(send("fix it")).await.unwrap();
    let asked = until(&mut watcher, requested).await;
    let Some(Event::PermissionRequested { tool, summary, .. }) = asked.last() else {
        panic!("a request");
    };
    assert_eq!((tool.as_str(), summary.as_str()), ("Edit", "feature.txt"));
    answer(&mut c, &request_id(&asked), PermissionAnswerKind::Once).await;
    until(&mut watcher, ready).await;
    assert_eq!(
        custom_decisions(&dir, 1).await,
        [("p1".to_string(), "allow".to_string())]
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_permissions_an_edit_outside_the_worktree_or_with_no_paths_is_refused() {
    let (w, dir, mut c, mut watcher) = started(
        CustomScript::new().turn(
            turn()
                .session("s1")
                .ask_edit("outside", &["feature.txt", "../escape.txt"])
                .ask_edit("absolute", &["/etc/hosts"])
                .ask_edit("empty", &[])
                .ask_command("blank", "   ")
                .final_("Finished."),
        ),
    )
    .await;
    c.request(send("edit")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert!(
        !events.iter().any(requested),
        "nobody is asked about what is refused: {events:?}"
    );
    let refusals = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::PermissionResolved {
                    outcome: PermissionOutcome::Denied,
                    ..
                }
            )
        })
        .count();
    assert_eq!(refusals, 4, "{events:?}");
    assert_eq!(
        custom_decisions(&dir, 4).await,
        [
            ("outside".to_string(), "deny".to_string()),
            ("absolute".to_string(), "deny".to_string()),
            ("empty".to_string(), "deny".to_string()),
            ("blank".to_string(), "deny".to_string()),
        ]
    );
    assert!(error_of(&events).is_none(), "the turn goes on: {events:?}");
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_permissions_a_duplicate_or_missing_id_is_refused_and_never_answered() {
    let line = |id: &str| {
        format!(r#"{{"type":"permission","id":"{id}","kind":"command","detail":"cargo test"}}"#)
    };
    let (w, dir, mut c, mut watcher) = started(
        CustomScript::new().turn(
            turn()
                .session("s1")
                .ask_command("p1", "cargo test")
                .raw(&line("p1"))
                .raw(&line(""))
                .raw(r#"{"type":"permission","kind":"command","detail":"cargo test"}"#)
                .final_("Done."),
        ),
    )
    .await;
    c.request(send("go")).await.unwrap();
    let asked = until(&mut watcher, requested).await;
    answer(&mut c, &request_id(&asked), PermissionAnswerKind::Once).await;
    let events = until(&mut watcher, ready).await;
    assert_eq!(
        events.iter().filter(|e| requested(e)).count(),
        0,
        "only the first request reached the reviewer"
    );
    assert_eq!(
        events.iter().filter(|e| resolved(e)).count(),
        4,
        "the answered one and the three refusals are told: {events:?}"
    );
    let summaries: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::PermissionResolved { summary, .. } => Some(summary.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        summaries
            .iter()
            .filter(|s| **s == "a second request with the id p1")
            .count(),
        1,
        "{summaries:?}"
    );
    assert_eq!(
        summaries
            .iter()
            .filter(|s| **s == "a request with no id")
            .count(),
        2,
        "{summaries:?}"
    );
    assert_eq!(
        FakeCustom::decisions(&dir),
        [("p1".to_string(), "allow".to_string())],
        "an id is answered once, and a request without one is not answered"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_permissions_an_unknown_kind_is_refused_with_one_deny() {
    let (w, dir, mut c, mut watcher) = started(
        CustomScript::new().turn(
            turn()
                .session("s1")
                .ask_raw(r#"{"type":"permission","id":"p9","kind":"teleport","detail":"to Mars"}"#)
                .final_("Done."),
        ),
    )
    .await;
    c.request(send("go")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert!(!events.iter().any(requested), "{events:?}");
    assert_eq!(
        custom_decisions(&dir, 1).await,
        [("p9".to_string(), "deny".to_string())]
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_permissions_stop_while_asking_denies_on_the_pipe_and_ends_the_group() {
    let (w, dir, mut c, mut watcher) = started(
        CustomScript::new().turn(
            turn()
                .ignore_term()
                .session("s1")
                .ask_command("p1", "cargo test")
                .final_("Never said."),
        ),
    )
    .await;
    c.request(send("go")).await.unwrap();
    until(&mut watcher, requested).await;
    let pid = custom_calls(&dir, 1).await[0].pid;

    c.request(Command::AgentCancel { pr: pr7() }).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert_eq!(
        error_of(&events),
        Some((AgentErrorKind::Interrupted, "Stopped by you".to_string()))
    );
    assert_eq!(
        custom_decisions(&dir, 1).await,
        [("p1".to_string(), "deny".to_string())],
        "the program is told no before its input closes: it ignores SIGTERM, so it can still record the reply"
    );
    assert!(group_gone(pid, 15).await, "no process is left");
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_stop_kills_the_group() {
    let (w, dir, mut c, mut watcher) = started(
        CustomScript::new().turn(
            turn()
                .session("s1")
                .text("thinking")
                .delay_ms(60_000)
                .final_("late"),
        ),
    )
    .await;
    c.request(send("go")).await.unwrap();
    let pid = custom_calls(&dir, 1).await[0].pid;
    assert!(group_alive(pid));

    c.request(Command::AgentCancel { pr: pr7() }).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert_eq!(
        error_of(&events),
        Some((AgentErrorKind::Interrupted, "Stopped by you".to_string()))
    );
    assert!(
        group_gone(pid, 5).await,
        "the program and what it started are gone"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_hang_is_killed_after_grace() {
    let (w, dir, mut c, mut watcher) =
        started(CustomScript::new().turn(turn().session("s1").hang())).await;
    c.request(send("go")).await.unwrap();
    let pid = custom_hung(&dir, 0).await.pid;

    let asked = Instant::now();
    c.request(Command::AgentCancel { pr: pr7() }).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        FakeCustom::is_running(pid) && group_alive(pid),
        "SIGTERM alone does not end it: the grace is not over"
    );
    assert!(
        group_gone(pid, 20).await,
        "SIGKILL ends the group after the grace"
    );
    assert!(
        asked.elapsed() >= Duration::from_secs(9),
        "the grace is 10 s, it was {:?}",
        asked.elapsed()
    );
    until(&mut watcher, ready).await;
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_shutdown_leaves_no_process() {
    let (w, dir, mut c, _watcher) =
        started(CustomScript::new().turn(turn().session("s1").hang())).await;
    c.request(send("go")).await.unwrap();
    let pid = custom_hung(&dir, 0).await.pid;

    w.daemon.stop().await;
    assert!(
        group_gone(pid, 15).await,
        "the daemon left a process behind"
    );
}

#[tokio::test]
async fn custom_oversized_output_ends_the_turn_and_the_group() {
    let (w, dir, mut c, mut watcher) = started(CustomScript::new().turns(vec![
        turn().session("s1").oversized_line().stall(),
        turn().session("s1").final_("Next"),
    ]))
    .await;
    c.request(send("go")).await.unwrap();
    let events = until(&mut watcher, ready).await;
    assert_eq!(
        error_of(&events),
        Some((
            AgentErrorKind::Crashed,
            "Echo wrote more than Clúsia reads in one turn".to_string()
        ))
    );
    let pid = custom_calls(&dir, 1).await[0].pid;
    assert!(group_gone(pid, 15).await);

    c.request(send("again")).await.unwrap();
    assert_eq!(chunks(&until(&mut watcher, ready).await), "Next");
    w.daemon.stop().await;
}

fn security_blocks() -> String {
    [
        finding_block(json!({
            "area": "security", "severity": "high", "title": "Token in the log",
            "file": "feature.txt", "line": 2, "body": "The token reaches the log.",
            "comment": "Do not log the token.", "code": "- log(token)\n+ log(\"…\")"
        })),
        finding_block(json!({
            "area": "security", "severity": "low", "title": "Loose parse",
            "file": "feature.txt", "line": 99, "body": "Line 99 is not in the diff."
        })),
    ]
    .join("\n")
}

#[tokio::test]
async fn custom_checks_send_no_session_and_never_store_the_one_they_get() {
    let (w, dir, mut c, mut watcher) = started(CustomScript::new().turns(vec![
        turn().session("chat-1").final_("Chat one."),
        turn()
            .session("check-9")
            .final_(&format!("Two things.\n{}", security_blocks())),
        turn().session("chat-1").final_("Chat two."),
    ]))
    .await;
    c.request(send("hello")).await.unwrap();
    until(&mut watcher, ready).await;

    c.request(Command::RunCheck {
        pr: pr7(),
        kind: CheckKind::Security,
    })
    .await
    .unwrap();
    until(&mut watcher, check_done(CheckKind::Security)).await;
    let calls = custom_calls(&dir, 2).await;
    let check = calls[1].turn_line().expect("a turn line");
    assert!(
        check["session_id"].is_null(),
        "a check starts a session of its own: {check}"
    );
    assert_ne!(check["prompt"], calls[0].turn_line().unwrap()["prompt"]);
    assert_eq!(
        review_file(&mut c).await.harness_session.as_deref(),
        Some("chat-1"),
        "the check's session never becomes the chat's"
    );
    let Reply::Checks { results, .. } = c.request(Command::GetChecks { pr: pr7() }).await.unwrap()
    else {
        panic!("the checks");
    };
    let titles: Vec<&str> = results[0]
        .findings
        .iter()
        .map(|f| f.title.as_str())
        .collect();
    assert_eq!(titles, ["Token in the log", "Loose parse"]);

    c.request(send("again")).await.unwrap();
    until(&mut watcher, ready).await;
    let calls = custom_calls(&dir, 3).await;
    assert_eq!(
        calls[2].turn_line().unwrap()["session_id"],
        "chat-1",
        "the chat goes on in its own session"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn custom_revalidation_sends_no_session_and_stores_none() {
    let w = world().await;
    let (mut c, ids) = saved(&w).await;
    let dir = use_fake_custom(
        &w,
        CustomScript::new().turn(turn().session("round-1").final_(&format!(
            "Checked.\n{}\n{}",
            revalidation_block(json!({
                "item": ids.a, "verdict": "moved", "path": FILE, "line": 1,
                "reason": "The comment follows the same code."
            })),
            revalidation_block(json!({
                "item": ids.b, "verdict": "obsolete", "reason": "The line was deleted."
            })),
        ))),
    )
    .await;
    push(&w, &mut c, "zero\n").await;
    run_round(&w, &mut c).await;

    let round = review_of(&mut c).await.revalidation.expect("a round");
    let judged = |id: &str| {
        let proposal = round
            .proposals
            .iter()
            .find(|p| p.item == id)
            .expect("a proposal");
        assert_eq!(proposal.decision, Decision::Pending);
        proposal.verdict.clone()
    };
    assert!(matches!(judged(&ids.a), Judged::Moved { anchor, .. } if anchor.line == 1));
    assert!(matches!(judged(&ids.b), Judged::Obsolete { .. }));

    let calls = custom_calls(&dir, 1).await;
    assert!(calls[0].turn_line().unwrap()["session_id"].is_null());
    assert_eq!(
        review_of(&mut c).await.harness_session,
        None,
        "a round's session is never the chat's"
    );
    w.daemon.stop().await;
}
