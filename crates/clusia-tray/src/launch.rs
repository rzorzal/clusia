//! Why this process started, and what that means: the tray is the main executable of
//! `Clusia.app`, so the user opening the app, the daemon starting its tray and a notification
//! click all land here.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clusia_core::Paths;

use crate::actions::{self, Action};

/// Set by the daemon on the tray it spawns (`daemon`). A relaunch by a notification click
/// carries no marker, so it looks like the user opening the app; `click` exists for tools.
pub const LAUNCHED_BY: &str = "CLUSIA_LAUNCHED_BY";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchReason {
    /// Opening Clusia.app (Finder, Spotlight, `open`, login item).
    User,
    /// The daemon spawned this tray.
    Daemon,
    /// A notification click started the process.
    NotificationClick,
}

/// `env` is the value of [`LAUNCHED_BY`]; `args` are the arguments after the program name.
pub fn reason_from(env: Option<&str>, args: &[String]) -> LaunchReason {
    match env {
        Some("daemon") => LaunchReason::Daemon,
        Some("click") => LaunchReason::NotificationClick,
        _ if args.iter().any(|a| a == "--notification-click") => LaunchReason::NotificationClick,
        _ => LaunchReason::User,
    }
}

/// The operating-system side of bringing Clúsia up.
pub trait Host {
    fn daemon_running(&self) -> bool;
    fn start_daemon(&self) -> Result<(), String>;
    /// Opens (or brings forward) the window at Home.
    fn open_window(&self) -> Result<(), String>;
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct BringUp {
    pub daemon_started: bool,
    pub window_opened: bool,
    pub error: Option<String>,
}

/// Makes sure the daemon is up and, when the user opened the app, that the window is open.
/// Safe to repeat: a running daemon is left alone and the window handles its own single instance.
pub fn bring_up(reason: LaunchReason, host: &dyn Host) -> BringUp {
    let mut out = BringUp::default();
    if reason == LaunchReason::Daemon {
        return out;
    }
    if !host.daemon_running() {
        match host.start_daemon() {
            Ok(()) => out.daemon_started = true,
            Err(e) => {
                out.error = Some(e);
                return out;
            }
        }
    }
    if reason == LaunchReason::User {
        match host.open_window() {
            Ok(()) => out.window_opened = true,
            Err(e) => out.error = Some(e),
        }
    }
    out
}

/// One tray per Clúsia home: an advisory `flock` the OS drops when the process dies.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// `Ok(None)` while another live process holds the lock.
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

    /// Like [`acquire`](Self::acquire), but waits for a tray that is on its way out (its daemon
    /// just died) before giving up.
    pub fn acquire_waiting(
        path: &Path,
        attempts: u32,
        pause: Duration,
    ) -> io::Result<Option<Self>> {
        for attempt in 0..attempts.max(1) {
            if attempt > 0 {
                std::thread::sleep(pause);
            }
            if let Some(lock) = Self::acquire(path)? {
                return Ok(Some(lock));
            }
        }
        Ok(None)
    }
}

/// Real processes and sockets.
pub struct SystemHost {
    pub paths: Paths,
    pub app_bin: Option<PathBuf>,
    /// The `--home` this tray was given. A daemon it starts gets the same one, so it
    /// serves the home the tray reads; `None` when the paths came from the environment.
    pub home: Option<PathBuf>,
    /// The daemon program; `None` is the `clusiad` next to this executable.
    pub daemon_bin: Option<PathBuf>,
    /// How long to wait for a started daemon to accept connections.
    pub daemon_wait: Duration,
}

impl SystemHost {
    pub fn new(paths: Paths, app_bin: Option<PathBuf>, home: Option<PathBuf>) -> Self {
        Self {
            paths,
            app_bin,
            home,
            daemon_bin: None,
            daemon_wait: clusia_protocol::launcher::READY_TIMEOUT,
        }
    }
}

impl Host for SystemHost {
    fn daemon_running(&self) -> bool {
        UnixStream::connect(self.paths.socket()).is_ok()
    }

    fn start_daemon(&self) -> Result<(), String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let bin = match &self.daemon_bin {
            Some(bin) => bin.clone(),
            None => clusia_protocol::launcher::daemon_binary().map_err(|e| e.to_string())?,
        };
        rt.block_on(clusia_protocol::launcher::start_daemon_with(
            &self.paths,
            self.home.as_deref(),
            &bin,
            self.daemon_wait,
        ))
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    fn open_window(&self) -> Result<(), String> {
        let plan = actions::plan(&Action::OpenHome, self.app_bin.as_deref(), &self.paths)
            .ok_or_else(|| "the window is not installed".to_string())?;
        actions::launch(&plan);
        Ok(())
    }
}

/// A tray the daemon started has its output redirected to `tray.log` by the daemon; one started
/// from a terminal keeps printing there; any other logs to the file itself.
pub fn logs_to_file(reason: LaunchReason, stderr_is_terminal: bool) -> bool {
    reason != LaunchReason::Daemon && !stderr_is_terminal
}

/// Only a user opening the app is a moment to ask for notification permission: the daemon's
/// own tray and a click relaunch run unattended, so they only read the current status.
pub fn should_request_authorization(reason: LaunchReason) -> bool {
    reason == LaunchReason::User
}

/// How many times to try the instance lock: a tray the daemon just started waits for an old
/// one to leave, anyone else gives up at once (and hands over to the running tray).
pub fn lock_attempts(reason: LaunchReason) -> u32 {
    if reason == LaunchReason::Daemon {
        TAKEOVER_ATTEMPTS
    } else {
        1
    }
}

/// How long a daemon-started tray waits for an old tray to let go of the lock.
pub const TAKEOVER_ATTEMPTS: u32 = 6;
pub const TAKEOVER_PAUSE: Duration = Duration::from_millis(300);

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_reason_comes_from_the_environment_then_the_arguments() {
        assert_eq!(reason_from(None, &[]), LaunchReason::User);
        assert_eq!(
            reason_from(None, &args(&["--home", "/x", "-psn_0_1234"])),
            LaunchReason::User,
            "a LaunchServices start has no marker"
        );
        assert_eq!(reason_from(Some("daemon"), &[]), LaunchReason::Daemon);
        assert_eq!(
            reason_from(Some("click"), &[]),
            LaunchReason::NotificationClick
        );
        assert_eq!(
            reason_from(None, &args(&["--notification-click"])),
            LaunchReason::NotificationClick
        );
        assert_eq!(reason_from(Some("something else"), &[]), LaunchReason::User);
    }

    struct FakeHost {
        running: RefCell<bool>,
        start_fails: bool,
        window_fails: bool,
        calls: RefCell<Vec<&'static str>>,
    }

    impl FakeHost {
        fn new(running: bool) -> Self {
            Self {
                running: RefCell::new(running),
                start_fails: false,
                window_fails: false,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Host for FakeHost {
        fn daemon_running(&self) -> bool {
            *self.running.borrow()
        }

        fn start_daemon(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("start_daemon");
            if self.start_fails {
                return Err("cannot start clusiad".into());
            }
            *self.running.borrow_mut() = true;
            Ok(())
        }

        fn open_window(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("open_window");
            if self.window_fails {
                return Err("no window installed".into());
            }
            Ok(())
        }
    }

    #[test]
    fn opening_the_app_brings_everything_up() {
        // Nothing running: the daemon starts, then the window opens.
        let host = FakeHost::new(false);
        let out = bring_up(LaunchReason::User, &host);
        assert_eq!(
            out,
            BringUp {
                daemon_started: true,
                window_opened: true,
                error: None
            }
        );
        assert_eq!(*host.calls.borrow(), ["start_daemon", "open_window"]);

        // Only the daemon (and its tray) running: just the window.
        let host = FakeHost::new(true);
        let out = bring_up(LaunchReason::User, &host);
        assert!(!out.daemon_started && out.window_opened);
        assert_eq!(*host.calls.borrow(), ["open_window"]);

        // A second quick launch repeats the same thing without harm.
        let again = bring_up(LaunchReason::User, &host);
        assert_eq!(again, out);
    }

    #[test]
    fn a_daemon_that_will_not_start_shows_no_window() {
        let mut host = FakeHost::new(false);
        host.start_fails = true;
        let out = bring_up(LaunchReason::User, &host);
        assert_eq!(out.error.as_deref(), Some("cannot start clusiad"));
        assert!(!out.window_opened);
        assert_eq!(*host.calls.borrow(), ["start_daemon"]);
    }

    #[test]
    fn a_missing_window_is_reported_and_the_tray_still_runs() {
        let mut host = FakeHost::new(true);
        host.window_fails = true;
        let out = bring_up(LaunchReason::User, &host);
        assert_eq!(out.error.as_deref(), Some("no window installed"));
        assert!(!out.window_opened);
    }

    #[test]
    fn the_daemons_own_tray_and_a_click_open_no_window() {
        let host = FakeHost::new(true);
        assert_eq!(
            bring_up(LaunchReason::Daemon, &host),
            BringUp::default(),
            "the daemon started it: nothing to bring up"
        );
        assert!(host.calls.borrow().is_empty());
        let out = bring_up(LaunchReason::NotificationClick, &host);
        assert!(!out.window_opened);
        assert!(
            host.calls.borrow().is_empty(),
            "a click opens its target, not Home"
        );

        // A click that relaunched the tray after the daemon died brings the daemon back.
        let host = FakeHost::new(false);
        let out = bring_up(LaunchReason::NotificationClick, &host);
        assert!(out.daemon_started && !out.window_opened);
        assert_eq!(*host.calls.borrow(), ["start_daemon"]);
    }

    #[test]
    fn logging_goes_to_the_file_unless_someone_else_already_handles_it() {
        assert!(logs_to_file(LaunchReason::User, false));
        assert!(logs_to_file(LaunchReason::NotificationClick, false));
        assert!(
            !logs_to_file(LaunchReason::User, true),
            "a terminal keeps its output"
        );
        assert!(
            !logs_to_file(LaunchReason::Daemon, false),
            "the daemon redirects it"
        );
    }

    /// A daemon that records its arguments and fails: the launcher returns as soon as it exits,
    /// so no test waits on a timeout (a fresh executable can take a while to first run).
    fn recording_daemon(dir: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let record = dir.join("args");
        let bin = dir.join("fake-clusiad");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho \"[$@]\" > '{}'\nexit 1\n",
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        (bin, record)
    }

    /// What the fake daemon wrote before it exited.
    fn recorded(record: &Path) -> String {
        std::fs::read_to_string(record)
            .expect("the fake daemon ran")
            .trim()
            .to_string()
    }

    #[test]
    fn a_daemon_the_tray_starts_gets_the_trays_own_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let (bin, record) = recording_daemon(dir.path());
        let mut host = SystemHost::new(Paths::new(&home), None, Some(home.clone()));
        host.daemon_bin = Some(bin);
        host.daemon_wait = Duration::from_secs(30);
        assert!(host.start_daemon().is_err(), "the fake exits at once");
        assert_eq!(recorded(&record), format!("[--home {}]", home.display()));
    }

    #[test]
    fn without_a_home_argument_the_daemon_finds_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, record) = recording_daemon(dir.path());
        let mut host = SystemHost::new(Paths::new(dir.path().join("home")), None, None);
        host.daemon_bin = Some(bin);
        host.daemon_wait = Duration::from_secs(30);
        assert!(host.start_daemon().is_err());
        assert_eq!(recorded(&record), "[]");
    }

    #[test]
    fn only_a_user_start_asks_for_notification_permission() {
        assert!(should_request_authorization(LaunchReason::User));
        assert!(!should_request_authorization(LaunchReason::Daemon));
        assert!(!should_request_authorization(
            LaunchReason::NotificationClick
        ));
    }

    #[test]
    fn only_a_daemon_started_tray_waits_for_the_lock() {
        assert_eq!(lock_attempts(LaunchReason::User), 1);
        assert_eq!(lock_attempts(LaunchReason::NotificationClick), 1);
        assert_eq!(lock_attempts(LaunchReason::Daemon), TAKEOVER_ATTEMPTS);
    }

    #[test]
    fn a_second_tray_cannot_take_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.lock");
        let first = InstanceLock::acquire(&path).unwrap();
        assert!(first.is_some());
        assert!(InstanceLock::acquire(&path).unwrap().is_none());
        drop(first);
        // A process forked by a parallel test holds the lock until it execs, so allow a moment.
        let again = InstanceLock::acquire_waiting(&path, 100, Duration::from_millis(20)).unwrap();
        assert!(again.is_some(), "free again once the first tray let go");
    }

    #[test]
    fn a_new_tray_waits_for_an_old_one_to_leave() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.lock");
        let old = InstanceLock::acquire(&path).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(old);
        });
        let got = InstanceLock::acquire_waiting(&path, 20, Duration::from_millis(50)).unwrap();
        release.join().unwrap();
        assert!(got.is_some(), "the old tray let go within the wait");

        let held = InstanceLock::acquire(&dir.path().join("other.lock")).unwrap();
        assert!(held.is_some());
        let none = InstanceLock::acquire_waiting(
            &dir.path().join("other.lock"),
            2,
            Duration::from_millis(10),
        )
        .unwrap();
        assert!(none.is_none(), "a live tray keeps the lock");
    }
}
