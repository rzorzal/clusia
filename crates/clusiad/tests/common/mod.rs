#![allow(dead_code)] // each test file uses a different subset

pub mod git_fixture;
pub mod github_mock;
pub mod review_world;

use std::path::PathBuf;
use std::sync::Arc;

use clusia_core::Paths;
use clusia_platform::MemoryStore;
use clusia_protocol::Client;
use clusiad::{Daemon, DaemonOptions, ShutdownHandle};
use tokio::task::JoinHandle;

/// How long a test waits for an event the daemon pushes. It returns as soon as the event
/// arrives; the margin only matters on a loaded machine.
pub const EVENT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// No network, no real gh, no Keychain, no background loop.
pub fn test_options() -> DaemonOptions {
    DaemonOptions {
        github_api: Some("http://127.0.0.1:9".into()),
        github_token: None,
        gh_program: PathBuf::from("/nonexistent/gh"),
        secrets: Arc::new(MemoryStore::default()),
        background_sync: false,
        tray_program: None,
        spawner: Arc::new(clusiad::RecordingSpawner::default()),
        media_extra_hosts: Vec::new(),
        media_allow_local: false,
        media_resolve: Vec::new(),
        giphy_api: Some("http://127.0.0.1:9".into()),
        harness_search_paths: Vec::new(),
    }
}

pub struct TestDaemon {
    pub dir: tempfile::TempDir,
    pub paths: Paths,
    handle: ShutdownHandle,
    task: JoinHandle<std::io::Result<()>>,
}

impl TestDaemon {
    pub async fn start() -> Self {
        Self::start_in(tempfile::tempdir().unwrap()).await
    }

    /// A restart in a home whose lock or socket a parallel test's forked child still holds (it
    /// keeps inherited descriptors until it execs) is refused for a moment: retry until it lets go.
    pub async fn start_in(dir: tempfile::TempDir) -> Self {
        let paths = Paths::new(dir.path());
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let daemon = loop {
            match Daemon::bind_with(paths.clone(), test_options()).await {
                Err(clusiad::StartError::AlreadyRunning(_))
                    if std::time::Instant::now() < give_up =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                other => break other.expect("daemon binds"),
            }
        };
        Self::running(dir, paths, daemon)
    }

    pub async fn start_with(dir: tempfile::TempDir, options: DaemonOptions) -> Self {
        let paths = Paths::new(dir.path());
        let daemon = Daemon::bind_with(paths.clone(), options)
            .await
            .expect("daemon binds");
        Self::running(dir, paths, daemon)
    }

    fn running(dir: tempfile::TempDir, paths: Paths, daemon: Daemon) -> Self {
        let handle = daemon.shutdown_handle();
        let task = tokio::spawn(daemon.run());
        Self {
            dir,
            paths,
            handle,
            task,
        }
    }

    pub async fn client(&self) -> Client {
        Client::connect(&self.paths.socket(), "test")
            .await
            .expect("client connects")
    }

    /// Triggers shutdown and waits for the daemon to exit.
    pub async fn stop(self) -> tempfile::TempDir {
        self.handle.trigger();
        self.wait().await
    }

    /// Waits for the daemon to exit on its own (e.g. after a Shutdown command).
    pub async fn wait(self) -> tempfile::TempDir {
        self.task.await.unwrap().unwrap();
        self.dir
    }
}
