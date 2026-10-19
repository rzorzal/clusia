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

/// How long to wait for the stderr text once the program has exited. A grandchild (the editor
/// a command-line tool started) can hold the pipe open long after that.
const STDERR_WAIT: Duration = Duration::from_millis(200);

/// The most of a program's stderr that is kept; the rest is read and thrown away.
const STDERR_KEEP: u64 = 64 * 1024;

/// The last of a program's stderr that goes into an error message, in characters.
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
        let (chunks, text) = mpsc::channel::<Vec<u8>>();
        if let Some(mut stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut kept = (&mut stderr).take(STDERR_KEEP);
                let mut buf = [0u8; 4096];
                while let Ok(n) = kept.read(&mut buf) {
                    if n == 0 || chunks.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                drop(chunks);
                // Keep reading so a program that writes a lot never blocks on a full pipe.
                let _ = io::copy(&mut stderr, &mut io::sink());
            });
        }
        let (done, exited) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(child.wait());
        });
        match exited.recv_timeout(GRACE) {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => {
                let mut bytes = Vec::new();
                while let Ok(chunk) = text.recv_timeout(STDERR_WAIT) {
                    bytes.extend(chunk);
                }
                let message = printable(&String::from_utf8_lossy(&bytes));
                Err(io::Error::other(if message.is_empty() {
                    status.to_string()
                } else {
                    format!("{status}: {message}")
                }))
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(()),
        }
    }
}

/// The last [`STDERR_TAIL`] characters of `text` on one line: whitespace runs become a space
/// and other control characters (ANSI escapes among them) are written out as `\u{..}`.
fn printable(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let escaped: Vec<char> = flat
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_unicode().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect();
    escaped[escaped.len().saturating_sub(STDERR_TAIL)..]
        .iter()
        .collect()
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
        assert!(ProcessSpawner.spawn(&sh("exec sleep 10")).is_ok());
        let took = started.elapsed();
        assert!(took >= GRACE, "{took:?}");
        assert!(took < Duration::from_secs(8), "{took:?}");
    }

    #[test]
    fn control_characters_in_stderr_are_escaped() {
        let e = ProcessSpawner
            .spawn(&sh("printf '\\033[31mx\\r\\n' >&2; exit 1"))
            .unwrap_err();
        let text = e.to_string();
        assert!(!text.chars().any(char::is_control), "{text:?}");
        assert!(text.contains("\\u{1b}[31mx"), "{text:?}");
    }

    #[test]
    fn the_message_keeps_the_last_400_characters_of_stderr() {
        let e = ProcessSpawner
            .spawn(&sh("head -c 1000 /dev/zero | tr '\\0' x >&2; exit 1"))
            .unwrap_err();
        let text = e.to_string();
        let shown = text.strip_prefix("exit status: 1: ").unwrap();
        assert_eq!(shown.chars().count(), STDERR_TAIL);
        assert!(shown.chars().all(|c| c == 'x'));
    }

    #[test]
    fn a_program_that_floods_stderr_is_still_read_to_its_end() {
        let e = ProcessSpawner
            .spawn(&sh("head -c 300000 /dev/zero | tr '\\0' y >&2; exit 1"))
            .unwrap_err();
        assert!(e.to_string().contains("yyyy"));
    }

    #[test]
    fn a_grandchild_holding_stderr_does_not_delay_a_success() {
        let started = std::time::Instant::now();
        assert!(ProcessSpawner.spawn(&sh("(sleep 10 &); exit 0")).is_ok());
        assert!(
            started.elapsed() < GRACE,
            "the exit was not awaited on its own"
        );
    }

    #[test]
    fn a_grandchild_holding_stderr_does_not_hide_a_failure() {
        let started = std::time::Instant::now();
        let e = ProcessSpawner
            .spawn(&sh("(sleep 10 &); echo bad >&2; exit 2"))
            .unwrap_err();
        assert!(e.to_string().contains("exit status: 2: bad"), "{e}");
        assert!(
            started.elapsed() < GRACE,
            "the exit was not awaited on its own"
        );
    }
}
