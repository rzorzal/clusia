//! Starting clusiad on demand, so opening any Clúsia client brings the daemon up (spec §3.1).

use std::fs::OpenOptions;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use clusia_core::Paths;
use clusia_core::paths::MAX_SOCKET_PATH;

use crate::client::{Client, ClientError};

pub const READY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("cannot start {}: {source}", bin.display())]
    Spawn { bin: PathBuf, source: io::Error },
    // Same wording as clusiad's `StartError::PathTooLong`.
    #[error("socket path {} is {len} bytes; macOS allows at most {MAX_SOCKET_PATH}. Use a shorter --home", path.display())]
    PathTooLong { path: PathBuf, len: usize },
    #[error("{context}: {source}")]
    Io { context: String, source: io::Error },
    #[error("clusiad exited with {status}; see {}", log.display())]
    Exited { status: ExitStatus, log: PathBuf },
    #[error("clusiad did not start within {}s; see {}", timeout.as_secs(), log.display())]
    Timeout { timeout: Duration, log: PathBuf },
}

fn io_error(context: String) -> impl FnOnce(io::Error) -> LaunchError {
    move |source| LaunchError::Io { context, source }
}

/// `CLUSIA_DAEMON_BIN`, else `clusiad` next to the running executable (true inside Clusia.app and `target/`).
pub fn daemon_binary() -> Result<PathBuf, LaunchError> {
    if let Some(p) = std::env::var_os("CLUSIA_DAEMON_BIN") {
        return Ok(PathBuf::from(p));
    }
    let exe =
        std::env::current_exe().map_err(io_error("cannot locate the running executable".into()))?;
    Ok(exe.with_file_name("clusiad"))
}

/// Fails when the socket path does not fit in a Unix socket address (macOS: 103 bytes).
pub fn check_socket_path(paths: &Paths) -> Result<(), LaunchError> {
    if paths.socket_path_fits() {
        return Ok(());
    }
    let path = paths.socket();
    Err(LaunchError::PathTooLong {
        len: path.as_os_str().len(),
        path,
    })
}

/// Starts clusiad and waits until it accepts connections. Returns its pid.
pub async fn start_daemon(paths: &Paths, home: Option<&Path>) -> Result<u32, LaunchError> {
    start_daemon_with(paths, home, &daemon_binary()?, READY_TIMEOUT).await
}

// The daemon outlives this process by design, so the child is deliberately never waited on.
#[allow(clippy::zombie_processes)]
pub async fn start_daemon_with(
    paths: &Paths,
    home: Option<&Path>,
    bin: &Path,
    timeout: Duration,
) -> Result<u32, LaunchError> {
    check_socket_path(paths)?;
    let logs = paths.logs_dir();
    std::fs::create_dir_all(logs).map_err(io_error(format!("cannot create {}", logs.display())))?;
    let log = logs.join("daemon.log");
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(io_error(format!("cannot open {}", log.display())))?;
    let err = out
        .try_clone()
        .map_err(io_error(format!("cannot open {}", log.display())))?;

    // The child runs in `/`, so a relative path like `target/debug/clusiad` must be resolved here.
    let program = if bin.is_relative() && bin.components().count() > 1 {
        std::path::absolute(bin).unwrap_or_else(|_| bin.to_path_buf())
    } else {
        bin.to_path_buf()
    };
    let mut cmd = std::process::Command::new(&program);
    if let Some(home) = home {
        cmd.arg("--home").arg(home);
    }
    cmd.stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .current_dir("/");
    // A new session (and so a new process group) without a controlling terminal: the daemon
    // never receives the terminal's SIGHUP/SIGINT and can never block reading from it.
    // SAFETY: `setsid` is async-signal-safe and touches no memory of the parent.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|source| LaunchError::Spawn {
        bin: bin.to_path_buf(),
        source,
    })?;

    let deadline = Instant::now() + timeout;
    loop {
        if Client::connect(&paths.socket(), "launcher").await.is_ok() {
            return Ok(child.id());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(io_error("cannot check on clusiad".into()))?
        {
            // Another client may have started the daemon a moment earlier; ours then exits with 3.
            if Client::connect(&paths.socket(), "launcher").await.is_ok() {
                return Ok(child.id());
            }
            return Err(LaunchError::Exited { status, log });
        }
        if Instant::now() >= deadline {
            return Err(LaunchError::Timeout { timeout, log });
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Connects to clusiad, starting it first when it is not running. `true` means it was started now.
pub async fn ensure_daemon(
    paths: &Paths,
    home: Option<&Path>,
    client_name: &str,
) -> Result<(Client, bool), LaunchError> {
    check_socket_path(paths)?;
    match Client::connect(&paths.socket(), client_name).await {
        Ok(client) => Ok((client, false)),
        Err(ClientError::NotRunning(_)) => {
            start_daemon(paths, home).await?;
            Ok((Client::connect(&paths.socket(), client_name).await?, true))
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMessage, MessageReader, PROTOCOL_VERSION, ServerMessage, write_message};
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[tokio::test]
    async fn missing_binary_is_a_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let err = start_daemon_with(
            &paths,
            None,
            Path::new("/nonexistent/clusiad"),
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LaunchError::Spawn { .. }), "{err}");
    }

    #[tokio::test]
    async fn early_exit_is_reported_with_the_log_path() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let bin = script(dir.path(), "fake", "echo boom >&2; exit 7");
        match start_daemon_with(&paths, None, &bin, Duration::from_secs(5)).await {
            Err(LaunchError::Exited { status, log }) => {
                assert_eq!(status.code(), Some(7));
                assert!(log.ends_with("daemon.log"));
                assert!(std::fs::read_to_string(&log).unwrap().contains("boom"));
            }
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn silent_binary_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let bin = script(dir.path(), "fake", "sleep 3");
        let err = start_daemon_with(&paths, None, &bin, Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(matches!(err, LaunchError::Timeout { .. }), "{err}");
    }

    #[tokio::test]
    async fn home_is_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let bin = script(dir.path(), "fake", r#"echo "$@" > "$(dirname "$0")/args""#);
        let _ = start_daemon_with(
            &paths,
            Some(Path::new("/x/home")),
            &bin,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("args"))
                .unwrap()
                .trim(),
            "--home /x/home"
        );
    }

    #[tokio::test]
    async fn daemon_has_no_controlling_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let bin = script(
            dir.path(),
            "fake",
            r#"d="$(dirname "$0")"; ps -o tty= -p $$ > "$d/tty.tmp"; pwd > "$d/cwd.tmp"; mv "$d/tty.tmp" "$d/tty"; mv "$d/cwd.tmp" "$d/cwd""#,
        );
        let _ = start_daemon_with(&paths, None, &bin, Duration::from_secs(5)).await;
        let tty = std::fs::read_to_string(dir.path().join("tty")).unwrap();
        assert!(
            matches!(tty.trim(), "?" | "??"),
            "the daemon has a controlling terminal: {tty:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cwd"))
                .unwrap()
                .trim(),
            "/"
        );
    }

    #[tokio::test]
    async fn too_long_socket_path_is_refused_before_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("x".repeat(120)));
        let bin = script(dir.path(), "fake", r#"touch "$(dirname "$0")/spawned""#);
        let err = start_daemon_with(&paths, None, &bin, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("at most 103"), "{err}");
        match ensure_daemon(&paths, None, "t").await {
            Err(err) => assert!(err.to_string().contains("at most 103"), "{err}"),
            Ok(_) => panic!("ensure_daemon accepted a too-long socket path"),
        }
        assert!(!dir.path().join("spawned").exists());
    }

    #[tokio::test]
    async fn ensure_daemon_reuses_a_running_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut r = MessageReader::new(r);
                    let _hello: Option<ClientMessage> = r.next().await.unwrap_or(None);
                    let welcome = ServerMessage::Welcome {
                        protocol: PROTOCOL_VERSION,
                        daemon: "t".into(),
                    };
                    let _ = write_message(&mut w, &welcome).await;
                    let _ = r.next::<ClientMessage>().await;
                });
            }
        });
        let (_client, started) = ensure_daemon(&paths, None, "t").await.unwrap();
        assert!(!started);
    }
}
