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

/// The memory and CPU time of one process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcUsage {
    pub pid: u32,
    pub rss_bytes: u64,
    pub cpu_ns: u64,
}

/// The processes of one process group at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupUsage {
    pub procs: Vec<ProcUsage>,
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

    /// `stream`, also handing `started` the pid of the program right after it starts. The pid
    /// is its process group's id. The default never calls `started`.
    fn stream_group<'a>(
        &'a self,
        cmd: &'a CommandSpec,
        limit: Duration,
        stop: &'a StopFlag,
        chunk: &'a mut (dyn FnMut(Stream, &[u8]) + Send),
        started: &'a mut (dyn FnMut(u32) + Send),
    ) -> BoxFut<'a, io::Result<Ended>> {
        let _ = started;
        self.stream(cmd, limit, stop, chunk)
    }

    /// The memory and CPU time of every process in `group`, or `None` when it can't be read.
    fn group_usage(&self, group: u32) -> Option<GroupUsage> {
        let _ = group;
        None
    }
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
    started: Option<&mut (dyn FnMut(u32) + Send)>,
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
    let mut child = command.spawn().map_err(|e| {
        let program = cmd.program.display();
        let text = match &cmd.cwd {
            Some(cwd) => format!("{program} can't start in {}: {e}", cwd.display()),
            None => format!("{program} can't start: {e}"),
        };
        io::Error::new(e.kind(), text)
    })?;
    let pid = child.id();
    if let (Some(started), Some(pid)) = (started, pid) {
        started(pid);
    }
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
    // A descendant that left the group can hold a pipe open past its leader's exit: after
    // the exit the pipes get `KILL_GRACE` to drain and no longer.
    let drain = tokio::time::sleep(limit);
    tokio::pin!(deadline, stopped, drain);
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
                drain.as_mut().reset(tokio::time::Instant::now() + KILL_GRACE);
                signal_group(pid, libc::SIGKILL);
            }
            _ = &mut drain, if status.is_some() => {
                return Ok(Ended::Exited(status.and_then(|s| s.code())));
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
            let ended = drive(cmd, limit, None, &mut chunk, None).await?;
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
        Box::pin(drive(cmd, limit, Some(stop), chunk, None))
    }

    fn stream_group<'a>(
        &'a self,
        cmd: &'a CommandSpec,
        limit: Duration,
        stop: &'a StopFlag,
        chunk: &'a mut (dyn FnMut(Stream, &[u8]) + Send),
        started: &'a mut (dyn FnMut(u32) + Send),
    ) -> BoxFut<'a, io::Result<Ended>> {
        Box::pin(drive(cmd, limit, Some(stop), chunk, Some(started)))
    }

    fn group_usage(&self, group: u32) -> Option<GroupUsage> {
        read_group(group)
    }
}

/// libc points to the `mach2` crate for these, which the workspace does not take.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn timebase() -> (u32, u32) {
    static TIMEBASE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    *TIMEBASE.get_or_init(|| {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: the call only writes the two fields of `info`.
        let status = unsafe { libc::mach_timebase_info(&mut info) };
        if status == 0 && info.denom != 0 {
            (info.numer, info.denom)
        } else {
            (1, 1)
        }
    })
}

/// The processes of `group` through libproc: resident memory and user plus system CPU time.
#[cfg(target_os = "macos")]
fn read_group(group: u32) -> Option<GroupUsage> {
    const PIDS_MAX: usize = 1024;
    let group = libc::pid_t::try_from(group).ok().filter(|g| *g > 0)?;
    let mut pids: Vec<libc::pid_t> = vec![0; PIDS_MAX];
    let bytes = libc::c_int::try_from(PIDS_MAX * std::mem::size_of::<libc::pid_t>()).ok()?;
    // SAFETY: `pids` holds `bytes` bytes and the call writes at most that many.
    let n = unsafe { libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), bytes) };
    let n = usize::try_from(n).ok().filter(|n| *n > 0)?.min(PIDS_MAX);
    let (numer, denom) = timebase();
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_taskinfo>()).ok()?;
    let mut procs = Vec::new();
    for &pid in &pids[..n] {
        let Ok(id) = u32::try_from(pid) else { continue };
        if id == 0 {
            continue;
        }
        // SAFETY: an all-zero `proc_taskinfo` is a valid value of a plain C struct.
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is `size` bytes long and the call writes at most that many.
        let got = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size)
        };
        if got != size {
            continue;
        }
        let ticks = u128::from(info.pti_total_user) + u128::from(info.pti_total_system);
        let cpu_ns = ticks * u128::from(numer) / u128::from(denom);
        procs.push(ProcUsage {
            pid: id,
            rss_bytes: info.pti_resident_size,
            cpu_ns: u64::try_from(cpu_ns).unwrap_or(u64::MAX),
        });
    }
    (!procs.is_empty()).then_some(GroupUsage { procs })
}

#[cfg(not(target_os = "macos"))]
fn read_group(_group: u32) -> Option<GroupUsage> {
    None
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
    async fn a_spawn_error_names_the_program_and_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let err = SystemExec
            .output(
                &CommandSpec::new("/nonexistent/acme-tool").cwd(dir.path()),
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let text = err.to_string();
        assert!(
            text.starts_with("/nonexistent/acme-tool can't start in ")
                && text.contains(&dir.path().display().to_string()),
            "{text}"
        );
        let err = SystemExec
            .output(
                &CommandSpec::new("/nonexistent/acme-tool"),
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("/nonexistent/acme-tool can't start: "),
            "{err}"
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
    async fn a_descendant_that_left_the_group_does_not_turn_an_exit_into_a_timeout() {
        // The child makes its own session, so the exit-time kill of the group misses it, and it
        // keeps the pipes open.
        let script = "use POSIX; my $p = fork; if ($p == 0) { POSIX::setsid(); exec 'sleep', '30' } print $p; exit 0";
        let cmd = CommandSpec::new("/usr/bin/perl").args(["-MPOSIX", "-e", script]);
        let started = Instant::now();
        let out = SystemExec
            .output(&cmd, Duration::from_secs(60))
            .await
            .unwrap();
        let pid: i32 = String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: the pid is the stray `sleep` this test started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        assert_eq!(out.code, Some(0));
        assert!(!out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(10));
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

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sampling_a_sleep_reads_its_group() {
        let stop = StopFlag::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut started = move |pid: u32| {
            let _ = tx.send(pid);
        };
        let mut chunk = |_: Stream, _: &[u8]| {};
        let cmd = CommandSpec::new("/bin/sleep").arg("5");
        let run = SystemExec.stream_group(
            &cmd,
            Duration::from_secs(20),
            &stop,
            &mut chunk,
            &mut started,
        );
        let read = async {
            let pid = loop {
                if let Ok(pid) = rx.try_recv() {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            tokio::time::sleep(Duration::from_millis(300)).await;
            let usage = SystemExec.group_usage(pid);
            stop.fire();
            (pid, usage)
        };
        let (ended, (pid, usage)) = tokio::join!(run, read);
        assert_eq!(ended.unwrap(), Ended::Stopped);
        let usage = usage.expect("the group of a running sleep");
        let leader = usage
            .procs
            .iter()
            .find(|p| p.pid == pid)
            .expect("the leader is listed");
        assert!(leader.rss_bytes > 0, "{leader:?}");
        assert_eq!(
            SystemExec.group_usage(pid),
            None,
            "an ended group reads as nothing"
        );
        assert_eq!(SystemExec.group_usage(0), None);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn group_usage_is_not_read_off_macos() {
        assert_eq!(SystemExec.group_usage(std::process::id()), None);
    }
}
