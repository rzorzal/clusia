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
    /// List pull requests: assigned to me and mine (both unless filtered).
    Prs {
        /// Only pull requests where my review is requested.
        #[arg(long, conflicts_with = "mine")]
        assigned: bool,
        /// Only pull requests I authored.
        #[arg(long)]
        mine: bool,
    },
    /// Refresh pull requests from GitHub now.
    Sync,
    /// GitHub authentication.
    #[command(subcommand)]
    Auth(AuthCommand),
    /// Prepare a local worktree for a pull request and print its path.
    Worktree {
        /// `owner/repo#number` or a pull request URL.
        pr: String,
    },
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

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Show which token is used and for whom.
    Status,
    /// Store a personal access token in the Keychain. The token is read from stdin.
    Login,
    /// Remove the stored token.
    Logout,
}
