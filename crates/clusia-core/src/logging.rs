//! Log files: one per process under the logs folder, a new file each day, a few days kept,
//! readable by the owner only.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many files a log keeps: the current one and the days before it.
pub const KEEP_FILES: usize = 7;

/// The local calendar day of `unix` seconds, `YYYY-MM-DD`.
pub fn local_date(unix: i64) -> String {
    // SAFETY: `tm` is plain data that `localtime_r` fills in; both pointers are valid.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t: libc::time_t = unix as libc::time_t;
        if libc::localtime_r(&t, &mut tm).is_null() {
            return "unknown".to_string();
        }
        tm
    };
    format!(
        "{:04}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    )
}

fn unix_of(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn today() -> String {
    local_date(unix_of(SystemTime::now()))
}

fn current(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.log"))
}

fn open_private(path: &Path, truncate: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(!truncate)
        .write(true)
        .truncate(truncate)
        .mode(0o600)
        .open(path)?;
    // A file created by an older version may be looser.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Moves `<name>.log` to `<name>.<day>.log` when its last write was on another day than
/// `today`, then prunes.
fn rotate(dir: &Path, name: &str, keep: usize, today: &str) -> io::Result<()> {
    let log = current(dir, name);
    if let Ok(meta) = fs::metadata(&log) {
        let day = local_date(unix_of(meta.modified()?));
        if day != today {
            fs::rename(&log, dir.join(format!("{name}.{day}.log")))?;
        }
    }
    prune(dir, name, keep)
}

/// Whether the file at `path` is the very file `open` holds (same device and inode).
fn same_file(path: &Path, open: &File) -> bool {
    match (fs::metadata(path), open.metadata()) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Deletes the oldest rotated files so that at most `keep` files remain, counting the
/// current one.
fn prune(dir: &Path, name: &str, keep: usize) -> io::Result<()> {
    let prefix = format!("{name}.");
    let mut rotated: Vec<String> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|file| {
            file.strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix(".log"))
                .is_some_and(|day| day.len() == 10 && day.as_bytes()[4] == b'-')
        })
        .collect();
    rotated.sort();
    let excess = rotated.len().saturating_sub(keep.saturating_sub(1));
    for old in rotated.into_iter().take(excess) {
        let _ = fs::remove_file(dir.join(old));
    }
    Ok(())
}

/// Rotates `<name>.log` for a process that writes it through a redirected stdout/stderr
/// (the tray) and creates it empty and private when missing. Call it before each start.
pub fn prepare(dir: &Path, name: &str, keep: usize) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    rotate(dir, name, keep, &today())?;
    open_private(&current(dir, name), false).map(|_| ())
}

struct Inner {
    dir: PathBuf,
    name: String,
    keep: usize,
    day: String,
    file: File,
    today: Box<dyn Fn() -> String + Send>,
}

/// A log writer that starts a new file when the day changes. Clones share one file.
#[derive(Clone)]
pub struct DailyLog(Arc<Mutex<Inner>>);

impl DailyLog {
    pub fn open(dir: &Path, name: &str, keep: usize) -> io::Result<Self> {
        Self::open_with(dir, name, keep, Box::new(today))
    }

    pub(crate) fn open_with(
        dir: &Path,
        name: &str,
        keep: usize,
        today: Box<dyn Fn() -> String + Send>,
    ) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let day = today();
        rotate(dir, name, keep, &day)?;
        let file = open_private(&current(dir, name), false)?;
        Ok(Self(Arc::new(Mutex::new(Inner {
            dir: dir.to_path_buf(),
            name: name.to_string(),
            keep,
            day,
            file,
            today,
        }))))
    }
}

impl Inner {
    /// Starts the new day's file when the day changed since the last write. The log is
    /// reopened whatever became of the rename, so lines never go to a file that was moved.
    fn roll(&mut self) -> io::Result<()> {
        let now = (self.today)();
        if now == self.day {
            return Ok(());
        }
        let log = current(&self.dir, &self.name);
        // Only a file this log still holds is moved: another process may have rotated
        // `<name>.log` already, and its new file must not replace a dated one.
        let renamed = if same_file(&log, &self.file) {
            fs::rename(
                &log,
                self.dir.join(format!("{}.{}.log", self.name, self.day)),
            )
        } else {
            Ok(())
        };
        self.day = now;
        let _ = prune(&self.dir, &self.name, self.keep);
        self.file = open_private(&log, false)?;
        renamed
    }
}

impl Write for DailyLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self.0.lock().unwrap_or_else(|p| p.into_inner());
        // A failed roll keeps logging to the old file rather than losing the line.
        if let Err(e) = inner.roll() {
            eprintln!("{} log: cannot rotate it: {}", inner.name, e);
        }
        inner.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .file
            .flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn local_date_is_a_calendar_day() {
        let day = local_date(1_790_000_000);
        assert_eq!(day.len(), 10);
        assert!(day.starts_with("2026-"), "{day}");
    }

    #[test]
    fn the_log_is_created_private_and_a_looser_one_is_tightened() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("daemon.log");
        fs::write(&log, "old\n").unwrap();
        fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).unwrap();
        let mut w = DailyLog::open(dir.path(), "daemon", KEEP_FILES).unwrap();
        w.write_all(b"line\n").unwrap();
        assert_eq!(mode(&log), 0o600);
        assert_eq!(fs::read_to_string(&log).unwrap(), "old\nline\n");
        let fresh = tempfile::tempdir().unwrap();
        DailyLog::open(&fresh.path().join("Logs"), "app", KEEP_FILES).unwrap();
        assert_eq!(mode(&fresh.path().join("Logs/app.log")), 0o600);
    }

    #[test]
    fn a_new_day_starts_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let clock = calls.clone();
        let days = ["2026-10-01", "2026-10-01", "2026-10-02"];
        let mut w = DailyLog::open_with(
            dir.path(),
            "daemon",
            KEEP_FILES,
            Box::new(move || days[clock.fetch_add(1, Ordering::SeqCst).min(2)].to_string()),
        )
        .unwrap();
        w.write_all(b"first\n").unwrap();
        w.write_all(b"second\n").unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("daemon.2026-10-01.log")).unwrap(),
            "first\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("daemon.log")).unwrap(),
            "second\n"
        );
        assert_eq!(mode(&dir.path().join("daemon.log")), 0o600);
    }

    #[test]
    fn a_file_from_an_earlier_day_is_rotated_when_the_log_opens() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("tray.log");
        fs::write(&log, "yesterday\n").unwrap();
        let three_days_ago = SystemTime::now() - std::time::Duration::from_secs(3 * 86_400);
        File::options()
            .write(true)
            .open(&log)
            .unwrap()
            .set_modified(three_days_ago)
            .unwrap();
        prepare(dir.path(), "tray", KEEP_FILES).unwrap();
        let rotated = format!("tray.{}.log", local_date(unix_of(three_days_ago)));
        assert_eq!(names(dir.path()), [rotated.clone(), "tray.log".to_string()]);
        assert_eq!(
            fs::read_to_string(dir.path().join(rotated)).unwrap(),
            "yesterday\n"
        );
        assert_eq!(fs::read_to_string(&log).unwrap(), "");
        assert_eq!(mode(&log), 0o600);
    }

    #[test]
    fn only_the_newest_files_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        for day in 1..=9 {
            fs::write(dir.path().join(format!("daemon.2026-09-{day:02}.log")), "x").unwrap();
        }
        fs::write(dir.path().join("app.2026-09-01.log"), "x").unwrap();
        fs::write(dir.path().join("daemon.launchd.log"), "x").unwrap();
        DailyLog::open(dir.path(), "daemon", 4).unwrap();
        assert_eq!(
            names(dir.path()),
            [
                "app.2026-09-01.log",
                "daemon.2026-09-07.log",
                "daemon.2026-09-08.log",
                "daemon.2026-09-09.log",
                "daemon.launchd.log",
                "daemon.log",
            ]
        );
    }

    #[test]
    fn a_log_another_process_already_rotated_is_not_moved_over_a_dated_file() {
        let dir = tempfile::tempdir().unwrap();
        let days = Arc::new(AtomicUsize::new(0));
        let clock = days.clone();
        let mut w = DailyLog::open_with(
            dir.path(),
            "daemon",
            KEEP_FILES,
            Box::new(move || {
                ["2026-10-01", "2026-10-02"][clock.load(Ordering::SeqCst).min(1)].to_string()
            }),
        )
        .unwrap();
        w.write_all(b"yesterday\n").unwrap();
        // Someone else moved our file away and started a new `daemon.log`.
        let dated = dir.path().join("daemon.2026-10-01.log");
        fs::rename(dir.path().join("daemon.log"), &dated).unwrap();
        fs::write(dir.path().join("daemon.log"), "theirs\n").unwrap();
        days.store(1, Ordering::SeqCst);
        w.write_all(b"today\n").unwrap();
        assert_eq!(fs::read_to_string(&dated).unwrap(), "yesterday\n");
        assert_eq!(
            fs::read_to_string(dir.path().join("daemon.log")).unwrap(),
            "theirs\ntoday\n"
        );
    }

    #[test]
    fn a_failed_rotation_keeps_logging_to_the_current_file() {
        let dir = tempfile::tempdir().unwrap();
        // A non-empty folder where the rotated file should go makes the rename fail.
        let blocker = dir.path().join("daemon.2026-10-01.log");
        fs::create_dir(&blocker).unwrap();
        fs::write(blocker.join("keep"), "x").unwrap();
        let days = Arc::new(AtomicUsize::new(0));
        let clock = days.clone();
        let mut w = DailyLog::open_with(
            dir.path(),
            "daemon",
            KEEP_FILES,
            Box::new(move || {
                ["2026-10-01", "2026-10-02"][clock.load(Ordering::SeqCst).min(1)].to_string()
            }),
        )
        .unwrap();
        w.write_all(b"first\n").unwrap();
        days.store(1, Ordering::SeqCst);
        w.write_all(b"second\n").unwrap();
        w.write_all(b"third\n").unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("daemon.log")).unwrap(),
            "first\nsecond\nthird\n"
        );
    }
}
