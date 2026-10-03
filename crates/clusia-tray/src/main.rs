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
    render: PathBuf,
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
    let args = Args::parse();
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("clusia-tray: must run on the main thread");
        return ExitCode::FAILURE;
    };
    let _app = NSApplication::sharedApplication(mtm);
    let now = now();
    let mut model = TrayModel::new(true);
    for snapshot in fixture::demo(now) {
        model.apply(snapshot);
    }
    match ui::render::render_png(
        mtm,
        layout::layout(&model.view(now)),
        args.dark,
        &args.render,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("clusia-tray: {e}");
            ExitCode::FAILURE
        }
    }
}
