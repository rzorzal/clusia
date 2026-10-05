//! One window per Clúsia home. The first `clusia-app` holds an advisory `flock` on
//! `<home>/app.lock` (the OS drops it when the process dies, so a crash never blocks the next
//! launch). Later launches hand their target to it through the daemon and exit.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::Duration;

use clusia_protocol::{Client, ClientError, Command, Reply, WindowTarget};

/// Held for the life of the window process.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// `Ok(None)` when another live process holds the lock.
    pub fn acquire(path: &Path) -> io::Result<Option<Self>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        // SAFETY: `file` owns an open descriptor for the whole call; flock does not retain it.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(Self { _file: file }));
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(err)
        }
    }
}

/// Asks the open window to show `target`, retrying while none is listening yet (it may still be
/// starting). Returns how many windows heard it (0 after `attempts` tries).
pub async fn hand_over(
    client: &mut Client,
    target: &WindowTarget,
    attempts: u32,
    pause: Duration,
) -> Result<usize, ClientError> {
    for attempt in 0..attempts.max(1) {
        if attempt > 0 {
            tokio::time::sleep(pause).await;
        }
        match client
            .request(Command::OpenWindow {
                target: target.clone(),
            })
            .await?
        {
            Reply::Delivered(0) => {}
            Reply::Delivered(n) => return Ok(n),
            other => return Err(ClientError::Unexpected(format!("{other:?}"))),
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_while_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.lock");
        let first = InstanceLock::acquire(&path).unwrap();
        assert!(first.is_some());
        assert!(InstanceLock::acquire(&path).unwrap().is_none());
        drop(first);
        assert!(
            InstanceLock::acquire(&path).unwrap().is_some(),
            "released on drop"
        );
    }

    #[test]
    fn creates_missing_parents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/app.lock");
        assert!(InstanceLock::acquire(&path).unwrap().is_some());
    }
}
