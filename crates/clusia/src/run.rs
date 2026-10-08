//! Executes a parsed command against the daemon.

use std::io::{IsTerminal, Read};
use std::path::Path;

use clusia_core::{Paths, PrFilter, PrRef, PrSummary};
use clusia_protocol::{
    Client, ClientError, Command as Request, LaunchError, Reply, SyncState, SyncStatus, launcher,
};
use serde_json::json;

use crate::cli::{AuthCommand, Command, ConfigCommand, DaemonCommand, InstallArgs, UninstallArgs};
use crate::{install, review, spawn};

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
        Command::Prs { assigned, mine } => prs(paths, home, assigned, mine).await,
        Command::Sync => sync(paths, home).await,
        Command::Auth(cmd) => auth(paths, home, cmd).await,
        Command::Worktree { pr } => worktree(paths, home, &pr).await,
        Command::Open { pr } => review::open(paths, home, &pr).await,
        Command::Review(cmd) => review::run(paths, home, cmd).await,
        Command::Activity => review::activity(paths, home).await,
        Command::Install(args) => install_command(paths, home, &args),
        Command::Uninstall(args) => uninstall_command(paths, home, &args),
    }
}

/// Connects to the daemon, starting it when needed (spec §3.1).
pub(crate) async fn connect(paths: &Paths, home: Option<&Path>) -> Result<Client, CliError> {
    let (client, started) = launcher::ensure_daemon(paths, home, "clusia").await?;
    if started {
        eprintln!("clusia: started the Clúsia daemon");
    }
    Ok(client)
}

pub(crate) fn unexpected(reply: Reply) -> CliError {
    CliError::Other(format!("unexpected reply from the daemon: {reply:?}"))
}

pub(crate) fn uptime(secs: u64) -> String {
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

fn state_name(state: SyncState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sync_line(s: &SyncStatus) -> String {
    sync_line_at(s, now_unix())
}

fn sync_line_at(s: &SyncStatus, now: i64) -> String {
    let mut line = format!("sync: {}", state_name(s.state));
    if let Some(message) = s.message.as_deref().filter(|m| !m.is_empty()) {
        line.push_str(&format!(" — {message}"));
    }
    if let Some(next) = s.next_sync_unix {
        line.push_str(&format!(" · next sync in {}s", (next - now).max(0)));
    }
    if s.paused {
        line.push_str(" · paused");
    }
    line
}

fn not_logged_in(error: Option<String>) -> String {
    match error.filter(|e| !e.is_empty()) {
        Some(e) => format!("Not logged in: {e}"),
        None => "Not logged in".to_string(),
    }
}

fn pr_line(p: &PrSummary) -> String {
    let draft = if p.draft { " [draft]" } else { "" };
    format!("  {}  {}  @{}{}", p.pr, p.title, p.author, draft)
}

async fn list(client: &mut Client, filter: PrFilter) -> Result<Vec<PrSummary>, CliError> {
    match client.request(Request::ListPrs { filter }).await? {
        Reply::Prs(list) => Ok(list),
        other => Err(unexpected(other)),
    }
}

async fn prs(
    paths: &Paths,
    home: Option<&Path>,
    assigned: bool,
    mine: bool,
) -> Result<Output, CliError> {
    let mut client = connect(paths, home).await?;
    let (want_assigned, want_mine) = if assigned || mine {
        (assigned, mine)
    } else {
        (true, true)
    };
    let mut human = Vec::new();
    let mut json = serde_json::Map::new();
    for (wanted, filter, title, key) in [
        (
            want_assigned,
            PrFilter::Assigned,
            "Assigned to me",
            "assigned",
        ),
        (want_mine, PrFilter::Mine, "Mine", "mine"),
    ] {
        if !wanted {
            continue;
        }
        let items = list(&mut client, filter).await?;
        human.push(format!("{title} ({})", items.len()));
        if items.is_empty() {
            human.push("  (none)".to_string());
        }
        human.extend(items.iter().map(pr_line));
        json.insert(key.into(), serde_json::to_value(&items).unwrap_or_default());
    }
    let status = match client.request(Request::GetSyncStatus).await? {
        Reply::Sync(s) => s,
        other => return Err(unexpected(other)),
    };
    if status.state != SyncState::Online {
        human.push(sync_line(&status));
    }
    json.insert(
        "sync".into(),
        serde_json::to_value(&status).unwrap_or_default(),
    );
    Ok(Output {
        human: human.join("\n"),
        json: serde_json::Value::Object(json),
    })
}

async fn sync(paths: &Paths, home: Option<&Path>) -> Result<Output, CliError> {
    match connect(paths, home)
        .await?
        .request(Request::SyncNow)
        .await?
    {
        Reply::Sync(s) => {
            let human = match (s.state, s.next_sync_unix, s.last_sync_unix) {
                (SyncState::Online, Some(next), Some(last)) => {
                    format!("synced: online · next sync in {}s", next - last)
                }
                _ => sync_line(&s),
            };
            Ok(Output {
                human,
                json: serde_json::to_value(&s).unwrap_or_default(),
            })
        }
        other => Err(unexpected(other)),
    }
}

fn read_token_from_stdin() -> Result<String, CliError> {
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err(CliError::Other(
            "pipe the token on stdin, e.g. `gh auth token | clusia auth login`".into(),
        ));
    }
    let mut input = String::new();
    stdin
        .read_to_string(&mut input)
        .map_err(|e| CliError::Other(format!("cannot read stdin: {e}")))?;
    input
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .ok_or_else(|| CliError::Other("no token on stdin".into()))
}

async fn auth(paths: &Paths, home: Option<&Path>, cmd: AuthCommand) -> Result<Output, CliError> {
    match cmd {
        AuthCommand::Login => {
            let token = read_token_from_stdin()?;
            connect(paths, home)
                .await?
                .request(Request::SetToken {
                    token: token.into(),
                })
                .await?;
            Ok(Output {
                human: "Token saved to the Keychain".into(),
                json: json!({ "saved": true }),
            })
        }
        AuthCommand::Logout => {
            connect(paths, home)
                .await?
                .request(Request::ClearToken)
                .await?;
            Ok(Output {
                human: "Token removed".into(),
                json: json!({ "removed": true }),
            })
        }
        AuthCommand::Status => {
            let mut client = connect(paths, home).await?;
            let host = match client
                .request(Request::GetConfigValue {
                    key: "github.host".into(),
                })
                .await?
            {
                Reply::Value(v) => v,
                other => return Err(unexpected(other)),
            };
            let info = match client.request(Request::AuthStatus).await? {
                Reply::Auth(a) => a,
                other => return Err(unexpected(other)),
            };
            match (&info.login, &info.source) {
                (Some(login), Some(source)) => {
                    let source = serde_json::to_value(source)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default();
                    let mut human =
                        format!("Logged in to {host} as @{login} (token from {source})");
                    if !info.scopes.is_empty() {
                        human.push_str(&format!("\nscopes: {}", info.scopes.join(", ")));
                    }
                    let mut value = serde_json::to_value(&info).unwrap_or_default();
                    value["host"] = json!(host);
                    Ok(Output { human, json: value })
                }
                _ => Err(CliError::Other(not_logged_in(info.error))),
            }
        }
    }
}

async fn worktree(paths: &Paths, home: Option<&Path>, pr: &str) -> Result<Output, CliError> {
    let pr: PrRef = pr
        .parse()
        .map_err(|e: clusia_core::PrRefError| CliError::Other(e.to_string()))?;
    match connect(paths, home)
        .await?
        .request(Request::PrepareWorktree { pr })
        .await?
    {
        Reply::Worktree(w) => Ok(Output {
            human: w.path.clone(),
            json: serde_json::to_value(&w).unwrap_or_default(),
        }),
        other => Err(unexpected(other)),
    }
}

fn install_plan(paths: &Paths, opts: install::InstallOptions) -> Result<install::Plan, CliError> {
    let start_at_login = clusia_store::load_config(paths)
        .map(|loaded| loaded.into_value().general.start_at_login)
        .unwrap_or(true);
    let env = install::system_env(paths, start_at_login).map_err(CliError::Other)?;
    Ok(install::plan(&opts, &env))
}

fn install_command(
    paths: &Paths,
    home: Option<&Path>,
    args: &InstallArgs,
) -> Result<Output, CliError> {
    let plan = install_plan(
        paths,
        install::InstallOptions {
            applications: args.applications.clone(),
            bin_dir: args.bin_dir.clone(),
            agents_dir: args.agents_dir.clone(),
            from: args.from.clone(),
            workspace: args.workspace.clone(),
            no_launchctl: args.no_launchctl,
        },
    )?;
    if args.dry_run {
        return Ok(Output {
            human: plan.describe_full(),
            json: plan.to_json(),
        });
    }
    let mut os = install::SystemOs::new(paths.clone(), home.map(Path::to_path_buf));
    let report = install::execute(&plan, &mut os).map_err(|e| CliError::Other(e.to_string()))?;
    Ok(Output {
        human: report.describe(),
        json: report.to_json(),
    })
}

fn uninstall_command(
    paths: &Paths,
    home: Option<&Path>,
    args: &UninstallArgs,
) -> Result<Output, CliError> {
    let plan = install_plan(
        paths,
        install::InstallOptions {
            applications: args.applications.clone(),
            bin_dir: args.bin_dir.clone(),
            agents_dir: args.agents_dir.clone(),
            no_launchctl: args.no_launchctl,
            ..install::InstallOptions::default()
        },
    )?;
    let mut os = install::SystemOs::new(paths.clone(), home.map(Path::to_path_buf));
    let removed = install::uninstall(&plan, &mut os).map_err(|e| CliError::Other(e.to_string()))?;
    Ok(Output {
        human: removed.describe(),
        json: removed.to_json(),
    })
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

    fn status(state: SyncState, message: Option<&str>, next: Option<i64>) -> SyncStatus {
        SyncStatus {
            state,
            last_sync_unix: None,
            next_sync_unix: next,
            message: message.map(str::to_string),
            paused: false,
        }
    }

    #[test]
    fn sync_line_has_no_dangling_separator() {
        assert_eq!(
            sync_line_at(&status(SyncState::NotYet, None, None), 100),
            "sync: not_yet"
        );
        assert_eq!(
            sync_line_at(
                &status(SyncState::Unauthorized, Some("no token"), None),
                100
            ),
            "sync: unauthorized — no token"
        );
    }

    #[test]
    fn sync_line_shows_next_sync() {
        assert_eq!(
            sync_line_at(
                &status(SyncState::RateLimited, Some("slow down"), Some(190)),
                100
            ),
            "sync: rate_limited — slow down · next sync in 90s"
        );
        assert_eq!(
            sync_line_at(&status(SyncState::Offline, None, Some(90)), 100),
            "sync: offline · next sync in 0s"
        );
    }

    #[test]
    fn sync_line_says_paused() {
        let mut s = status(SyncState::Online, None, None);
        s.paused = true;
        assert_eq!(sync_line_at(&s, 100), "sync: online · paused");
    }

    #[test]
    fn not_logged_in_has_no_dangling_colon() {
        assert_eq!(not_logged_in(None), "Not logged in");
        assert_eq!(
            not_logged_in(Some("bad token".into())),
            "Not logged in: bad token"
        );
    }

    #[test]
    fn not_running_maps_to_exit_3() {
        let e: CliError = ClientError::NotRunning("/x".into()).into();
        assert_eq!(e.exit_code(), 3);
        assert_eq!(e.kind(), "not_running");
        assert_eq!(CliError::Other("x".into()).exit_code(), 1);
    }
}
