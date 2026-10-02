//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "clusia", version, about = "Clúsia — PR Reviews for Humans")]
pub struct Cli {
    /// Data directory (defaults to $CLUSIA_HOME or ~/Library/Application Support/Clusia).
    #[arg(long, global = true, value_name = "DIR")]
    pub home: Option<PathBuf>,
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start, stop or inspect the background daemon.
    #[command(subcommand)]
    Daemon(DaemonCommand),
    /// Read or change settings.
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Start clusiad in the background.
    Start,
    /// Ask clusiad to stop.
    Stop,
    /// Show whether clusiad is running.
    Status,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the whole configuration.
    Show,
    /// Print one value, e.g. `github.poll_interval_secs`.
    Get { key: String },
    /// Change one value.
    Set { key: String, value: String },
}
