//! Executes a parsed command against the daemon.

use std::path::Path;

use clusia_core::Paths;
use clusia_protocol::{Client, ClientError, Command as Request, LaunchError, Reply, launcher};
use serde_json::json;

use crate::cli::{Command, ConfigCommand, DaemonCommand};
use crate::spawn;

/// What a successful command prints: `human` normally, `json` with `--json`.
pub struct Output {
    pub human: String,
    pub json: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("Clúsia daemon not running. Start it with: clusia daemon start")]
    NotRunning,
    #[error("{0}")]
    Client(ClientError),
    #[error("{0}")]
    Launch(LaunchError),
    #[error("{0}")]
    Other(String),
}

impl From<LaunchError> for CliError {
    fn from(e: LaunchError) -> Self {
        match e {
            LaunchError::Client(c) => c.into(),
            other => CliError::Launch(other),
        }
    }
}

impl From<ClientError> for CliError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::NotRunning(_) => CliError::NotRunning,
            other => CliError::Client(other),
        }
    }
}

impl CliError {
    pub fn exit_code(&self) -> u8 {
        match self {
            CliError::NotRunning => 3,
            _ => 1,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            CliError::NotRunning => "not_running",
            CliError::Client(ClientError::Server(_)) => "server",
            CliError::Client(ClientError::Incompatible { .. }) => "incompatible",
            CliError::Client(_) => "connection",
            CliError::Launch(_) => "launch",
            CliError::Other(_) => "other",
        }
    }
}

pub async fn run(paths: &Paths, home: Option<&Path>, command: Command) -> Result<Output, CliError> {
    match command {
        Command::Daemon(DaemonCommand::Start) => spawn::start(paths, home).await,
        Command::Daemon(DaemonCommand::Stop) => spawn::stop(paths).await,
        Command::Daemon(DaemonCommand::Status) => status(paths).await,
        Command::Config(cmd) => config(paths, home, cmd).await,
    }
}

/// Connects to the daemon, starting it when needed (spec §3.1).
async fn connect(paths: &Paths, home: Option<&Path>) -> Result<Client, CliError> {
    let (client, started) = launcher::ensure_daemon(paths, home, "clusia").await?;
    if started {
        eprintln!("clusia: started the Clúsia daemon");
    }
    Ok(client)
}

fn unexpected(reply: Reply) -> CliError {
    CliError::Other(format!("unexpected reply from the daemon: {reply:?}"))
}

pub fn uptime(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

async fn status(paths: &Paths) -> Result<Output, CliError> {
    let mut client = Client::connect(&paths.socket(), "clusia").await?;
    match client.request(Request::DaemonStatus).await? {
        Reply::Status(s) => Ok(Output {
            human: format!(
                "clusiad {} · pid {} · up {} · {} client(s)\nsocket: {}",
                s.version,
                s.pid,
                uptime(s.uptime_secs),
                s.clients,
                s.socket
            ),
            json: serde_json::to_value(&s).unwrap_or_default(),
        }),
        other => Err(unexpected(other)),
    }
}

async fn config(
    paths: &Paths,
    home: Option<&Path>,
    cmd: ConfigCommand,
) -> Result<Output, CliError> {
    let mut client = connect(paths, home).await?;
    match cmd {
        ConfigCommand::Show => match client.request(Request::GetConfig).await? {
            Reply::Config(c) => Ok(Output {
                human: toml::to_string_pretty(&c).map_err(|e| CliError::Other(e.to_string()))?,
                json: serde_json::to_value(&c).unwrap_or_default(),
            }),
            other => Err(unexpected(other)),
        },
        ConfigCommand::Get { key } => match client
            .request(Request::GetConfigValue { key: key.clone() })
            .await?
        {
            Reply::Value(v) => Ok(Output {
                human: v.clone(),
                json: json!({ "key": key, "value": v }),
            }),
            other => Err(unexpected(other)),
        },
        ConfigCommand::Set { key, value } => {
            match client
                .request(Request::SetConfigValue {
                    key: key.clone(),
                    value,
                })
                .await?
            {
                Reply::Value(v) => Ok(Output {
                    human: format!("{key} = {v}"),
                    json: json!({ "key": key, "value": v }),
                }),
                other => Err(unexpected(other)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_is_compact() {
        assert_eq!(uptime(5), "5s");
        assert_eq!(uptime(65), "1m 5s");
        assert_eq!(uptime(3 * 3600 + 120), "3h 2m");
    }

    #[test]
    fn not_running_maps_to_exit_3() {
        let e: CliError = ClientError::NotRunning("/x".into()).into();
        assert_eq!(e.exit_code(), 3);
        assert_eq!(e.kind(), "not_running");
        assert_eq!(CliError::Other("x".into()).exit_code(), 1);
    }
}
