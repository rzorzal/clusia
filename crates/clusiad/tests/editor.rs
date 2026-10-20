//! `OpenInEditor` runs the configured editor's argv (recorded, never launched) for files inside
//! the Clúsia home, and refuses everything else.

mod common;

use std::sync::Arc;

use clusia_protocol::{ClientError, Command, ErrorCode, Reply};
use clusiad::RecordingSpawner;
use common::{TestDaemon, test_options};

async fn daemon() -> (TestDaemon, Arc<RecordingSpawner>) {
    let spawner = Arc::new(RecordingSpawner::default());
    let mut options = test_options();
    options.spawner = spawner.clone();
    (
        TestDaemon::start_with(tempfile::tempdir().unwrap(), options).await,
        spawner,
    )
}

fn code(e: ClientError) -> ErrorCode {
    match e {
        ClientError::Server(e) => e.code,
        other => panic!("expected a server error, got {other:?}"),
    }
}

#[tokio::test]
async fn open_in_editor_runs_the_configured_editor() {
    let (d, spawner) = daemon().await;
    let file = d.paths.root().join("notes.txt");
    std::fs::write(&file, "x").unwrap();
    let mut c = d.client().await;
    let open = |line| Command::OpenInEditor {
        path: file.display().to_string(),
        line,
    };
    assert_eq!(c.request(open(Some(3))).await.unwrap(), Reply::Ack);
    let calls = spawner.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0][0], "/usr/bin/open");
    assert!(calls[0][1].starts_with("vscode://file/"), "{:?}", calls[0]);
    assert!(calls[0][1].ends_with("/notes.txt:3"), "{:?}", calls[0]);

    c.request(Command::SetConfigValue {
        key: "editor.custom_command".into(),
        value: "myeditor --line {line} {path}".into(),
    })
    .await
    .unwrap();
    c.request(Command::SetConfigValue {
        key: "editor.kind".into(),
        value: "custom".into(),
    })
    .await
    .unwrap();
    c.request(open(None)).await.unwrap();
    let last = spawner.calls().pop().unwrap();
    assert_eq!(last[..3], ["myeditor", "--line", "1"]);
    assert!(last[3].ends_with("/notes.txt"));
    d.stop().await;
}

#[tokio::test]
async fn open_in_editor_refuses_outside_paths() {
    let (d, spawner) = daemon().await;
    let mut c = d.client().await;
    let outside = tempfile::NamedTempFile::new().unwrap();
    for path in [
        outside.path().display().to_string(),
        d.paths.root().join("missing.rs").display().to_string(),
        d.paths.root().join("../escape").display().to_string(),
    ] {
        let err = c
            .request(Command::OpenInEditor { path, line: None })
            .await
            .unwrap_err();
        assert_eq!(code(err), ErrorCode::BadRequest);
    }
    assert!(spawner.calls().is_empty(), "nothing was launched");
    d.stop().await;
}

/// Fails every launch the way `open` does when no program handles the URL.
struct FailingSpawner;

impl clusiad::Spawner for FailingSpawner {
    fn spawn(&self, _argv: &[String]) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "exit status: 1: no application knows how to open the URL",
        ))
    }
}

#[tokio::test]
async fn open_in_editor_reports_a_launch_that_fails() {
    let mut options = test_options();
    options.spawner = Arc::new(FailingSpawner);
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options).await;
    let file = d.paths.root().join("notes.txt");
    std::fs::write(&file, "x").unwrap();
    let mut c = d.client().await;
    let err = c
        .request(Command::OpenInEditor {
            path: file.display().to_string(),
            line: Some(1),
        })
        .await
        .unwrap_err();
    match err {
        ClientError::Server(e) => {
            assert_eq!(e.code, ErrorCode::Internal);
            assert!(e.message.contains("no application knows"), "{}", e.message);
            assert!(e.message.contains("/usr/bin/open"), "{}", e.message);
        }
        other => panic!("expected a server error, got {other:?}"),
    }
    d.stop().await;
}
