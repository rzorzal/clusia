use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
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
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("clusia-tray: must run on the main thread");
        return ExitCode::FAILURE;
    };
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
        for snapshot in fixture::demo(now) {
            model.apply(snapshot);
        }
        return match ui::render::render_png(mtm, layout::layout(&model.view(now)), args.dark, out) {
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
    ui::app::run(mtm, paths, clusia_tray::actions::app_binary());
    ExitCode::SUCCESS
}
