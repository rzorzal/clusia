//! Every agent command is refused with `InvalidState`.

mod common;

use clusia_core::PrRef;
use clusia_protocol::{ClientError, Command, ErrorCode};
use common::TestDaemon;

#[tokio::test]
async fn agent_commands_are_refused_with_invalid_state() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let pr: PrRef = "acme/widgets#7".parse().unwrap();
    for cmd in [
        Command::AgentSend {
            pr: pr.clone(),
            text: "hi".into(),
        },
        Command::AgentCancel { pr: pr.clone() },
        Command::AcceptSuggestion {
            pr: pr.clone(),
            id: "sug-1".into(),
            body: None,
        },
        Command::DismissSuggestion {
            pr: pr.clone(),
            id: "sug-1".into(),
        },
        Command::GetAgentLog { pr: pr.clone() },
        Command::HarnessProbe,
    ] {
        match c.request(cmd.clone()).await {
            Err(ClientError::Server(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidState, "{cmd:?}");
                assert!(e.message.contains("agent"), "{}", e.message);
            }
            other => panic!("{cmd:?}: expected a server error, got {other:?}"),
        }
    }
    d.stop().await;
}
