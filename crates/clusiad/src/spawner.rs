//! Starting outside programs (the editor). Tests record instead of launching.

use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

pub trait Spawner: Send + Sync {
    /// Starts `argv[0]` with the rest as arguments. It may block briefly to see the program fail.
    fn spawn(&self, argv: &[String]) -> io::Result<()>;
}

/// How long a launched program may take to fail before it counts as started. `open` and the
/// editors' command-line tools report an unknown handler or a bad path within this time; an
/// editor that keeps running is left alone.
const GRACE: Duration = Duration::from_secs(2);

/// The last of a program's stderr that goes into an error message.
const STDERR_TAIL: usize = 400;

/// Launches real processes, detached from the daemon's stdin and stdout. It waits up to
/// [`GRACE`] for the program to exit: a non-zero status is an error carrying its stderr. A
/// thread reaps each program afterwards.
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
            .stderr(Stdio::piped())
            .spawn()?;
        let stderr = child.stderr.take();
        let (done, exited) = mpsc::channel();
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut stderr) = stderr {
                // A program that outlives the grace period keeps this open; the read ends
                // when it exits, and nobody is listening by then.
                let _ = stderr.read_to_end(&mut text);
            }
            let status = child.wait();
            let _ = done.send((status, text));
        });
        match exited.recv_timeout(GRACE) {
            Ok((Ok(status), _)) if status.success() => Ok(()),
            Ok((Ok(status), text)) => {
                let text = String::from_utf8_lossy(&text);
                let text = text.trim();
                let tail = text
                    .char_indices()
                    .rev()
                    .nth(STDERR_TAIL)
                    .map_or(text, |(i, _)| &text[i..]);
                Err(io::Error::other(if tail.is_empty() {
                    status.to_string()
                } else {
                    format!("{status}: {tail}")
                }))
            }
            Ok((Err(e), _)) => Err(e),
            Err(_) => Ok(()),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        ["/bin/sh", "-c", script].map(String::from).to_vec()
    }

    #[test]
    fn a_program_that_exits_zero_is_started() {
        assert!(ProcessSpawner.spawn(&sh("exit 0")).is_ok());
    }

    #[test]
    fn a_program_that_fails_at_once_reports_its_status_and_stderr() {
        let e = ProcessSpawner
            .spawn(&sh("echo 'no handler' >&2; exit 3"))
            .unwrap_err();
        let text = e.to_string();
        assert!(text.contains("exit status: 3"), "{text}");
        assert!(text.contains("no handler"), "{text}");
    }

    #[test]
    fn a_missing_program_is_an_error() {
        assert!(
            ProcessSpawner
                .spawn(&["/nonexistent/editor".to_string()])
                .is_err()
        );
    }

    #[test]
    fn a_program_still_running_after_the_grace_period_counts_as_started() {
        let started = std::time::Instant::now();
        assert!(ProcessSpawner.spawn(&sh("sleep 30")).is_ok());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
}
