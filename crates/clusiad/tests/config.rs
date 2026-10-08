mod common;

use std::time::Duration;

use clusia_core::Config;
use clusia_protocol::{ClientError, Command, ErrorCode, Event, Reply, topics};
use common::TestDaemon;

fn error_code(r: Result<Reply, ClientError>) -> ErrorCode {
    match r {
        Err(ClientError::Server(e)) => e.code,
        other => panic!("expected a server error, got {other:?}"),
    }
}

#[tokio::test]
async fn get_config_returns_defaults() {
    let d = TestDaemon::start().await;
    assert_eq!(
        d.client().await.request(Command::GetConfig).await.unwrap(),
        Reply::Config(Config::default())
    );
    d.stop().await;
}

#[tokio::test]
async fn set_value_persists_and_replies_rendered() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let set = Command::SetConfigValue {
        key: "github.poll_interval_secs".into(),
        value: "120".into(),
    };
    assert_eq!(c.request(set).await.unwrap(), Reply::Value("120".into()));
    let get = Command::GetConfigValue {
        key: "github.poll_interval_secs".into(),
    };
    assert_eq!(c.request(get).await.unwrap(), Reply::Value("120".into()));
    let file = std::fs::read_to_string(d.paths.config_file()).unwrap();
    assert!(file.contains("poll_interval_secs = 120"), "{file}");
    d.stop().await;
}

#[tokio::test]
async fn invalid_value_is_rejected_and_nothing_is_written() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let set = Command::SetConfigValue {
        key: "github.poll_interval_secs".into(),
        value: "abc".into(),
    };
    assert_eq!(
        error_code(c.request(set).await),
        ErrorCode::InvalidConfigValue
    );
    assert!(!d.paths.config_file().exists());
    assert_eq!(
        c.request(Command::GetConfig).await.unwrap(),
        Reply::Config(Config::default())
    );
    d.stop().await;
}

#[tokio::test]
async fn unknown_key_is_rejected() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    assert_eq!(
        error_code(
            c.request(Command::GetConfigValue {
                key: "nope.key".into()
            })
            .await
        ),
        ErrorCode::UnknownConfigKey
    );
    d.stop().await;
}

#[tokio::test]
async fn subscribers_receive_config_changed() {
    let d = TestDaemon::start().await;
    let mut watcher = d.client().await;
    watcher
        .request(Command::Subscribe {
            topics: vec![topics::CONFIG.into()],
        })
        .await
        .unwrap();
    let mut writer = d.client().await;
    writer
        .request(Command::SetConfigValue {
            key: "appearance.theme".into(),
            value: "dark".into(),
        })
        .await
        .unwrap();
    let (topic, event) = tokio::time::timeout(common::EVENT_WAIT, watcher.next_event())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(topic, topics::CONFIG);
    assert_eq!(
        event,
        Event::ConfigChanged {
            key: "appearance.theme".into(),
            value: "dark".into()
        }
    );
    d.stop().await;
}

#[tokio::test]
async fn non_subscribers_get_no_events() {
    let d = TestDaemon::start().await;
    let mut quiet = d.client().await;
    let mut writer = d.client().await;
    writer
        .request(Command::SetConfigValue {
            key: "appearance.theme".into(),
            value: "light".into(),
        })
        .await
        .unwrap();
    quiet.request(Command::DaemonStatus).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), quiet.next_event())
            .await
            .is_err()
    );
    d.stop().await;
}

#[tokio::test]
async fn corrupt_config_on_disk_is_quarantined() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[[[ not toml").unwrap();
    let d = TestDaemon::start_in(dir).await;
    assert_eq!(
        d.client().await.request(Command::GetConfig).await.unwrap(),
        Reply::Config(Config::default())
    );
    let quarantined = std::fs::read_dir(d.paths.root())
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("config.toml.corrupt-")
        });
    assert!(quarantined);
    d.stop().await;
}

#[tokio::test]
async fn config_survives_restart() {
    let d = TestDaemon::start().await;
    d.client()
        .await
        .request(Command::SetConfigValue {
            key: "github.poll_interval_secs".into(),
            value: "300".into(),
        })
        .await
        .unwrap();
    let dir = d.stop().await;
    let d = TestDaemon::start_in(dir).await;
    let got = d
        .client()
        .await
        .request(Command::GetConfigValue {
            key: "github.poll_interval_secs".into(),
        })
        .await;
    assert_eq!(got.unwrap(), Reply::Value("300".into()));
    d.stop().await;
}
