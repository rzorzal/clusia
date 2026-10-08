mod common;

use clusia_core::launch_agent::{self, LaunchAgent};
use clusia_protocol::{Command, Reply};
use common::TestDaemon;

fn agent_text(start_at_login: bool) -> String {
    launch_agent::render(&LaunchAgent {
        daemon: "/Applications/Clusia.app/Contents/MacOS/clusiad".as_ref(),
        log: "/tmp/daemon.launchd.log".as_ref(),
        start_at_login,
    })
}

#[tokio::test]
async fn start_at_login_changes_the_setting_and_the_agent_file() {
    let d = TestDaemon::start().await;
    let agent = d.paths.launch_agent();
    std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
    std::fs::write(&agent, agent_text(true)).unwrap();
    let mut c = d.client().await;

    let reply = c
        .request(Command::SetStartAtLogin { on: false })
        .await
        .unwrap();
    assert_eq!(reply, Reply::Ack);
    let text = std::fs::read_to_string(&agent).unwrap();
    assert_eq!(launch_agent::start_at_login(&text), Some(false));
    assert_eq!(text, agent_text(false), "only RunAtLoad changed");
    let get = Command::GetConfigValue {
        key: "general.start_at_login".into(),
    };
    assert_eq!(c.request(get).await.unwrap(), Reply::Value("false".into()));

    c.request(Command::SetStartAtLogin { on: true })
        .await
        .unwrap();
    let text = std::fs::read_to_string(&agent).unwrap();
    assert_eq!(launch_agent::start_at_login(&text), Some(true));
    d.stop().await;
}

#[tokio::test]
async fn without_an_installed_agent_only_the_setting_changes() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let reply = c
        .request(Command::SetStartAtLogin { on: true })
        .await
        .unwrap();
    assert_eq!(reply, Reply::Ack);
    assert!(
        !d.paths.launch_agent().exists(),
        "a daemon run from a build folder writes no agent"
    );
    d.stop().await;
}

#[tokio::test]
async fn a_foreign_agent_file_is_refused_and_left_alone() {
    let d = TestDaemon::start().await;
    let agent = d.paths.launch_agent();
    std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
    std::fs::write(&agent, "not a plist").unwrap();
    let mut c = d.client().await;
    assert!(
        c.request(Command::SetStartAtLogin { on: false })
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&agent).unwrap(), "not a plist");
    d.stop().await;
}
