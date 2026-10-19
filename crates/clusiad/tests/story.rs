mod common;

use clusia_harness::testkit::{
    CodexScript, CodexTurn, CustomScript, CustomTurnScript, Script, Turn, story_block,
};
use clusia_protocol::{Client, Command, ErrorCode, Event, Reply, StoryState};
use common::agent::{calls, refused, set_config, until, use_fake, watching};
use common::codex::use_fake_codex;
use common::custom::use_fake_custom;
use common::review_world::{World, open, pr7, world};
use serde_json::json;

/// A story of the world's pull request: it adds the three lines of `feature.txt`.
fn block() -> String {
    story_block(json!({
        "sentence": "A feature file arrives.",
        "steps": [{"id": "feature", "title": "The feature file",
                   "hunks": [{"path": "feature.txt", "start_line": 1, "end_line": 3}]}]
    }))
}

fn idle(e: &Event) -> bool {
    matches!(
        e,
        Event::StoryChanged {
            state: StoryState::Idle,
            ..
        }
    )
}

async fn story(c: &mut Client) -> (Option<clusia_core::story::StoryVersion>, StoryState) {
    let Reply::Story { latest, state, .. } =
        c.request(Command::GetStory { pr: pr7() }).await.unwrap()
    else {
        panic!("a story");
    };
    (latest, state)
}

async fn written_with(w: &World, mut watcher: Client) {
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    assert_eq!(
        c.request(Command::WriteStory { pr: pr7() }).await.unwrap(),
        Reply::Ack
    );
    until(&mut watcher, idle).await;
    let (latest, state) = story(&mut c).await;
    assert_eq!(state, StoryState::Idle);
    let latest = latest.expect("a version");
    assert_eq!(latest.number, 1);
    assert_eq!(latest.spec.expect("written").steps[0].id, "feature");
    let refusal = refused(c.request(Command::WriteStory { pr: pr7() }).await);
    assert_eq!(
        (refusal.code, refusal.message.as_str()),
        (
            ErrorCode::InvalidState,
            "A story already exists; use Rewrite"
        )
    );
}

#[tokio::test]
async fn claude_code_writes_the_story_and_the_chat_never_sees_it() {
    let w = world().await;
    let dir = use_fake(&w, Script::one(Turn::answer_blocks("Story.", &[block()]))).await;
    let watcher = watching(&w).await;
    written_with(&w, watcher).await;
    let call = calls(&dir, 1).await.remove(0);
    assert!(call.argv.iter().any(|a| a == "-p"));
    let mut c = w.daemon.client().await;
    let Reply::AgentLog(entries) = c.request(Command::GetAgentLog { pr: pr7() }).await.unwrap()
    else {
        panic!("the log");
    };
    assert!(
        entries.iter().all(|e| !matches!(
            e,
            clusia_protocol::AgentLogEntry::Text { text, .. } if text.contains("Story.")
        )),
        "the story's answer never shows in the chat"
    );
    // The review is discarded: its stories go with it.
    let stories = w.daemon.paths.stories_for(&pr7());
    assert!(stories.is_dir());
    c.request(Command::DiscardReview { pr: pr7() })
        .await
        .unwrap();
    assert!(!stories.exists());
    w.daemon.stop().await;
}

#[tokio::test]
async fn codex_writes_the_story() {
    let w = world().await;
    use_fake_codex(
        &w,
        CodexScript::turns(vec![CodexTurn::answer_blocks("Story.", &[block()])]),
    )
    .await;
    let watcher = watching(&w).await;
    written_with(&w, watcher).await;
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_custom_command_writes_the_story() {
    let w = world().await;
    use_fake_custom(
        &w,
        CustomScript::new().turns(vec![
            CustomTurnScript::new()
                .session("story-1")
                .final_(&format!("Story.\n{}", block())),
        ]),
    )
    .await;
    let watcher = watching(&w).await;
    written_with(&w, watcher).await;
    w.daemon.stop().await;
}

#[tokio::test]
async fn the_story_needs_an_open_review() {
    let w = world().await;
    set_config(&w, "harness.program", "/nonexistent/claude").await;
    let mut c = w.daemon.client().await;
    let refusal = refused(c.request(Command::WriteStory { pr: pr7() }).await);
    assert_eq!(refusal.code, ErrorCode::InvalidState);
    w.daemon.stop().await;
}
