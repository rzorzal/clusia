#![allow(dead_code)] // each test file uses a different subset

use std::sync::Arc;

use clusia_core::Paths;
use clusia_platform::MemoryStore;
use clusia_protocol::Client;
use clusiad::{DaemonOptions, RecordingSpawner, ShutdownHandle};
use tokio::task::JoinHandle;

/// An in-process clusiad: no network, no gh, no Keychain, no background sync, no tray.
pub struct Daemon {
    pub dir: tempfile::TempDir,
    pub paths: Paths,
    handle: ShutdownHandle,
    task: JoinHandle<std::io::Result<()>>,
}

impl Daemon {
    pub async fn start() -> Self {
        Self::start_in(tempfile::tempdir().unwrap()).await
    }

    /// A daemon with a token that talks to `api` (a wiremock server).
    pub async fn start_with_github(api: String) -> Self {
        Self::start_with(tempfile::tempdir().unwrap(), api, Some("test-token".into())).await
    }

    pub async fn start_in(dir: tempfile::TempDir) -> Self {
        Self::start_with(dir, "http://127.0.0.1:9".into(), None).await
    }

    async fn start_with(dir: tempfile::TempDir, api: String, token: Option<String>) -> Self {
        let paths = Paths::new(dir.path());
        let options = DaemonOptions {
            github_api: Some(api),
            github_token: token,
            gh_program: "/nonexistent/gh".into(),
            secrets: Arc::new(MemoryStore::default()),
            background_sync: false,
            tray_program: None,
            spawner: Arc::new(RecordingSpawner::default()),
            media_extra_hosts: Vec::new(),
            media_allow_local: false,
        };
        let daemon = clusiad::Daemon::bind_with(paths.clone(), options)
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

    pub async fn stop(self) -> tempfile::TempDir {
        self.handle.trigger();
        self.wait().await
    }

    /// Waits for the daemon to exit on its own (e.g. after a `Shutdown` request).
    pub async fn wait(self) -> tempfile::TempDir {
        self.task.await.unwrap().unwrap();
        self.dir
    }
}
