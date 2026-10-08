use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use clusia_core::logging::{DailyLog, KEEP_FILES};
use clusia_tray::launch::{self, InstanceLock, LAUNCHED_BY, LaunchReason, SystemHost};
use clusia_tray::{fixture, layout, model::TrayModel, ui};
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;

#[derive(Parser)]
#[command(name = "clusia-tray", version, about = "Clúsia menu bar tray")]
struct Args {
    /// Clúsia home (defaults to CLUSIA_HOME or ~/Library/Application Support/Clusia).
    #[arg(long)]
    home: Option<PathBuf>,
    /// Paint the popover with demo data to this PNG and exit (no menu bar item, no daemon).
    #[arg(long, value_name = "PNG")]
    render: Option<PathBuf>,
    /// Draw the menu bar icon to this PNG and exit.
    #[arg(long, value_name = "PNG")]
    render_icon: Option<PathBuf>,
    /// With --render-icon: draw the variant with the new-activity dot.
    #[arg(long)]
    news: bool,
    /// With --render: use the dark appearance.
    #[arg(long)]
    dark: bool,
    /// With --render: fill the bitmap with this color first, simulating the wallpaper behind the glass.
    #[arg(long, value_name = "#RRGGBB", value_parser = parse_hex, hide = true)]
    backdrop: Option<(f64, f64, f64)>,
    /// The process was started by a notification click.
    #[arg(long, hide = true)]
    notification_click: bool,
}

fn parse_hex(s: &str) -> Result<(f64, f64, f64), String> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 || !hex.is_ascii() {
        return Err(format!("expected #RRGGBB, got {s:?}"));
    }
    let byte = |i: usize| {
        u8::from_str_radix(&hex[i..i + 2], 16)
            .map(|v| f64::from(v) / 255.0)
            .map_err(|e| format!("bad hex color {s:?}: {e}"))
    };
    Ok((byte(0)?, byte(2)?, byte(4)?))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}

fn log_to_stderr() {
    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .with_writer(std::io::stderr)
        .init();
}

/// A tray the user started logs to `tray.log` in the logs folder; one the daemon started has its
/// stderr redirected there already, and a terminal keeps its output. Only the tray holding the
/// instance lock opens the file: opening it rotates it, which a second instance must not do
/// under the running tray.
fn init_logging(paths: &clusia_core::Paths, reason: LaunchReason, holds_lock: bool) {
    if holds_lock && launch::logs_to_file(reason, std::io::stderr().is_terminal()) {
        match DailyLog::open(paths.logs_dir(), "tray", KEEP_FILES) {
            Ok(log) => {
                tracing_subscriber::fmt()
                    .with_env_filter(env_filter())
                    .with_ansi(false)
                    .with_writer(move || log.clone())
                    .init();
                return;
            }
            Err(e) => eprintln!(
                "clusia-tray: cannot open the log in {}: {e}",
                paths.logs_dir().display()
            ),
        }
    }
    log_to_stderr();
}

fn main() -> ExitCode {
    let args = Args::parse_from(launch::without_psn(std::env::args_os()));
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("clusia-tray: must run on the main thread");
        return ExitCode::FAILURE;
    };
    if args.render.is_some() || args.render_icon.is_some() {
        log_to_stderr();
    }
    if let Some(out) = &args.render_icon {
        return match ui::icon::render_png(args.news, out) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("clusia-tray: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if let Some(out) = &args.render {
        let _app = NSApplication::sharedApplication(mtm);
        let now = now();
        let mut model = TrayModel::new(true);
        model.apply(fixture::demo(now));
        return match ui::render::render_png(
            mtm,
            layout::layout(&model.view(now)),
            args.dark,
            args.backdrop,
            out,
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("clusia-tray: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let paths = match &args.home {
        Some(home) => match std::path::absolute(home) {
            Ok(home) => clusia_core::Paths::new(home),
            Err(e) => {
                eprintln!("clusia-tray: cannot resolve --home {}: {e}", home.display());
                return ExitCode::FAILURE;
            }
        },
        None => match clusia_core::Paths::from_env() {
            Ok(paths) => paths,
            Err(e) => {
                eprintln!("clusia-tray: {e}");
                return ExitCode::FAILURE;
            }
        },
    };
    let reason = launch::reason_from(
        std::env::var(LAUNCHED_BY).ok().as_deref(),
        &if args.notification_click {
            vec!["--notification-click".to_string()]
        } else {
            Vec::new()
        },
    );
    let app_bin = clusia_tray::actions::app_binary();
    // The resolved home, not the argument: a daemon this tray starts runs in `/`.
    let home = args.home.as_ref().map(|_| paths.root().to_path_buf());
    let lock = InstanceLock::acquire_waiting(
        &paths.root().join("tray.lock"),
        launch::lock_attempts(reason),
        launch::TAKEOVER_PAUSE,
    );
    init_logging(&paths, reason, matches!(lock, Ok(Some(_))));
    let _lock = match lock {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            // Another tray is running: make sure the daemon and (for the user) the window are up,
            // then leave.
            let host = SystemHost::new(paths, app_bin, home);
            if let Some(e) = launch::bring_up(reason, &host).error {
                tracing::warn!(error = %e, "could not bring Clúsia up");
            }
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            // Without the lock the tray still runs; two trays at worst.
            tracing::warn!(error = %e, "cannot take the tray lock");
            None
        }
    };
    ui::app::run(mtm, paths, app_bin, reason, home);
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_services_start_parses() {
        let args = launch::without_psn(["clusia-tray", "-psn_0_1234"].map(Into::into));
        assert!(Args::try_parse_from(args).is_ok());
    }
}
