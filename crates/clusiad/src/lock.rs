//! The daemon lock: one clusiad per home. It is an advisory `flock` the OS drops when the
//! process dies, so a crash never blocks the next start.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Held for the life of the daemon.
#[derive(Debug)]
pub struct DaemonLock {
    _file: File,
}

impl DaemonLock {
    /// `Ok(None)` when another live process holds the lock.
    pub(crate) fn acquire(path: &Path) -> io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_until_the_first_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clusiad.lock");
        let first = DaemonLock::acquire(&path).unwrap().expect("free");
        assert!(DaemonLock::acquire(&path).unwrap().is_none());
        drop(first);
        assert!(DaemonLock::acquire(&path).unwrap().is_some());
    }

    #[test]
    fn the_lock_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clusiad.lock");
        let _lock = DaemonLock::acquire(&path).unwrap().expect("free");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
