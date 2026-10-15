mod common;

use std::time::Duration;

use clusia_core::DraftKind;
use clusia_core::diagram::{DiagramVersion, EntryState, VersionPhase};
use clusia_harness::testkit::{
    CodexScript, CodexTurn, CustomScript, CustomTurnScript, FakeClaude, Script, Turn,
    diagram_block, diagram_plan_block,
};
use clusia_protocol::{Client, Command, DiagramsState, ErrorCode, Event, Reply};
use common::agent::{calls, ready, refused, send, set_config, until, use_fake, watching};
use common::codex::{codex_calls, use_fake_codex};
use common::custom::{custom_calls, use_fake_custom};
use common::review_world::{World, open, pr7, world};
use serde_json::json;

fn plan() -> Turn {
    Turn::answer_blocks(
        "Two diagrams explain it.",
        &[diagram_plan_block(json!([
            {"key": "flow", "kind": "flow", "title": "Token refresh", "purpose": "The new path"},
            {"key": "graph", "kind": "graph", "title": "Modules", "purpose": "Who calls whom"}
        ]))],
    )
}

/// A drawing whose first box points at `feature.txt`, a file of the pull request, and whose second
/// points at a file it does not have.
fn drawing(key: &str, kind: &str) -> Turn {
    Turn::answer_blocks(
        "Drawn.",
        &[diagram_block(json!({
            "key": key, "kind": kind, "title": "Token refresh", "caption": "Two steps",
            "nodes": [
                {"id": "a", "label": "refresh()", "shape": "box",
                 "anchor": {"path": "feature.txt", "start_line": 1, "end_line": 2}},
                {"id": "b", "label": "store.save()", "shape": "box",
                 "anchor": {"path": "nope.txt", "start_line": 1, "end_line": 2}}
            ],
            "edges": [{"from": "a", "to": "b"}]
        }))],
    )
}

/// The text of a plan of one diagram and of its drawing, as a custom command or Codex answers.
fn one_plan_block() -> String {
    diagram_plan_block(json!([
        {"key": "flow", "kind": "flow", "title": "Token refresh", "purpose": "The new path"}
    ]))
}

fn one_drawing_block() -> String {
    diagram_block(json!({
        "key": "flow", "kind": "flow", "title": "Token refresh", "caption": "Two steps",
        "nodes": [{"id": "a", "label": "refresh()", "shape": "box"}]
    }))
}

fn plan_and_drawings() -> Script {
    Script::turns(vec![
        plan(),
        drawing("flow", "flow"),
        drawing("graph", "graph"),
    ])
}

fn idle(e: &Event) -> bool {
    matches!(
        e,
        Event::DiagramsChanged {
            state: DiagramsState::Idle,
            ..
        }
    )
}

async fn diagrams_of(
    c: &mut Client,
) -> (Vec<DiagramVersion>, Option<DiagramVersion>, DiagramsState) {
    let Reply::Diagrams {
        versions,
        latest,
        state,
        ..
    } = c.request(Command::GetDiagrams { pr: pr7() }).await.unwrap()
    else {
        panic!("the diagrams");
    };
    let mut full = Vec::new();
    for info in versions {
        let Reply::DiagramVersion(version) = c
            .request(Command::GetDiagramVersion {
                pr: pr7(),
                number: info.number,
            })
            .await
            .unwrap()
        else {
            panic!("a version");
        };
        full.push(*version);
    }
    (full, latest.map(|v| *v), state)
}

async fn gone(pid: u32) -> bool {
    for _ in 0..60 {
        if !FakeClaude::is_running(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn drawing_world(script: Script) -> (World, std::path::PathBuf, Client, Client) {
    let w = world().await;
    let dir = use_fake(&w, script).await;
    let watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    (w, dir, c, watcher)
}

#[tokio::test]
async fn the_plan_and_each_drawing_arrive_as_they_settle() {
    let (w, _dir, mut c, mut watcher) = drawing_world(plan_and_drawings()).await;
    assert_eq!(
        c.request(Command::DrawDiagrams { pr: pr7() })
            .await
            .unwrap(),
        Reply::Ack
    );
    let events = until(&mut watcher, idle).await;

    let states: Vec<&DiagramsState> = events
        .iter()
        .filter_map(|e| match e {
            Event::DiagramsChanged { state, .. } => Some(state),
            _ => None,
        })
        .collect();
    let drawing = |done, key: &str| DiagramsState::Drawing {
        done,
        total: 2,
        key: key.into(),
    };
    assert_eq!(
        states,
        [
            &DiagramsState::Planning,
            &drawing(0, "flow"),
            &drawing(1, "graph"),
            &DiagramsState::Idle
        ]
    );
    let planned = events
        .iter()
        .find_map(|e| match e {
            Event::DiagramsPlanned { version, .. } => Some(version),
            _ => None,
        })
        .expect("the plan is told");
    assert_eq!(
        (planned.number, planned.commit.as_str()),
        (1, w.head.as_str())
    );
    assert!(
        planned
            .entries
            .iter()
            .all(|e| matches!(e.state, EntryState::Waiting))
    );
    let entry_states: Vec<(&str, &'static str)> = events
        .iter()
        .filter_map(|e| match e {
            Event::DiagramEntryChanged { key, state, .. } => Some((
                key.as_str(),
                match state {
                    EntryState::Drawing => "drawing",
                    EntryState::Ready { .. } => "ready",
                    other => panic!("{other:?}"),
                },
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        entry_states,
        [
            ("flow", "drawing"),
            ("flow", "ready"),
            ("graph", "drawing"),
            ("graph", "ready")
        ]
    );

    let (versions, latest, state) = diagrams_of(&mut c).await;
    let latest = latest.expect("the version");
    assert_eq!(state, DiagramsState::Idle);
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0], latest, "the full version is the latest");
    assert_eq!(latest.phase, VersionPhase::Done);
    let EntryState::Ready { spec, notes } = &latest.entries[0].state else {
        panic!("flow is ready");
    };
    assert!(
        spec.nodes[0].anchor.is_some(),
        "a file of the pull request keeps its link"
    );
    assert!(
        spec.nodes[1].anchor.is_none(),
        "a file it does not have loses it"
    );
    assert!(!notes.is_empty(), "and says so");

    // Nothing of it is the chat.
    let Reply::AgentLog(chat) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap() else {
        panic!("the log");
    };
    assert!(chat.is_empty(), "{chat:?}");
}

#[tokio::test]
async fn a_second_draw_is_refused_and_stop_ends_the_first_leaving_nothing() {
    let (w, dir, mut c, mut watcher) = drawing_world(Script::one(Turn::hanging())).await;
    c.request(Command::DrawDiagrams { pr: pr7() })
        .await
        .unwrap();
    let call = calls(&dir, 1).await.remove(0);
    let refusal = refused(c.request(Command::DrawDiagrams { pr: pr7() }).await);
    assert_eq!(refusal.code, ErrorCode::InvalidState);
    assert_eq!(refusal.message, "Diagrams are already being drawn");
    let (_, _, state) = diagrams_of(&mut c).await;
    assert_eq!(state, DiagramsState::Planning);

    c.request(Command::StopDiagrams { pr: pr7() })
        .await
        .unwrap();
    until(&mut watcher, idle).await;
    assert!(gone(call.pid).await, "stopping killed the agent");
    let (versions, _, state) = diagrams_of(&mut c).await;
    assert!(versions.is_empty() && state == DiagramsState::Idle);
    assert!(!w.daemon.paths.diagrams_for(&pr7()).exists());
}

#[tokio::test]
async fn a_review_opened_with_the_setting_on_draws_by_itself() {
    let w = world().await;
    set_config(&w, "harness.diagrams", "true").await;
    let dir = use_fake(&w, plan_and_drawings()).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    until(&mut watcher, idle).await;
    assert_eq!(FakeClaude::calls(&dir).len(), 3);
    let (_, latest, _) = diagrams_of(&mut c).await;
    assert_eq!(latest.expect("a version").commit, w.head);
    // The same head is not drawn again.
    open(&mut c).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(FakeClaude::calls(&dir).len(), 3);
}

#[tokio::test]
async fn closing_a_review_with_a_draft_keeps_the_version_with_its_entries_not_drawn() {
    let script = Script::turns(vec![plan(), Turn::hanging()]);
    let (w, dir, mut c, _watcher) = drawing_world(script).await;
    c.request(Command::AddDraftItem {
        pr: pr7(),
        kind: DraftKind::General,
        anchor: None,
        thread: None,
        body: "Looks fine".into(),
    })
    .await
    .unwrap();
    c.request(Command::DrawDiagrams { pr: pr7() })
        .await
        .unwrap();
    let call = calls(&dir, 2).await.remove(1);
    assert_eq!(
        c.request(Command::CloseReview { pr: pr7() }).await.unwrap(),
        Reply::Ack
    );
    assert!(gone(call.pid).await, "closing stopped the agent");
    assert!(w.daemon.paths.diagrams_for(&pr7()).exists());
    open(&mut c).await;
    let (_, latest, state) = diagrams_of(&mut c).await;
    let latest = latest.expect("the version stays");
    assert_eq!(state, DiagramsState::Idle);
    assert_eq!(latest.phase, VersionPhase::Done);
    assert!(
        latest
            .entries
            .iter()
            .all(|e| matches!(e.state, EntryState::NotDrawn))
    );
}

#[tokio::test]
async fn discarding_a_review_deletes_its_diagrams_and_stops_the_drawing() {
    let script = Script::turns(vec![plan(), Turn::hanging()]);
    let (w, dir, mut c, _watcher) = drawing_world(script).await;
    c.request(Command::DrawDiagrams { pr: pr7() })
        .await
        .unwrap();
    let call = calls(&dir, 2).await.remove(1);
    assert!(w.daemon.paths.diagrams_for(&pr7()).exists());
    assert_eq!(
        c.request(Command::DiscardReview { pr: pr7() })
            .await
            .unwrap(),
        Reply::Ack
    );
    assert!(gone(call.pid).await, "discarding stopped the agent");
    assert!(
        !w.daemon.paths.diagrams_for(&pr7()).exists(),
        "discarding deletes the versions"
    );
}

async fn the_entry_is_ready_and_the_chat_stays_empty(w: &World, c: &mut Client) {
    let (_, latest, state) = diagrams_of(c).await;
    let latest = latest.expect("a version");
    assert_eq!(state, DiagramsState::Idle);
    assert_eq!(latest.commit, w.head);
    assert!(
        matches!(latest.entries[0].state, EntryState::Ready { .. }),
        "{:?}",
        latest.entries
    );
    let Reply::AgentLog(chat) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap() else {
        panic!("the log");
    };
    assert!(chat.is_empty(), "{chat:?}");
}

#[tokio::test]
async fn codex_plans_and_draws_in_threads_forked_from_the_chat_thread() {
    let w = world().await;
    let dir = use_fake_codex(
        &w,
        CodexScript::turns(vec![
            CodexTurn::answer("Chat one."),
            CodexTurn::answer_blocks("Plan.", &[one_plan_block()]),
            CodexTurn::answer_blocks("Drawn.", &[one_drawing_block()]),
        ]),
    )
    .await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(send("hello")).await.unwrap();
    until(&mut watcher, ready).await;
    c.request(Command::DrawDiagrams { pr: pr7() })
        .await
        .unwrap();
    until(&mut watcher, idle).await;

    let calls = codex_calls(&dir, 1).await;
    let forks = calls[0].requests("thread/fork");
    assert_eq!(
        forks.len(),
        2,
        "the plan and the drawing each fork the chat's thread"
    );
    assert!(forks.iter().all(|f| f["params"]["threadId"] == "thr-1"));
    let (_, latest, _) = diagrams_of(&mut c).await;
    assert!(matches!(
        latest.expect("a version").entries[0].state,
        EntryState::Ready { .. }
    ));
    // Only the chat's own turn is in the log.
    let Reply::AgentLog(chat) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap() else {
        panic!("the log");
    };
    assert!(
        chat.iter()
            .all(|e| !matches!(e, clusia_protocol::AgentLogEntry::Diagrams { .. })),
        "{chat:?}"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_custom_command_plans_and_draws_in_sessions_of_its_own() {
    let w = world().await;
    let dir = use_fake_custom(
        &w,
        CustomScript::new().turns(vec![
            CustomTurnScript::new()
                .session("plan-1")
                .final_(&format!("Plan.\n{}", one_plan_block())),
            CustomTurnScript::new()
                .session("draw-1")
                .final_(&format!("Drawn.\n{}", one_drawing_block())),
        ]),
    )
    .await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(Command::DrawDiagrams { pr: pr7() })
        .await
        .unwrap();
    until(&mut watcher, idle).await;

    let calls = custom_calls(&dir, 2).await;
    for call in &calls {
        let line = call.turn_line().expect("a turn line");
        assert!(
            line["session_id"].is_null(),
            "a fork starts a session of its own: {line}"
        );
    }
    assert_ne!(
        calls[0].turn_line().unwrap()["prompt"],
        calls[1].turn_line().unwrap()["prompt"]
    );
    the_entry_is_ready_and_the_chat_stays_empty(&w, &mut c).await;
    w.daemon.stop().await;
}
