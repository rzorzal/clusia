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

/// No network, no real gh, no Keychain, no background loop.
pub fn test_options() -> DaemonOptions {
    DaemonOptions {
        github_api: Some("http://127.0.0.1:9".into()),
        github_token: None,
        gh_program: PathBuf::from("/nonexistent/gh"),
        secrets: Arc::new(MemoryStore::default()),
        background_sync: false,
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

    pub async fn start_in(dir: tempfile::TempDir) -> Self {
        Self::start_with(dir, test_options()).await
    }

    pub async fn start_with(dir: tempfile::TempDir, options: DaemonOptions) -> Self {
        let paths = Paths::new(dir.path());
        let daemon = Daemon::bind_with(paths.clone(), options)
            .await
            .expect("daemon binds");
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
