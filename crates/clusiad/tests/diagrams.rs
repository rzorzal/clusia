mod common;

use std::time::Duration;

use clusia_core::diagram::{DiagramVersion, EntryState, VersionPhase};
use clusia_core::revalidation::DiagramsStale;
use clusia_core::{DraftKind, Origin};
use clusia_harness::testkit::{
    CodexScript, CodexTurn, CustomScript, CustomTurnScript, FakeClaude, Script, Turn,
    diagram_block, diagram_plan_block, keyword_placement_block, keyword_plan_block,
    revalidation_block,
};
use clusia_protocol::{Client, Command, DiagramCode, DiagramsState, ErrorCode, Event, Reply};
use common::agent::{calls, ready, refused, send, set_config, until, use_fake, watching};
use common::codex::{codex_calls, use_fake_codex};
use common::custom::{custom_calls, use_fake_custom};
use common::git_fixture::{advance_pr, sh};
use common::github_mock::{PrMock, mount_pr};
use common::revalidation;
use common::review_world::{World, open, pr7, restart_after, world};
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
        step: None,
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
async fn a_draw_with_no_version_number_left_is_refused_before_the_agent_runs() {
    let (w, dir, mut c, _watcher) = drawing_world(plan_and_drawings()).await;
    let folder = w.daemon.paths.diagrams_for(&pr7());
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("v4294967295.json"), "x").unwrap();
    let refusal = refused(c.request(Command::DrawDiagrams { pr: pr7() }).await);
    assert_eq!(refusal.code, ErrorCode::InvalidState);
    assert_eq!(
        refusal.message,
        "There is no version number left for these diagrams"
    );
    assert!(FakeClaude::calls(&dir).is_empty(), "no turn was spent");
    assert_eq!(
        std::fs::read_to_string(folder.join("v4294967295.json")).unwrap(),
        "x",
        "the last file is left as it is"
    );
}

/// How many times the daemon asked GitHub for the pull request's files.
async fn files_requests(w: &World) -> usize {
    w.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/pulls/7/files"))
        .count()
}

#[tokio::test]
async fn without_the_review_cache_the_code_links_still_know_the_pull_requests_files() {
    let (w, _dir, c, watcher) = drawing_world(plan_and_drawings()).await;
    drop((c, watcher));
    // Removed while the daemon is down, so the next one has no copy in memory either.
    let w = restart_after(w, |paths| {
        std::fs::remove_file(paths.review_cache_file(&pr7())).unwrap();
    })
    .await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    let files_before = files_requests(&w).await;
    c.request(Command::DrawDiagrams { pr: pr7() })
        .await
        .unwrap();
    until(&mut watcher, idle).await;
    assert_eq!(
        files_requests(&w).await,
        files_before + 1,
        "the files are read from GitHub again"
    );
    let (_, latest, _) = diagrams_of(&mut c).await;
    let latest = latest.expect("a version");
    let EntryState::Ready { spec, notes } = &latest.entry("flow").expect("flow").state else {
        panic!("ready: {latest:?}");
    };
    assert_eq!(
        spec.nodes[0].anchor.as_ref().map(|a| a.path.as_str()),
        Some("feature.txt"),
        "a file of the pull request keeps its link"
    );
    assert_eq!(notes, &["1 code link dropped".to_string()], "only nope.txt");
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

async fn advance(w: &World, content: &str) -> String {
    let head = advance_pr(w.tmp.path(), 7, "feature.txt", content);
    w.server.reset().await;
    mount_pr(
        &w.server,
        &PrMock::new(&head, &w.base, &w.origin).adding_feature(content),
    )
    .await;
    head
}

fn read_code(number: u32, node: &str) -> Command {
    Command::ReadDiagramCode {
        pr: pr7(),
        number,
        key: "flow".into(),
        node: node.into(),
    }
}

fn comment_on(number: u32, node: &str) -> Command {
    Command::AddDiagramComment {
        pr: pr7(),
        number,
        key: "flow".into(),
        node: node.into(),
        body: "Why this?".into(),
    }
}

async fn code_of(c: &mut Client, number: u32, node: &str) -> DiagramCode {
    match c.request(read_code(number, node)).await.unwrap() {
        Reply::DiagramCode(code) => code,
        other => panic!("the code, got {other:?}"),
    }
}

async fn item_of(c: &mut Client, number: u32, node: &str) -> clusia_core::DraftItem {
    match c.request(comment_on(number, node)).await.unwrap() {
        Reply::DraftItem(item) => item,
        other => panic!("a draft item, got {other:?}"),
    }
}

#[tokio::test]
async fn the_code_of_a_box_is_read_from_the_worktree_and_from_the_pinned_commit_later() {
    let (w, _dir, mut c, mut watcher) = drawing_world(plan_and_drawings()).await;
    c.request(Command::DrawDiagrams { pr: pr7() })
        .await
        .unwrap();
    until(&mut watcher, idle).await;

    // At the head: the file as the worktree has it from the box's first line, with the lines of
    // the box that the pull request adds marked (line 3 is added too, but it is only context).
    let code = code_of(&mut c, 1, "a").await;
    assert_eq!(
        (code.path.as_str(), code.commit.as_str()),
        ("feature.txt", w.head.as_str())
    );
    assert_eq!(code.start_line, 1);
    let shown: Vec<(u32, &str, bool)> = code
        .lines
        .iter()
        .map(|l| (l.number, l.text.as_str(), l.changed))
        .collect();
    assert_eq!(
        shown,
        [(1, "one", true), (2, "two", true), (3, "three", false)]
    );
    assert_eq!(code.unavailable, None);
    let untied = refused(c.request(read_code(1, "b")).await);
    assert!(
        untied.message.contains("not tied to code"),
        "{}",
        untied.message
    );
    assert_eq!(
        refused(c.request(read_code(1, "zz")).await).code,
        ErrorCode::NotFound
    );

    // The commit is pinned, so a later head does not take its code away.
    let pin = sh(
        &w.daemon.paths.worktree_for(&pr7()),
        &["rev-parse", "--verify", "refs/clusia/diagrams/pr-7-v1"],
    );
    assert_eq!(pin, w.head);
    advance(&w, "zero\none\ntwo\nthree\n").await;
    open(&mut c).await;
    let old = code_of(&mut c, 1, "a").await;
    assert_eq!(old.commit, w.head);
    let texts: Vec<&str> = old.lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        texts,
        ["one", "two", "three"],
        "the old file, not the new one"
    );
    assert!(
        old.lines.iter().all(|l| !l.changed),
        "an old version marks nothing"
    );
}

#[tokio::test]
async fn code_that_cannot_be_read_says_so_instead_of_failing() {
    let (w, _dir, mut c, mut watcher) = drawing_world(plan_and_drawings()).await;
    c.request(Command::DrawDiagrams { pr: pr7() })
        .await
        .unwrap();
    until(&mut watcher, idle).await;
    std::fs::remove_file(w.daemon.paths.worktree_for(&pr7()).join("feature.txt")).unwrap();
    let gone = code_of(&mut c, 1, "a").await;
    assert!(gone.lines.is_empty());
    let why = gone.unavailable.expect("a reason");
    assert_eq!(
        why,
        format!("The code at {} is no longer available.", &w.head[..7])
    );
}

#[tokio::test]
async fn a_box_comment_follows_its_lines_to_the_new_head_or_names_them() {
    let (w, _dir, mut c, mut watcher) = drawing_world(plan_and_drawings()).await;
    c.request(Command::DrawDiagrams { pr: pr7() })
        .await
        .unwrap();
    until(&mut watcher, idle).await;

    // At the head the lines are in the diff: a line comment, written by the diagram.
    let first = item_of(&mut c, 1, "a").await;
    assert_eq!(
        (first.kind, first.origin),
        (DraftKind::LineComment, Origin::Diagram)
    );
    let anchor = first.anchor.expect("an anchor");
    assert_eq!(
        (anchor.start_line, anchor.line, anchor.commit.as_str()),
        (Some(1), 2, w.head.as_str())
    );
    assert_eq!(first.body, "Why this?");

    // One line was added above: the same lines are two and three at the new head.
    let head2 = advance(&w, "zero\none\ntwo\nthree\n").await;
    open(&mut c).await;
    let moved = item_of(&mut c, 1, "a").await;
    assert_eq!(moved.kind, DraftKind::LineComment);
    let anchor = moved.anchor.expect("an anchor");
    assert_eq!(
        (anchor.start_line, anchor.line, anchor.commit.as_str()),
        (Some(2), 3, head2.as_str())
    );

    // A line of the range was rewritten: no line comment, a general one that says where.
    advance(&w, "zero\none\nTWO\nthree\n").await;
    open(&mut c).await;
    let general = item_of(&mut c, 1, "a").await;
    assert_eq!(
        (general.kind, general.origin),
        (DraftKind::General, Origin::Diagram)
    );
    assert_eq!(general.anchor, None);
    assert_eq!(general.body, "feature.txt:1-2: Why this?");
}

#[tokio::test]
async fn a_box_without_code_comments_on_the_review_itself() {
    let (w, _dir, mut c, mut watcher) = drawing_world(plan_and_drawings()).await;
    c.request(Command::DrawDiagrams { pr: pr7() })
        .await
        .unwrap();
    until(&mut watcher, idle).await;
    let item = item_of(&mut c, 1, "b").await;
    assert_eq!(
        (item.kind, item.origin),
        (DraftKind::General, Origin::Diagram)
    );
    assert_eq!(item.body, "Why this?");
    drop(w);
}

fn stale_block(json: serde_json::Value) -> String {
    format!("```clusia-diagrams-stale\n{json}\n```")
}

#[tokio::test]
async fn the_revalidation_asks_about_the_diagrams_and_keeps_the_answer() {
    let w = world().await;
    let dir = use_fake(&w, plan_and_drawings()).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let Reply::DraftItem(note) = c
        .request(Command::AddDraftItem {
            pr: pr7(),
            kind: DraftKind::General,
            anchor: None,
            thread: None,
            body: "Looks fine".into(),
        })
        .await
        .unwrap()
    else {
        panic!("the note");
    };
    c.request(Command::DrawDiagrams { pr: pr7() })
        .await
        .unwrap();
    until(&mut watcher, idle).await;
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();

    revalidation::use_script(
        &w,
        Script::one(Turn::answer_blocks(
            "Checked.",
            &[
                revalidation_block(json!({"item": note.id, "verdict": "kept"})),
                stale_block(json!({"stale": true, "reason": "The refresh path changed."})),
            ],
        )),
    )
    .await;
    let head = revalidation::push(&w, &mut c, "zero\n").await;
    revalidation::run_round(&w, &mut c).await;

    let round = revalidation::review_of(&mut c)
        .await
        .revalidation
        .expect("a round");
    assert_eq!(
        round.diagrams_stale,
        Some(DiagramsStale {
            stale: true,
            reason: "The refresh path changed.".into()
        })
    );
    let prompt = FakeClaude::calls(&dir)
        .last()
        .and_then(|call| common::agent::value_of(&call.argv, "-p"))
        .expect("a prompt");
    assert!(prompt.contains("clusia-diagrams-stale"), "{prompt}");
    assert!(
        prompt.contains("Token refresh") && prompt.contains("feature.txt"),
        "{prompt}"
    );

    // Even without a round the listing knows: the review is at another commit.
    let Reply::Diagrams {
        stale,
        stale_reason,
        head: now,
        ..
    } = c.request(Command::GetDiagrams { pr: pr7() }).await.unwrap()
    else {
        panic!("the diagrams");
    };
    assert_eq!((stale, now), (true, head));
    assert_eq!(stale_reason.as_deref(), Some("The refresh path changed."));
}

#[tokio::test]
async fn a_review_without_diagrams_is_not_asked_about_them() {
    let w = world().await;
    let (mut c, ids) = revalidation::saved(&w).await;
    let dir = revalidation::use_script(
        &w,
        Script::one(Turn::answer_blocks(
            "Checked.",
            &[
                revalidation_block(json!({"item": ids.n, "verdict": "kept"})),
                stale_block(json!({"stale": true, "reason": "Nothing was drawn."})),
            ],
        )),
    )
    .await;
    revalidation::push(&w, &mut c, "zero\n").await;
    revalidation::run_round(&w, &mut c).await;
    let round = revalidation::review_of(&mut c)
        .await
        .revalidation
        .expect("a round");
    assert_eq!(round.diagrams_stale, None);
    let prompt = FakeClaude::calls(&dir)
        .last()
        .and_then(|call| common::agent::value_of(&call.argv, "-p"))
        .expect("a prompt");
    assert!(!prompt.contains("clusia-diagrams-stale"), "{prompt}");
}

#[tokio::test]
async fn a_range_with_a_line_rewritten_or_inserted_inside_it_is_not_a_line_comment() {
    let drawing = Turn::answer_blocks(
        "Drawn.",
        &[diagram_block(json!({
            "key": "flow", "kind": "flow", "title": "Token refresh", "caption": "Two steps",
            "nodes": [{"id": "a", "label": "refresh()", "shape": "box",
                       "anchor": {"path": "feature.txt", "start_line": 1, "end_line": 3}}]
        }))],
    );
    let w = world().await;
    use_fake(
        &w,
        Script::turns(vec![
            Turn::answer_blocks("Plan.", &[one_plan_block()]),
            drawing,
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

    // The ends are where they were, but the middle line was rewritten.
    advance(&w, "one\nTWO\nthree\n").await;
    open(&mut c).await;
    let rewritten = item_of(&mut c, 1, "a").await;
    assert_eq!(rewritten.kind, DraftKind::General);
    assert_eq!(rewritten.body, "feature.txt:1-3: Why this?");

    // A line was inserted inside the range: the lines are no longer one after the other.
    advance(&w, "one\nINSERTED\ntwo\nthree\n").await;
    open(&mut c).await;
    let inserted = item_of(&mut c, 1, "a").await;
    assert_eq!(inserted.kind, DraftKind::General);
    assert_eq!(inserted.body, "feature.txt:1-3: Why this?");

    // Lines added above the range move it whole: still a line comment.
    advance(&w, "zero\none\ntwo\nthree\n").await;
    open(&mut c).await;
    let moved = item_of(&mut c, 1, "a").await;
    assert_eq!(moved.kind, DraftKind::LineComment);
    assert_eq!(
        moved.anchor.map(|a| (a.start_line, a.line)),
        Some((Some(2), 4))
    );
}

#[tokio::test]
async fn a_not_stale_answer_about_newer_commits_does_not_cover_an_older_drawing() {
    let w = world().await;
    let _dir = use_fake(&w, plan_and_drawings()).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let Reply::DraftItem(note) = c
        .request(Command::AddDraftItem {
            pr: pr7(),
            kind: DraftKind::General,
            anchor: None,
            thread: None,
            body: "Looks fine".into(),
        })
        .await
        .unwrap()
    else {
        panic!("the note");
    };
    c.request(Command::DrawDiagrams { pr: pr7() })
        .await
        .unwrap();
    until(&mut watcher, idle).await;
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    // The drawing is older than the commits the round will read.
    let file = w.daemon.paths.diagrams_for(&pr7()).join("v1.json");
    let stored = std::fs::read_to_string(&file).unwrap();
    let mut version: serde_json::Value = serde_json::from_str(&stored).unwrap();
    version["commit"] = json!("0".repeat(40));
    std::fs::write(&file, version.to_string()).unwrap();

    revalidation::use_script(
        &w,
        Script::one(Turn::answer_blocks(
            "Checked.",
            &[
                revalidation_block(json!({"item": note.id, "verdict": "kept"})),
                stale_block(json!({"stale": false, "reason": "Only docs changed."})),
            ],
        )),
    )
    .await;
    revalidation::push(&w, &mut c, "zero\n").await;
    revalidation::run_round(&w, &mut c).await;
    let round = revalidation::review_of(&mut c)
        .await
        .revalidation
        .expect("a round");
    assert_eq!(round.diagrams_stale, None);
    let Reply::Diagrams {
        stale,
        stale_reason,
        ..
    } = c.request(Command::GetDiagrams { pr: pr7() }).await.unwrap()
    else {
        panic!("the diagrams");
    };
    assert_eq!((stale, stale_reason), (true, None));
}

#[tokio::test]
async fn the_keywords_entry_is_drawn_last_from_the_commit_and_never_reaches_the_chat() {
    let keyword_plan = Turn::answer_blocks(
        "The words.",
        &[keyword_plan_block(json!({
            "title": "Where the words live", "caption": "c",
            "axes": [
                {"name": "Layer", "low": "a", "high": "b"},
                {"name": "Time", "low": "a", "high": "b"},
                {"name": "Risk", "low": "a", "high": "b"}
            ],
            "families": [{"id": "count", "label": "count",
                "seed": {"word": "two", "path": "feature.txt", "line": 2},
                "words": ["two", "three"]}],
            "use_cases": [{"id": "feature", "name": "The feature"}]
        }))],
    );
    let placement = Turn::answer_blocks(
        "Placed.",
        &[keyword_placement_block(json!({
            "regions": [{"path": "feature.txt", "use_case": "feature", "at": [0.2, 0.8, 0.5]}]
        }))],
    );
    let script = Script::turns(vec![
        plan(),
        drawing("flow", "flow"),
        drawing("graph", "graph"),
        keyword_plan,
        placement,
    ]);
    let w = world().await;
    set_config(&w, "harness.keywords_3d", "true").await;
    let _dir = use_fake(&w, script).await;
    let mut watcher = watching(&w).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(Command::DrawDiagrams { pr: pr7() })
        .await
        .unwrap();
    let events = until(&mut watcher, idle).await;
    let drawing = |done, key: &str, step: Option<&str>| DiagramsState::Drawing {
        done,
        total: 3,
        key: key.into(),
        step: step.map(str::to_string),
    };
    let states: Vec<&DiagramsState> = events
        .iter()
        .filter_map(|e| match e {
            Event::DiagramsChanged { state, .. } => Some(state),
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        [
            &DiagramsState::Planning,
            &drawing(0, "flow", None),
            &drawing(1, "graph", None),
            &drawing(2, "keywords", None),
            &drawing(2, "keywords", Some("Finding where the words live…")),
            &drawing(2, "keywords", Some("Placing files on the axes…")),
            &DiagramsState::Idle,
        ]
    );
    let (_, latest, _) = diagrams_of(&mut c).await;
    let latest = latest.expect("a version");
    let keys: Vec<&str> = latest.entries.iter().map(|e| e.plan.key.as_str()).collect();
    assert_eq!(keys, ["flow", "graph", "keywords"]);
    let EntryState::ReadyMap { map, .. } = &latest.entries[2].state else {
        panic!("a map: {:?}", latest.entries[2].state);
    };
    assert_eq!(map.paths, ["feature.txt"]);
    assert_eq!(map.places.len(), 2, "two and three, each on its own line");
    assert!(map.places.iter().all(|p| p.changed()));
    let Reply::AgentLog(chat) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap() else {
        panic!("the log");
    };
    assert!(chat.is_empty(), "{chat:?}");
}
