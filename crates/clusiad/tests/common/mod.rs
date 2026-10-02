#![allow(dead_code)] // each test file uses a different subset

use clusia_core::Paths;
use clusia_protocol::Client;
use clusiad::{Daemon, ShutdownHandle};
use tokio::task::JoinHandle;

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
        let paths = Paths::new(dir.path());
        let daemon = Daemon::bind(paths.clone()).await.expect("daemon binds");
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
