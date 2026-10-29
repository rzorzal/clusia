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
    daemon.stop().await;
}
