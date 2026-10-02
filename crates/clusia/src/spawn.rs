//! Starting and stopping clusiad from the CLI (until launchd manages it in M6).

use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use clusia_core::Paths;
use clusia_protocol::{Client, ClientError, Command as Request, Reply};
use serde_json::json;

use crate::run::{CliError, Output};

const WAIT_STEP: Duration = Duration::from_millis(100);
const WAIT_STEPS: u32 = 50;

/// `CLUSIA_DAEMON_BIN`, else `clusiad` next to this executable (true inside Clusia.app and in target/).
fn daemon_binary() -> Result<PathBuf, CliError> {
    if let Some(p) = std::env::var_os("CLUSIA_DAEMON_BIN") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe()
        .map_err(|e| CliError::Other(format!("cannot locate the clusia executable: {e}")))?;
    Ok(exe.with_file_name("clusiad"))
}

fn other(context: &str, e: impl std::fmt::Display) -> CliError {
    CliError::Other(format!("{context}: {e}"))
}

// The daemon is meant to outlive this process, so it is deliberately never waited on.
#[allow(clippy::zombie_processes)]
pub async fn start(paths: &Paths, home: Option<&Path>) -> Result<Output, CliError> {
    match Client::connect(&paths.socket(), "clusia").await {
        Err(ClientError::NotRunning(_)) => {}
        Err(e) => return Err(e.into()),
        Ok(mut client) => {
            let pid = match client.request(Request::DaemonStatus).await? {
                Reply::Status(s) => s.pid,
                _ => 0,
            };
            return Ok(Output {
                human: format!("Clúsia daemon already running (pid {pid})"),
                json: json!({ "started": false, "pid": pid }),
            });
        }
    }

    let bin = daemon_binary()?;
    std::fs::create_dir_all(paths.logs_dir())
        .map_err(|e| other("cannot create the logs directory", e))?;
    let log_path = paths.logs_dir().join("daemon.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| other(&format!("cannot open {}", log_path.display()), e))?;
    let log_err = log
        .try_clone()
        .map_err(|e| other("cannot open the daemon log", e))?;

    let mut cmd = Command::new(&bin);
    if let Some(home) = home {
        cmd.arg("--home").arg(home);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .process_group(0)
        .spawn()
        .map_err(|e| other(&format!("cannot start {}", bin.display()), e))?;

    for _ in 0..WAIT_STEPS {
        if Client::connect(&paths.socket(), "clusia").await.is_ok() {
            return Ok(Output {
                human: format!("Clúsia daemon started (pid {})", child.id()),
                json: json!({ "started": true, "pid": child.id() }),
            });
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(CliError::Other(format!(
                "clusiad exited with {status}; see {}",
                log_path.display()
            )));
        }
        tokio::time::sleep(WAIT_STEP).await;
    }
    Err(CliError::Other(format!(
        "clusiad did not start within 5s; see {}",
        log_path.display()
    )))
}

pub async fn stop(paths: &Paths) -> Result<Output, CliError> {
    let mut client = Client::connect(&paths.socket(), "clusia").await?;
    client.request(Request::Shutdown).await?;
    for _ in 0..WAIT_STEPS {
        if !paths.socket().exists() {
            return Ok(Output {
                human: "Clúsia daemon stopped".into(),
                json: json!({ "stopped": true }),
            });
        }
        tokio::time::sleep(WAIT_STEP).await;
    }
    Err(CliError::Other(
        "clusiad acknowledged but did not stop within 5s".into(),
    ))
}
