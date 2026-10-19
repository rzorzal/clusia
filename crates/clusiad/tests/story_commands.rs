//! A command the daemon does not answer yet is refused as an invalid state.

mod common;

use clusia_protocol::{Command, ErrorCode};
use common::agent::refused;
use common::review_world::{pr7, world};

#[tokio::test]
async fn the_story_and_calls_commands_are_not_available_yet() {
    let w = world().await;
    let mut c = w.daemon.client().await;
    let commands = vec![
        Command::WriteStory { pr: pr7() },
        Command::RewriteStory { pr: pr7() },
        Command::StopStory { pr: pr7() },
        Command::GetStory { pr: pr7() },
        Command::GetStoryVersion {
            pr: pr7(),
            number: 1,
        },
        Command::MarkStepRead {
            pr: pr7(),
            number: 1,
            step: "a".into(),
            read: true,
        },
        Command::AddStepComment {
            pr: pr7(),
            number: 1,
            step: "a".into(),
            body: "Why?".into(),
        },
        Command::GetCalls {
            pr: pr7(),
            center: None,
            depth: None,
            expand: Vec::new(),
        },
        Command::FindFunction {
            pr: pr7(),
            query: "refresh".into(),
        },
        Command::ReadFunctionCode {
            pr: pr7(),
            function: "src/a.rs#f#1".into(),
        },
        Command::AddCallsComment {
            pr: pr7(),
            function: "src/a.rs#f#1".into(),
            body: "Why?".into(),
        },
    ];
    for command in commands {
        let refusal = refused(c.request(command.clone()).await);
        assert_eq!(refusal.code, ErrorCode::InvalidState, "{command:?}");
        assert_eq!(refusal.message, "Story and Calls are not available yet");
    }
}
