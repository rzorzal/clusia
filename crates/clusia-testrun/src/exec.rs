//! The one place a program is started. Everything else asks through [`Exec`], so tests answer
//! with a fake and the real thing runs only here.

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Notify;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How long a program gets between `SIGTERM` and `SIGKILL`.
pub const KILL_GRACE: Duration = Duration::from_secs(2);
/// The most `output` keeps of one stream.
pub const OUTPUT_MAX: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    /// Start from an empty environment: the child gets exactly `env`.
    pub clear_env: bool,
    pub cwd: Option<PathBuf>,
}

impl CommandSpec {
    pub fn new(program: impl AsRef<Path>) -> Self {
        Self {
            program: program.as_ref().to_path_buf(),
            ..Self::default()
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Self {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    pub fn cwd(mut self, cwd: impl AsRef<Path>) -> Self {
        self.cwd = Some(cwd.as_ref().to_path_buf());
        self
    }

    pub fn clear_env(mut self) -> Self {
        self.clear_env = true;
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// `Exited(None)` is a program ended by a signal.
    Exited(Option<i32>),
    TimedOut,
    Stopped,
}

/// A request to stop that a waiter sees whether it asks before or after the request.
pub struct StopFlag {
    fired: AtomicBool,
    notify: Notify,
}

impl StopFlag {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            fired: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    pub fn fire(&self) {
        self.fired.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    pub async fn notified(&self) {
        // Register before looking at the flag, so a `fire` between the two is not lost.
        let waiter = self.notify.notified();
        tokio::pin!(waiter);
        waiter.as_mut().enable();
        if self.fired() {
            return;
        }
        waiter.await;
    }
}

pub trait Exec: Send + Sync {
    /// Whether `path` is a file a program can be started from.
    fn executable(&self, path: &Path) -> bool;

    /// Runs `cmd` to its end or to `limit`, keeping the start of each stream.
    fn output<'a>(
        &'a self,
        cmd: &'a CommandSpec,
        limit: Duration,
    ) -> BoxFut<'a, io::Result<Output>>;

    /// Runs `cmd` to its end, to `limit` or to `stop`, handing each piece of output to `chunk`
    /// in the order it arrived.
    fn stream<'a>(
        &'a self,
        cmd: &'a CommandSpec,
        limit: Duration,
        stop: &'a StopFlag,
        chunk: &'a mut (dyn FnMut(Stream, &[u8]) + Send),
    ) -> BoxFut<'a, io::Result<Ended>>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemExec;

fn signal_group(pid: Option<u32>, signal: i32) {
    if let Some(pid) = pid {
        // SAFETY: `killpg` only sends a signal. The group is the one the child leads
        // (`process_group(0)` at spawn), so no other process is addressed.
        unsafe {
            libc::killpg(pid as i32, signal);
        }
    }
}

/// `SIGTERM` to the group, `SIGKILL` once `KILL_GRACE` has passed, then reaps the leader.
async fn terminate(child: &mut Child, pid: Option<u32>) {
    signal_group(pid, libc::SIGTERM);
    if tokio::time::timeout(KILL_GRACE, child.wait())
        .await
        .is_err()
    {
        signal_group(pid, libc::SIGKILL);
        let _ = child.wait().await;
    }
    // A member of the group may have ignored the `SIGTERM` that its leader obeyed.
    signal_group(pid, libc::SIGKILL);
}

/// Runs `cmd` to its end, to `limit` or to `stop`, handing every chunk to `chunk`.
async fn drive(
    cmd: &CommandSpec,
    limit: Duration,
    stop: Option<&StopFlag>,
    chunk: &mut (dyn FnMut(Stream, &[u8]) + Send),
) -> io::Result<Ended> {
    let mut command = Command::new(&cmd.program);
    command.args(&cmd.args);
    if cmd.clear_env {
        command.env_clear();
    }
    command.envs(cmd.env.iter().map(|(k, v)| (k, v)));
    if let Some(cwd) = &cmd.cwd {
        command.current_dir(cwd);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn()?;
    let pid = child.id();
    let (mut out, mut err) = (
        child.stdout.take().expect("stdout is piped"),
        child.stderr.take().expect("stderr is piped"),
    );
    let (mut out_buf, mut err_buf) = ([0u8; 8192], [0u8; 8192]);
    let (mut out_open, mut err_open) = (true, true);
    let mut status = None;
    let deadline = tokio::time::sleep(limit);
    let stopped = async {
        match stop {
            Some(stop) => stop.notified().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline, stopped);
    loop {
        if !out_open
            && !err_open
            && let Some(status) = status
        {
            return Ok(Ended::Exited(std::process::ExitStatus::code(&status)));
        }
        tokio::select! {
            n = out.read(&mut out_buf), if out_open => match n {
                Ok(n) if n > 0 => chunk(Stream::Out, &out_buf[..n]),
                _ => out_open = false,
            },
            n = err.read(&mut err_buf), if err_open => match n {
                Ok(n) if n > 0 => chunk(Stream::Err, &err_buf[..n]),
                _ => err_open = false,
            },
            // The program exited: its group dies with it, which also closes pipes that a
            // background process would have held open.
            exit = child.wait(), if status.is_none() => {
                status = Some(exit?);
                signal_group(pid, libc::SIGKILL);
            }
            _ = &mut stopped => {
                terminate(&mut child, pid).await;
                return Ok(Ended::Stopped);
            }
            _ = &mut deadline => {
                terminate(&mut child, pid).await;
                return Ok(Ended::TimedOut);
            }
        }
    }
}

fn keep(buf: &mut Vec<u8>, bytes: &[u8]) {
    let room = OUTPUT_MAX.saturating_sub(buf.len());
    buf.extend_from_slice(&bytes[..bytes.len().min(room)]);
}

impl Exec for SystemExec {
    fn executable(&self, path: &Path) -> bool {
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    fn output<'a>(
        &'a self,
        cmd: &'a CommandSpec,
        limit: Duration,
    ) -> BoxFut<'a, io::Result<Output>> {
        Box::pin(async move {
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            let mut chunk = |stream: Stream, bytes: &[u8]| match stream {
                Stream::Out => keep(&mut stdout, bytes),
                Stream::Err => keep(&mut stderr, bytes),
            };
            let ended = drive(cmd, limit, None, &mut chunk).await?;
            Ok(Output {
                code: match ended {
                    Ended::Exited(code) => code,
                    _ => None,
                },
                stdout,
                stderr,
                timed_out: ended == Ended::TimedOut,
            })
        })
    }

    fn stream<'a>(
        &'a self,
        cmd: &'a CommandSpec,
        limit: Duration,
        stop: &'a StopFlag,
        chunk: &'a mut (dyn FnMut(Stream, &[u8]) + Send),
    ) -> BoxFut<'a, io::Result<Ended>> {
        Box::pin(drive(cmd, limit, Some(stop), chunk))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    use super::*;

    fn sh(script: &str) -> CommandSpec {
        CommandSpec::new("/bin/sh").arg("-c").arg(script)
    }

    #[tokio::test]
    async fn output_holds_both_streams_and_the_exit_code() {
        let out = SystemExec
            .output(
                &sh("echo out; echo err >&2; exit 3"),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
        assert_eq!(out.code, Some(3));
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn clear_env_starts_from_nothing_and_applies_the_pairs() {
        let cmd = CommandSpec::new("/usr/bin/env").env("A", "1").clear_env();
        let out = SystemExec
            .output(&cmd, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out.stdout, b"A=1\n");
        let inherited = CommandSpec::new("/bin/sh")
            .args(["-c", "echo $HOME"])
            .env("A", "1");
        let out = SystemExec
            .output(&inherited, Duration::from_secs(10))
            .await
            .unwrap();
        assert_ne!(
            out.stdout, b"\n",
            "without clear_env the daemon's HOME is inherited"
        );
    }

    #[tokio::test]
    async fn the_working_folder_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = sh("pwd -P").cwd(dir.path());
        let out = SystemExec
            .output(&cmd, Duration::from_secs(10))
            .await
            .unwrap();
        let shown = String::from_utf8(out.stdout).unwrap();
        assert_eq!(
            shown.trim(),
            dir.path().canonicalize().unwrap().to_str().unwrap()
        );
    }

    #[tokio::test]
    async fn a_run_past_its_limit_is_ended_with_its_group() {
        let started = Instant::now();
        let out = SystemExec
            .output(&sh("sleep 30 & wait"), Duration::from_millis(300))
            .await
            .unwrap();
        assert!(out.timed_out);
        assert_eq!(out.code, None);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn output_keeps_only_the_first_four_mebibytes_of_a_stream() {
        let out = SystemExec
            .output(&sh("head -c 5000000 /dev/zero"), Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(out.stdout.len(), OUTPUT_MAX);
        assert_eq!(out.code, Some(0));
    }

    #[tokio::test]
    async fn a_program_that_is_not_there_is_not_found() {
        let err = SystemExec
            .output(
                &CommandSpec::new("/nonexistent/docker"),
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn stream_hands_over_chunks_in_order_and_the_exit_code() {
        let stop = StopFlag::new();
        let mut seen: Vec<(Stream, String)> = Vec::new();
        let mut chunk = |stream: Stream, bytes: &[u8]| {
            seen.push((stream, String::from_utf8_lossy(bytes).into_owned()));
        };
        let ended = SystemExec
            .stream(
                &sh("echo one; echo two >&2; exit 4"),
                Duration::from_secs(10),
                &stop,
                &mut chunk,
            )
            .await
            .unwrap();
        assert_eq!(ended, Ended::Exited(Some(4)));
        let out: String = seen
            .iter()
            .filter(|(s, _)| *s == Stream::Out)
            .map(|(_, t)| t.as_str())
            .collect();
        let err: String = seen
            .iter()
            .filter(|(s, _)| *s == Stream::Err)
            .map(|(_, t)| t.as_str())
            .collect();
        assert_eq!((out.as_str(), err.as_str()), ("one\n", "two\n"));
    }

    #[tokio::test]
    async fn stop_ends_a_stream_and_its_group() {
        let stop = StopFlag::new();
        let flag = stop.clone();
        let mut chunk = move |_: Stream, bytes: &[u8]| {
            if bytes.starts_with(b"ready") {
                flag.fire();
            }
        };
        let started = Instant::now();
        let ended = SystemExec
            .stream(
                &sh("echo ready; sleep 30"),
                Duration::from_secs(60),
                &stop,
                &mut chunk,
            )
            .await
            .unwrap();
        assert_eq!(ended, Ended::Stopped);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_stop_fired_before_the_start_ends_it_at_once() {
        let stop = StopFlag::new();
        stop.fire();
        let mut chunk = |_: Stream, _: &[u8]| {};
        let ended = SystemExec
            .stream(&sh("sleep 30"), Duration::from_secs(60), &stop, &mut chunk)
            .await
            .unwrap();
        assert_eq!(ended, Ended::Stopped);
    }

    #[tokio::test]
    async fn stream_reports_the_limit() {
        let stop = StopFlag::new();
        let mut chunk = |_: Stream, _: &[u8]| {};
        let ended = SystemExec
            .stream(
                &sh("sleep 30"),
                Duration::from_millis(200),
                &stop,
                &mut chunk,
            )
            .await
            .unwrap();
        assert_eq!(ended, Ended::TimedOut);
    }

    #[tokio::test]
    async fn a_process_left_in_the_background_does_not_outlive_the_run() {
        let out = SystemExec
            .output(&sh("sleep 30 & echo $!"), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out.code, Some(0));
        let pid: i32 = String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone = async {
            loop {
                // SAFETY: signal 0 only checks that the process exists.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), gone)
            .await
            .expect("the background process was killed with its group");
    }

    #[test]
    fn executable_means_a_file_with_an_execute_bit() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        let plain = dir.path().join("plain");
        std::fs::write(&run, "#!/bin/sh\n").unwrap();
        std::fs::write(&plain, "x").unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(SystemExec.executable(&run));
        assert!(!SystemExec.executable(&plain));
        assert!(
            !SystemExec.executable(dir.path()),
            "a folder is not a program"
        );
        assert!(!SystemExec.executable(&dir.path().join("missing")));
    }

    #[tokio::test]
    async fn a_stop_flag_wakes_every_waiter_and_remembers() {
        let stop = StopFlag::new();
        assert!(!stop.fired());
        let waiter = {
            let stop = stop.clone();
            tokio::spawn(async move { stop.notified().await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.fire();
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(stop.fired());
        tokio::time::timeout(Duration::from_secs(1), stop.notified())
            .await
            .unwrap();
    }

    #[test]
    fn the_builders_fill_the_spec() {
        let cmd = CommandSpec::new("/usr/bin/git")
            .args(["ls-files", "-z"])
            .arg("--")
            .env("HOME", "/Users/octo")
            .cwd("/tmp/wt")
            .clear_env();
        assert_eq!(cmd.program, PathBuf::from("/usr/bin/git"));
        assert_eq!(cmd.args, ["ls-files", "-z", "--"].map(OsString::from));
        assert_eq!(
            cmd.env,
            vec![(OsString::from("HOME"), OsString::from("/Users/octo"))]
        );
        assert_eq!(cmd.cwd, Some(PathBuf::from("/tmp/wt")));
        assert!(cmd.clear_env);
    }
}
