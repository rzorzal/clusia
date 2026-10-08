//! `app.log` in the logs folder: the window logs through the daemon's daily-rotating writer,
//! like the daemon does for `daemon.log`.

use std::io::IsTerminal;

use clusia_core::Paths;
use clusia_core::logging::{DailyLog, KEEP_FILES};

/// The log's name in the logs folder (`app.log`).
pub const LOG_NAME: &str = "app";

pub fn open(paths: &Paths) -> std::io::Result<DailyLog> {
    DailyLog::open(paths.logs_dir(), LOG_NAME, KEEP_FILES)
}

/// Starts logging: to the terminal when the window was started from one or runs on demo data,
/// otherwise (opened from the Dock, Finder or the tray) to `app.log`. A log that cannot be
/// opened falls back to the terminal.
pub fn init(paths: &Paths, demo: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if demo || std::io::stderr().is_terminal() {
        builder.with_writer(std::io::stderr).init();
        return;
    }
    match open(paths) {
        Ok(log) => builder
            .with_ansi(false)
            .with_writer(move || log.clone())
            .init(),
        Err(e) => {
            eprintln!(
                "clusia-app: cannot open the log in {}: {e}",
                paths.logs_dir().display()
            );
            builder.with_writer(std::io::stderr).init();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn the_app_log_is_a_private_file_in_the_logs_folder() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let mut log = open(&paths).unwrap();
        writeln!(log, "window started").unwrap();
        log.flush().unwrap();
        let file = paths.logs_dir().join("app.log");
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("window started")
        );
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_second_window_process_appends_to_the_same_day() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        writeln!(open(&paths).unwrap(), "one").unwrap();
        writeln!(open(&paths).unwrap(), "two").unwrap();
        let text = std::fs::read_to_string(paths.logs_dir().join("app.log")).unwrap();
        assert_eq!(text.lines().collect::<Vec<_>>(), ["one", "two"]);
    }
}
