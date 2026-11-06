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
