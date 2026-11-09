mod common;

use clusia_core::PrRef;
use clusia_core::tests::TestsPhase;
use clusia_protocol::{ClientError, Command, ErrorCode, Reply};
use common::TestDaemon;

#[tokio::test]
async fn the_test_commands_answer_through_the_socket() {
    let daemon = TestDaemon::start().await;
    let mut client = daemon.client().await;
    let pr: PrRef = "acme/widgets#7".parse().unwrap();
    let Reply::Tests(view) = client
        .request(Command::GetTests { pr: pr.clone() })
        .await
        .unwrap()
    else {
        panic!("a view");
    };
    assert_eq!(view.phase, TestsPhase::NotGenerated);
    let Err(ClientError::Server(refusal)) = client.request(Command::GenerateTests { pr }).await
    else {
        panic!("refused");
    };
    assert_eq!(refusal.code, ErrorCode::InvalidState);
    assert_eq!(
        refusal.message,
        "no review for acme/widgets#7; open it first"
    );
    let Reply::TestDisk(report) = client.request(Command::GetTestDisk).await.unwrap() else {
        panic!("a report");
    };
    assert!(
        report.items.is_empty(),
        "no engine and no cache: {report:?}"
    );
    let Reply::TestEngine(info) = client
        .request(Command::CheckTestEngine { pr: None })
        .await
        .unwrap()
    else {
        panic!("an engine report");
    };
    assert!(info.cores >= 1);
    assert_eq!(info.engine, None, "the test daemon has no docker");
    daemon.stop().await;
}

#[tokio::test]
async fn the_cockpit_is_served_through_the_wire() {
    use clusia_core::cockpit::{CopilotAsk, NO_REVIEW};

    let daemon = TestDaemon::start().await;
    let mut client = daemon.client().await;
    let pr: PrRef = "acme/widgets#7".parse().unwrap();
    match client.request(Command::GetCockpit { pr: pr.clone() }).await {
        Ok(Reply::Cockpit(view)) => assert_eq!(view.note.as_deref(), Some(NO_REVIEW)),
        other => panic!("not a cockpit view: {other:?}"),
    }
    let Err(ClientError::Server(refusal)) =
        client.request(Command::OpenMain { pr: pr.clone() }).await
    else {
        panic!("OpenMain was not refused without a review");
    };
    assert!(
        !refusal.message.contains("not served yet"),
        "{}",
        refusal.message
    );
    let Err(ClientError::Server(refusal)) = client
        .request(Command::AskCopilot {
            pr: pr.clone(),
            item: None,
            ask: CopilotAsk::Next,
        })
        .await
    else {
        panic!("the copilot was not refused without a review");
    };
    assert!(
        !refusal.message.contains("not served yet"),
        "{}",
        refusal.message
    );
    daemon.stop().await;
}

#[tokio::test]
async fn system_commands_are_refused_until_served() {
    use clusia_core::cockpit::RunSide;

    let daemon = TestDaemon::start().await;
    let mut client = daemon.client().await;
    let pr: PrRef = "acme/widgets#7".parse().unwrap();
    let commands = [Command::SendEndpoint {
        pr,
        side: RunSide::Pr,
        endpoint: "GET /".into(),
    }];
    for command in commands {
        let Err(ClientError::Server(refusal)) = client.request(command.clone()).await else {
            panic!("{command:?} was not refused");
        };
        assert_eq!(refusal.code, ErrorCode::InvalidState, "{command:?}");
        assert_eq!(refusal.message, "not served yet", "{command:?}");
    }
    daemon.stop().await;
}

#[tokio::test]
async fn the_system_is_served_through_the_wire() {
    use clusia_core::cockpit::RunSide;
    use clusia_core::system::{NO_REVIEW_SYSTEM, SystemState, SystemSupport};

    let daemon = TestDaemon::start().await;
    let mut client = daemon.client().await;
    let pr = |text: &str| -> PrRef { text.parse().unwrap() };
    // A review that is not open: the view says why, and the writes are refused with text.
    let Ok(Reply::System(view)) = client
        .request(Command::GetSystem {
            pr: pr("acme/widgets#7"),
        })
        .await
    else {
        panic!("a system view");
    };
    assert_eq!(
        view.support,
        SystemSupport::Unavailable {
            reason: NO_REVIEW_SYSTEM.into()
        }
    );
    assert_eq!(view.state, SystemState::NotStarted);
    for command in [
        Command::StopSystem {
            pr: pr("acme/widgets#7"),
            side: RunSide::Pr,
        },
        Command::WriteEndpoints {
            pr: pr("acme/widgets#7"),
        },
        Command::FixSystem {
            pr: pr("acme/widgets#7"),
        },
    ] {
        let Err(ClientError::Server(err)) = client.request(command.clone()).await else {
            panic!("{command:?} was not refused");
        };
        assert_eq!(err.code, ErrorCode::InvalidState);
        assert_ne!(err.message, "not served yet");
    }
    let Err(ClientError::Server(err)) = client
        .request(Command::GetSystemLogs {
            pr: pr("acme/widgets#7"),
            side: RunSide::Pr,
            process: "../x".into(),
            lines: 10,
        })
        .await
    else {
        panic!("a bad process id was not refused");
    };
    assert_eq!(err.message, "invalid process id");
    daemon.stop().await;
}
