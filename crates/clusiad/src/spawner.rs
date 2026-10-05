//! Starting outside programs (the editor). Tests record instead of launching.

use std::io;
use std::process::{Command, Stdio};
use std::sync::Mutex;

pub trait Spawner: Send + Sync {
    /// Starts `argv[0]` with the rest as arguments and returns without waiting for it.
    fn spawn(&self, argv: &[String]) -> io::Result<()>;
}

/// Launches real processes, detached from the daemon's stdio; a thread reaps each one.
pub struct ProcessSpawner;

impl Spawner for ProcessSpawner {
    fn spawn(&self, argv: &[String]) -> io::Result<()> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// Remembers every argv instead of launching it.
#[derive(Default)]
pub struct RecordingSpawner(Mutex<Vec<Vec<String>>>);

impl RecordingSpawner {
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl Spawner for RecordingSpawner {
    fn spawn(&self, argv: &[String]) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(argv.to_vec());
        Ok(())
    }
}
