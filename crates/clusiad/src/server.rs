//! Socket ownership: binding safely, accepting clients, shutting down.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clusia_core::Paths;
use clusia_core::paths::MAX_SOCKET_PATH;
use clusia_store::{Loaded, load_config};
use tokio::net::{UnixListener, UnixStream};

use crate::connection;
use crate::lock::DaemonLock;
use crate::options::DaemonOptions;
use crate::state::Shared;
use crate::sync;

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("socket path {} is {len} bytes; macOS allows at most {MAX_SOCKET_PATH}. Use a shorter --home", path.display())]
    PathTooLong { path: PathBuf, len: usize },
    #[error("another clusiad is already running on {}", .0.display())]
    AlreadyRunning(PathBuf),
    #[error("could not start: {0}")]
    Io(#[from] io::Error),
}

pub struct Daemon {
    /// Released when the daemon is dropped, after the socket is gone.
    _lock: DaemonLock,
    listener: UnixListener,
    shared: Arc<Shared>,
    socket: PathBuf,
}

/// Stops a running daemon from anywhere (signal handler, tests).
#[derive(Clone)]
pub struct ShutdownHandle(Arc<Shared>);

impl ShutdownHandle {
    pub fn trigger(&self) {
        self.0.trigger_shutdown();
    }
}

/// Binds under a umask that leaves nothing to group or others, so the socket is never
/// reachable by anyone else, not even for the moment before its mode is set.
fn bind_private(socket: &Path) -> io::Result<UnixListener> {
    // SAFETY: `umask` only changes the file-creation mask of this process and cannot fail.
    let previous = unsafe { libc::umask(0o077) };
    let bound = UnixListener::bind(socket);
    // SAFETY: as above; restores the mask read a moment ago.
    unsafe { libc::umask(previous) };
    bound
}

impl Daemon {
    /// Binds with options from the environment (`DaemonOptions::from_env`).
    pub async fn bind(paths: Paths) -> Result<Self, StartError> {
        Self::bind_with(paths, DaemonOptions::from_env()).await
    }

    pub async fn bind_with(paths: Paths, options: DaemonOptions) -> Result<Self, StartError> {
        let lock = Self::acquire_lock(&paths)?;
        Self::bind_locked(paths, options, lock).await
    }

    /// Takes the one-daemon-per-home lock. A caller that opens log files should take it
    /// first: a daemon that loses the lock must not rotate or prune the winner's logs.
    pub fn acquire_lock(paths: &Paths) -> Result<DaemonLock, StartError> {
        let socket = paths.socket();
        if !paths.socket_path_fits() {
            return Err(StartError::PathTooLong {
                len: socket.as_os_str().len(),
                path: socket,
            });
        }
        fs::create_dir_all(paths.root())?;
        DaemonLock::acquire(&paths.daemon_lock())?.ok_or(StartError::AlreadyRunning(socket))
    }

    /// Binds the socket once the lock from [`Daemon::acquire_lock`] is held.
    pub async fn bind_locked(
        paths: Paths,
        options: DaemonOptions,
        lock: DaemonLock,
    ) -> Result<Self, StartError> {
        let socket = paths.socket();
        let socket_exists = fs::symlink_metadata(&socket).is_ok();
        let mut stale = false;
        if socket_exists {
            match UnixStream::connect(&socket).await {
                Ok(_) => return Err(StartError::AlreadyRunning(socket)),
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => stale = true,
                Err(e) => {
                    return Err(StartError::Io(io::Error::new(
                        e.kind(),
                        format!("cannot probe existing socket {}: {e}", socket.display()),
                    )));
                }
            }
        }
        let loaded = load_config(&paths)?;
        let reset_config = match &loaded {
            Loaded::Recovered { quarantined, .. } => Some(quarantined.clone()),
            _ => None,
        };
        let config = loaded.into_value();
        if stale {
            fs::remove_file(&socket)?;
            tracing::info!(socket = %socket.display(), "removed stale socket");
        }
        let listener = bind_private(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;

        let shared = Arc::new(Shared::new(paths, config, options));
        if let Some(file) = reset_config {
            crate::notifications::note_recovered(&shared, &file);
        }
        Ok(Self {
            _lock: lock,
            listener,
            shared,
            socket,
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle(self.shared.clone())
    }

    pub async fn run(self) -> io::Result<()> {
        let mut shutdown = self.shared.shutdown.subscribe();
        tracing::info!(socket = %self.socket.display(), version = crate::VERSION, "clusiad listening");
        if self.shared.background_sync {
            tokio::spawn(sync::run_loop(self.shared.clone()));
        }
        tokio::spawn(crate::notifications::run_checks(self.shared.clone()));
        let tray = self.shared.tray_program.clone().map(|program| {
            tokio::spawn(crate::tray::supervise(
                program,
                self.shared.paths.root().to_path_buf(),
                self.shared.paths.logs_dir().join("tray.log"),
                self.shared.shutdown.subscribe(),
                crate::tray::RestartPolicy::DEFAULT,
            ))
        });
        {
            let shared = self.shared.clone();
            let periodic = self.shared.background_sync;
            tokio::spawn(async move {
                let mut shutdown = shared.shutdown.subscribe();
                loop {
                    let removed = crate::retention::sweep(&shared).await;
                    if removed > 0 {
                        tracing::info!(removed, "removed stale worktrees");
                    }
                    let media = shared.paths.media_dir();
                    let trimmed = tokio::task::spawn_blocking(move || {
                        crate::retention::sweep_media(
                            &media,
                            crate::retention::MEDIA_MAX_AGE,
                            crate::retention::MEDIA_MAX_BYTES,
                        )
                    })
                    .await
                    .unwrap_or(0);
                    if trimmed > 0 {
                        tracing::info!(removed = trimmed, "trimmed the media cache");
                    }
                    if !periodic {
                        return;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(crate::retention::SWEEP_EVERY) => {}
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                return;
                            }
                        }
                    }
                }
            });
        }
        loop {
            if *shutdown.borrow_and_update() {
                break;
            }
            tokio::select! {
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        tokio::spawn(connection::serve(stream, self.shared.clone()));
                    }
                    Err(e) => tracing::warn!(error = %e, "accept failed"),
                },
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
        }
        drop(self.listener);
        if let Some(tray) = tray {
            // The supervisor kills the tray on shutdown; give it a moment to reap it.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), tray).await;
        }
        match fs::remove_file(&self.socket) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                tracing::warn!(error = %e, "could not remove socket")
            }
            _ => {}
        }
        tracing::info!("clusiad stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[tokio::test]
    async fn the_socket_is_private_from_the_moment_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        let _listener = bind_private(&socket).unwrap();
        assert_eq!(fs::metadata(&socket).unwrap().mode() & 0o077, 0);
    }
}
