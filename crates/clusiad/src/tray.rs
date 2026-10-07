//! The menu bar tray is a separate process the daemon spawns and supervises (spec §3.1).
//! The tray exits by itself when the socket closes; this keeps it running while the daemon is up.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};
use tokio::sync::watch;

#[derive(Debug, Clone, Copy)]
pub struct RestartPolicy {
    /// Restarts allowed inside `window` after the first start.
    pub max_restarts: usize,
    pub window: Duration,
    pub delay: Duration,
}

impl RestartPolicy {
    pub const DEFAULT: Self = Self {
        max_restarts: 3,
        window: Duration::from_secs(600),
        delay: Duration::from_secs(2),
    };
}

/// Runs `program --home <home>` until the daemon shuts down. A clean exit (status 0) is final;
/// a crash is restarted after `policy.delay`, at most `policy.max_restarts` times per window.
pub async fn supervise(
    program: PathBuf,
    home: PathBuf,
    log: PathBuf,
    mut shutdown: watch::Receiver<bool>,
    policy: RestartPolicy,
) {
    let mut starts: VecDeque<Instant> = VecDeque::new();
    loop {
        let now = Instant::now();
        while starts
            .front()
            .is_some_and(|t| now.duration_since(*t) > policy.window)
        {
            starts.pop_front();
        }
        if starts.len() > policy.max_restarts {
            tracing::warn!(program = %program.display(), "the tray keeps crashing; not restarting it again");
            return;
        }
        starts.push_back(now);
        let mut child = match spawn(&program, &home, &log) {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!(program = %program.display(), error = %e, "cannot start the tray");
                return;
            }
        };
        tokio::select! {
            status = child.wait() => match status {
                Ok(s) if s.success() => {
                    tracing::info!("tray exited");
                    return;
                }
                Ok(s) => tracing::warn!(status = %s, "tray exited unexpectedly; restarting"),
                Err(e) => {
                    tracing::warn!(error = %e, "lost track of the tray");
                    return;
                }
            },
            () = shutting_down(&mut shutdown) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return;
            }
        }
        tokio::select! {
            () = tokio::time::sleep(policy.delay) => {}
            () = shutting_down(&mut shutdown) => return,
        }
    }
}

fn spawn(program: &Path, home: &Path, log: &Path) -> std::io::Result<Child> {
    if let (Some(dir), Some(name)) = (log.parent(), log.file_stem().and_then(|n| n.to_str())) {
        clusia_core::logging::prepare(dir, name, clusia_core::logging::KEEP_FILES)?;
    }
    let out = OpenOptions::new().create(true).append(true).open(log)?;
    let err = out.try_clone()?;
    Command::new(program)
        .arg("--home")
        .arg(home)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .kill_on_drop(true)
        .spawn()
}

/// Resolves once shutdown is requested; never resolves if the sender is gone.
async fn shutting_down(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const FAST: RestartPolicy = RestartPolicy {
        max_restarts: 3,
        window: Duration::from_secs(60),
        delay: Duration::from_millis(10),
    };

    fn script(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("fake-tray");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn lines(p: &Path) -> Vec<String> {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn crashing_tray_is_restarted_three_times_then_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let starts = dir.path().join("starts");
        let prog = script(
            dir.path(),
            &format!("echo \"$@\" >> '{}'; exit 1", starts.display()),
        );
        let (_tx, rx) = watch::channel(false);
        let home = dir.path().join("home");
        tokio::time::timeout(
            Duration::from_secs(10),
            supervise(prog, home.clone(), dir.path().join("tray.log"), rx, FAST),
        )
        .await
        .expect("gives up");
        let got = lines(&starts);
        assert_eq!(got.len(), 4, "first start + 3 restarts: {got:?}");
        assert!(
            got.iter()
                .all(|l| *l == format!("--home {}", home.display()))
        );
    }

    #[tokio::test]
    async fn clean_exit_is_not_restarted() {
        let dir = tempfile::tempdir().unwrap();
        let starts = dir.path().join("starts");
        let prog = script(
            dir.path(),
            &format!("echo x >> '{}'; exit 0", starts.display()),
        );
        let (_tx, rx) = watch::channel(false);
        tokio::time::timeout(
            Duration::from_secs(5),
            supervise(
                prog,
                dir.path().into(),
                dir.path().join("tray.log"),
                rx,
                FAST,
            ),
        )
        .await
        .unwrap();
        assert_eq!(lines(&starts).len(), 1);
    }

    #[tokio::test]
    async fn shutdown_stops_the_tray() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let prog = script(
            dir.path(),
            &format!("echo $$ > '{}'; exec sleep 30", pid_file.display()),
        );
        let (tx, rx) = watch::channel(false);
        let task = tokio::spawn(supervise(
            prog,
            dir.path().into(),
            dir.path().join("tray.log"),
            rx,
            FAST,
        ));
        let pid = loop {
            if let Some(p) = lines(&pid_file).into_iter().next() {
                break p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(alive(&pid));
        tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("supervisor stops")
            .unwrap();
        assert!(!alive(&pid), "tray {pid} still running");
    }

    #[tokio::test]
    async fn missing_binary_gives_up_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(false);
        tokio::time::timeout(
            Duration::from_secs(5),
            supervise(
                PathBuf::from("/nonexistent/clusia-tray"),
                dir.path().into(),
                dir.path().join("tray.log"),
                rx,
                FAST,
            ),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn output_goes_to_the_tray_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("tray.log");
        let prog = script(dir.path(), "echo hello-from-tray >&2; exit 0");
        let (_tx, rx) = watch::channel(false);
        supervise(prog, dir.path().into(), log.clone(), rx, FAST).await;
        assert!(
            std::fs::read_to_string(&log)
                .unwrap()
                .contains("hello-from-tray")
        );
        let mode = std::fs::metadata(&log).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "only the owner reads the log");
    }
}
