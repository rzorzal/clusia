use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use clusia_core::logging::{DailyLog, KEEP_FILES};
use clusia_core::{Paths, PrRef};
use clusiad::{Daemon, DaemonOptions, StartError};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Parser)]
#[command(name = "clusiad", version, about = "Clúsia daemon")]
/// Environment overrides: CLUSIA_HOME, CLUSIA_GITHUB_API, CLUSIA_GITHUB_TOKEN, CLUSIA_GH_BIN,
/// CLUSIA_SECRET_STORE=memory, CLUSIA_TRAY_BIN, CLUSIA_CLAUDE_BIN, CLUSIA_GIPHY_API. The daemon
/// keeps the environment of whoever started it (see docs/daemon.md).
struct Args {
    /// Data directory (defaults to $CLUSIA_HOME or ~/Library/Application Support/Clusia).
    #[arg(long, value_name = "DIR")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Mode>,
}

#[derive(Subcommand)]
enum Mode {
    /// The stdio MCP server `claude` starts to ask the reviewer for permission. Started by
    /// the daemon's own command line, never by hand.
    #[command(hide = true)]
    PermissionBridge {
        /// The daemon's Unix socket.
        #[arg(long, value_name = "PATH")]
        socket: PathBuf,
        /// The review the turn belongs to, as `owner/repo#number`.
        #[arg(long)]
        pr: PrRef,
        /// The agent turn that asks.
        #[arg(long)]
        turn: u64,
    },
}

/// Logs go to `daemon.log` in the logs folder; a daemon started from a terminal logs there.
fn init_logging(paths: &Paths) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if std::io::stderr().is_terminal() {
        builder.with_writer(std::io::stderr).init();
        return;
    }
    match DailyLog::open(paths.logs_dir(), "daemon", KEEP_FILES) {
        Ok(log) => builder
            .with_ansi(false)
            .with_writer(move || log.clone())
            .init(),
        Err(e) => {
            eprintln!(
                "clusiad: cannot open the log in {}: {e}",
                paths.logs_dir().display()
            );
            builder.with_writer(std::io::stderr).init();
        }
    }
}

fn start_failed(e: &StartError) -> ExitCode {
    eprintln!("clusiad: {e}");
    match e {
        StartError::AlreadyRunning(_) => ExitCode::from(already_running_status(
            std::env::var("XPC_SERVICE_NAME").ok().as_deref(),
        )),
        _ => ExitCode::FAILURE,
    }
}

/// The launcher waits on 3, which tells it another daemon won. Under the login agent's job
/// (launchd names it in `XPC_SERVICE_NAME`) any failure is a crash to restart, and another
/// daemon running is not one: launchd would retry every few seconds while it lives, and win
/// the lock after a Quit.
fn already_running_status(xpc_service: Option<&str>) -> u8 {
    if xpc_service == Some(clusia_core::launch_agent::LABEL) {
        0
    } else {
        3
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    // The bridge speaks MCP on standard output, so it neither logs there nor takes the lock.
    if let Some(Mode::PermissionBridge { socket, pr, turn }) = args.command {
        return match clusiad::run_permission_bridge(socket, pr, turn).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("clusiad permission-bridge: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let paths = match args.home {
        Some(home) => match std::path::absolute(&home) {
            Ok(home) => Paths::new(home),
            Err(e) => {
                eprintln!("clusiad: cannot resolve --home {}: {e}", home.display());
                return ExitCode::FAILURE;
            }
        },
        None => match Paths::from_env() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("clusiad: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    // The lock comes before the log is opened: a daemon that loses it only writes to stderr.
    let lock = match Daemon::acquire_lock(&paths) {
        Ok(lock) => lock,
        Err(e) => return start_failed(&e),
    };
    init_logging(&paths);

    let daemon = match Daemon::bind_locked(paths, DaemonOptions::from_env(), lock).await {
        Ok(d) => d,
        Err(e) => return start_failed(&e),
    };

    let handle = daemon.shutdown_handle();
    tokio::spawn(async move {
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM; waiting for Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        handle.trigger();
    });

    match daemon.run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("clusiad: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_daemon_under_the_login_agent_exits_cleanly() {
        assert_eq!(
            already_running_status(Some("io.github.rzorzal.clusia.daemon")),
            0
        );
        assert_eq!(already_running_status(None), 3);
        assert_eq!(already_running_status(Some("0")), 3);
        assert_eq!(already_running_status(Some("com.apple.Terminal")), 3);
    }
}
