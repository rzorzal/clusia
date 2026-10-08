use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use clusia_app::app::{self, Launch, Mode};
use clusia_app::args::Args;
use clusia_app::instance::{self, InstanceLock};
use clusia_core::Paths;
use clusia_core::config::Theme;

fn main() -> ExitCode {
    let args = Args::parse();
    let target = match args.target() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("clusia-app: {e}");
            return ExitCode::from(2);
        }
    };
    let (paths, home) = match resolve(&args) {
        Ok(p) => p,
        Err(message) => {
            eprintln!("clusia-app: {message}");
            return ExitCode::FAILURE;
        }
    };
    clusia_app::logging::init(&paths, args.demo);
    let launch = Launch {
        paths: paths.clone(),
        home,
        target,
        mode: if args.demo {
            Mode::Demo {
                theme: demo_theme(&args),
            }
        } else {
            Mode::Live
        },
        screenshot: args.screenshot.clone(),
        frames: args.frames,
        frame_step: args.frame_ms.map(Duration::from_millis),
        scene: args.scene,
    };
    if args.demo {
        app::run(launch);
        return ExitCode::SUCCESS;
    }
    let _lock = match InstanceLock::acquire(&paths.app_lock()) {
        Ok(Some(lock)) => lock,
        Ok(None) => return hand_over(&launch),
        Err(e) => {
            eprintln!(
                "clusia-app: cannot lock {}: {e}",
                paths.app_lock().display()
            );
            return ExitCode::FAILURE;
        }
    };
    app::run(launch);
    ExitCode::SUCCESS
}

fn resolve(args: &Args) -> Result<(Paths, Option<PathBuf>), String> {
    match &args.home {
        Some(home) => {
            let home = std::path::absolute(home)
                .map_err(|e| format!("cannot resolve --home {}: {e}", home.display()))?;
            Ok((Paths::new(&home), Some(home)))
        }
        None => Paths::from_env()
            .map(|p| (p, None))
            .map_err(|e| e.to_string()),
    }
}

/// Another window owns this home: ask it to show the target, then leave.
fn hand_over(launch: &Launch) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("clusia-app: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        let (mut client, _) = clusia_protocol::launcher::ensure_daemon(
            &launch.paths,
            launch.home.as_deref(),
            clusia_app::CLIENT_NAME,
        )
        .await
        .map_err(|e| e.to_string())?;
        instance::hand_over(&mut client, &launch.target, 15, Duration::from_millis(200))
            .await
            .map_err(|e| e.to_string())
    });
    match result {
        Ok(n) if n > 0 => ExitCode::SUCCESS,
        Ok(_) => {
            eprintln!(
                "clusia-app: another Clúsia window holds {} but is not answering",
                launch.paths.app_lock().display()
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("clusia-app: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The theme a demo run forces; `None` follows the macOS appearance.
fn demo_theme(args: &Args) -> Option<Theme> {
    match (args.dark, args.light) {
        (true, _) => Some(Theme::Dark),
        (_, true) => Some(Theme::Light),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme_of(flags: &[&str]) -> Option<Theme> {
        let argv = ["clusia-app", "--demo"].iter().chain(flags);
        demo_theme(&Args::try_parse_from(argv).unwrap())
    }

    #[test]
    fn demo_flags_pick_the_theme() {
        assert_eq!(theme_of(&["--dark"]), Some(Theme::Dark));
        assert_eq!(theme_of(&["--light"]), Some(Theme::Light));
        assert_eq!(theme_of(&[]), None);
    }
}
