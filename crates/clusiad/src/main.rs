use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use clusia_core::Paths;
use clusiad::{Daemon, StartError};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Parser)]
#[command(name = "clusiad", version, about = "Clúsia daemon")]
struct Args {
    /// Data directory (defaults to $CLUSIA_HOME or ~/Library/Application Support/Clusia).
    #[arg(long, value_name = "DIR")]
    home: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let paths = match args.home {
        Some(home) => Paths::new(home),
        None => match Paths::from_env() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("clusiad: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    let daemon = match Daemon::bind(paths).await {
        Ok(d) => d,
        Err(e @ StartError::AlreadyRunning(_)) => {
            eprintln!("clusiad: {e}");
            return ExitCode::from(3);
        }
        Err(e) => {
            eprintln!("clusiad: {e}");
            return ExitCode::FAILURE;
        }
    };

    let handle = daemon.shutdown_handle();
    tokio::spawn(async move {
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM");
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
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
