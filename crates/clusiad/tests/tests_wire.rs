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
async fn cockpit_commands_are_refused_until_served() {
    use clusia_core::cockpit::{
        BenchAsk, BenchInput, CockpitScope, CopilotAsk, ItemKind, ItemRef, ScenarioAsk,
        ScenarioInput,
    };
    use clusia_core::tests::Approve;

    let daemon = TestDaemon::start().await;
    let mut client = daemon.client().await;
    let pr: PrRef = "acme/widgets#7".parse().unwrap();
    let item = ItemRef {
        kind: ItemKind::Scenario,
        id: "refresh".into(),
    };
    let commands = vec![
        Command::GetCockpit { pr: pr.clone() },
        Command::SaveScenario {
            pr: pr.clone(),
            scenario: "refresh".into(),
            input: ScenarioInput {
                values: Default::default(),
                files: Default::default(),
                run: None,
                rounds: None,
                timeout_secs: None,
            },
        },
        Command::ResetScenario {
            pr: pr.clone(),
            scenario: "refresh".into(),
        },
        Command::RemoveScenario {
            pr: pr.clone(),
            scenario: "refresh".into(),
        },
        Command::ApproveScenario {
            pr: pr.clone(),
            scenario: "refresh".into(),
            hash: "h".into(),
        },
        Command::GetScenarioScript {
            pr: pr.clone(),
            scenario: "refresh".into(),
        },
        Command::WriteScenarios {
            pr: pr.clone(),
            ask: ScenarioAsk::Fresh,
        },
        Command::SaveBench {
            pr: pr.clone(),
            function: "f-0123456789".into(),
            input: BenchInput {
                values: Default::default(),
                warmup: None,
                secs: None,
                rounds: None,
            },
        },
        Command::ResetBench {
            pr: pr.clone(),
            function: "f-0123456789".into(),
        },
        Command::WriteBench {
            pr: pr.clone(),
            function: "f-0123456789".into(),
            ask: BenchAsk::Fresh,
        },
        Command::OpenMain { pr: pr.clone() },
        Command::PrepareMain {
            pr: pr.clone(),
            main_hash: "h".into(),
            approve: Approve::All,
        },
        Command::RunCockpit {
            pr: pr.clone(),
            scope: CockpitScope::Scenarios,
            on_main: false,
            seen: "h".into(),
        },
        Command::AskCopilot {
            pr: pr.clone(),
            item: Some(item),
            ask: CopilotAsk::Next,
        },
    ];
    assert_eq!(commands.len(), 14, "every new command is covered");
    for command in commands {
        let name = format!("{command:?}");
        let Err(ClientError::Server(refusal)) = client.request(command).await else {
            panic!("{name} was not refused");
        };
        assert_eq!(refusal.code, ErrorCode::InvalidState, "{name}");
        assert_eq!(
            refusal.message, "The QA cockpit is not served yet",
            "{name}"
        );
    }
    daemon.stop().await;
}
