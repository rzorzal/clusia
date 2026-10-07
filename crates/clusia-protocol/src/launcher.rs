//! Starting clusiad on demand, so opening any Clúsia client brings the daemon up (spec §3.1).

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use clusia_core::Paths;
use clusia_core::paths::MAX_SOCKET_PATH;

use crate::client::{Client, ClientError};
use crate::message::{Command, Reply};

pub const READY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(100);
/// The exit status of a clusiad that found another one running (`StartError::AlreadyRunning`).
const ALREADY_RUNNING: i32 = 3;

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

/// Starts clusiad and waits until it accepts connections. Returns the pid of the daemon that
/// answered, or `None` when it cannot be told (a winner that did not answer `DaemonStatus`
/// in time, and our own child already gone).
pub async fn start_daemon(paths: &Paths, home: Option<&Path>) -> Result<Option<u32>, LaunchError> {
    start_daemon_with(paths, home, &daemon_binary()?, READY_TIMEOUT).await
}

// The daemon outlives this process by design, so the child is deliberately never waited on.
#[allow(clippy::zombie_processes)]
pub async fn start_daemon_with(
    paths: &Paths,
    home: Option<&Path>,
    bin: &Path,
    timeout: Duration,
) -> Result<Option<u32>, LaunchError> {
    check_socket_path(paths)?;
    let logs = paths.logs_dir();
    std::fs::create_dir_all(logs).map_err(io_error(format!("cannot create {}", logs.display())))?;
    // What the daemon prints before its own log is open (a refusal to start, a panic). The
    // running daemon writes `daemon.log` itself, so this file only keeps the latest start.
    let log = logs.join("daemon.start.log");
    let out = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
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
        if let Ok(mut client) = Client::connect(&paths.socket(), "launcher").await {
            // Whichever daemon won a race answers; it is not necessarily our child, so our
            // child's pid is only a fallback while that child is still running.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let status = daemon_pid(&mut client, remaining).await;
            let running = matches!(child.try_wait(), Ok(None));
            return Ok(pick_pid(status, running, child.id()));
        }
        if let Some(status) = child
            .try_wait()
            .map_err(io_error("cannot check on clusiad".into()))?
        {
            // Another client started the daemon a moment earlier and ours stepped aside:
            // keep waiting for the winner to accept connections.
            if status.code() != Some(ALREADY_RUNNING) {
                return Err(LaunchError::Exited { status, log });
            }
        } else if Instant::now() >= deadline {
            // It would otherwise start late and find itself unwanted, or linger half-started.
            let _ = child.kill();
            let _ = child.wait();
            return Err(LaunchError::Timeout { timeout, log });
        }
        if Instant::now() >= deadline {
            return Err(LaunchError::Timeout { timeout, log });
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The daemon's own answer wins; our child's pid is only a guess while that child is running.
fn pick_pid(status: Option<u32>, child_running: bool, child: u32) -> Option<u32> {
    status.or(child_running.then_some(child))
}

/// The pid in the daemon's `DaemonStatus` reply, if it comes within `limit`.
async fn daemon_pid(client: &mut Client, limit: Duration) -> Option<u32> {
    match tokio::time::timeout(limit, client.request(Command::DaemonStatus)).await {
        Ok(Ok(Reply::Status(status))) => Some(status.pid),
        _ => None,
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
                assert!(log.ends_with("daemon.start.log"));
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

    /// A daemon that answers the handshake and `DaemonStatus` with `pid`, from `delay` on.
    fn serve_pid(socket: PathBuf, pid: u32, delay: Duration) {
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let listener = tokio::net::UnixListener::bind(socket).unwrap();
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
                    while let Ok(Some(ClientMessage::Request { id, .. })) = r.next().await {
                        let status = serde_json::from_value(serde_json::json!({
                            "version": "t", "pid": pid, "uptime_secs": 1,
                            "clients": 1, "socket": "s"
                        }))
                        .unwrap();
                        let reply = ServerMessage::Response {
                            id,
                            result: crate::Outcome::Ok(Reply::Status(status)),
                        };
                        let _ = write_message(&mut w, &reply).await;
                    }
                });
            }
        });
    }

    #[tokio::test]
    async fn a_lost_race_returns_the_winners_pid_even_when_it_is_still_binding() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        // Our child steps aside at once; the winner only starts listening a moment later.
        let bin = script(dir.path(), "fake", "exit 3");
        serve_pid(paths.socket(), 4242, Duration::from_millis(400));
        let pid = start_daemon_with(&paths, None, &bin, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(pid, Some(4242));
    }

    #[tokio::test]
    async fn a_winner_that_never_answers_status_does_not_hang_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let bin = script(dir.path(), "fake", "exit 3");
        // Starts listening once our child has likely gone, accepts the handshake, then says
        // nothing: the pid lookup must give up by itself.
        let socket = paths.socket();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let listener = tokio::net::UnixListener::bind(socket).unwrap();
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
                    std::future::pending::<()>().await;
                });
            }
        });
        // Whether the child is still running at the deadline is up to the scheduler, so the
        // pid is not asserted here; `pick_pid` covers that choice.
        let started = tokio::time::timeout(
            Duration::from_secs(10),
            start_daemon_with(&paths, None, &bin, Duration::from_millis(500)),
        )
        .await;
        assert!(started.expect("the start gave up by itself").is_ok());
    }

    #[test]
    fn the_daemons_own_pid_beats_our_childs() {
        assert_eq!(pick_pid(Some(4242), true, 7), Some(4242));
        assert_eq!(pick_pid(Some(4242), false, 7), Some(4242));
    }

    #[test]
    fn our_childs_pid_is_only_a_guess_while_it_runs() {
        assert_eq!(pick_pid(None, true, 7), Some(7));
        assert_eq!(pick_pid(None, false, 7), None);
    }

    #[tokio::test]
    async fn a_lost_race_with_no_winner_is_a_timeout_not_an_exit() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let bin = script(dir.path(), "fake", "exit 3");
        let err = start_daemon_with(&paths, None, &bin, Duration::from_millis(400))
            .await
            .unwrap_err();
        assert!(matches!(err, LaunchError::Timeout { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_daemon_that_never_listens_is_killed_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let bin = script(
            dir.path(),
            "fake",
            r#"echo $$ > "$(dirname "$0")/pid"; exec sleep 30"#,
        );
        let err = start_daemon_with(&paths, None, &bin, Duration::from_secs(3))
            .await
            .unwrap_err();
        assert!(matches!(err, LaunchError::Timeout { .. }), "{err}");
        let pid = std::fs::read_to_string(dir.path().join("pid")).expect("the script ran");
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the child {pid} outlived the timeout");
    }

    #[tokio::test]
    async fn the_start_log_holds_only_the_latest_start_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        for word in ["first", "second"] {
            let bin = script(dir.path(), "fake", &format!("echo {word} >&2; exit 7"));
            let _ = start_daemon_with(&paths, None, &bin, Duration::from_secs(5)).await;
        }
        let log = paths.logs_dir().join("daemon.start.log");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "second\n");
        let mode = std::fs::metadata(&log).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
