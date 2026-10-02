//! `clusia daemon start|stop`. Auto-start for every other command lives in `clusia_protocol::launcher`.

use std::path::Path;
use std::time::Duration;

use clusia_core::Paths;
use clusia_protocol::{Client, ClientError, Command as Request, Reply, launcher};
use serde_json::json;

use crate::run::{CliError, Output};

const WAIT_STEP: Duration = Duration::from_millis(100);
const WAIT_STEPS: u32 = 50;

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
    let pid = launcher::start_daemon(paths, home).await?;
    Ok(Output {
        human: format!("Clúsia daemon started (pid {pid})"),
        json: json!({ "started": true, "pid": pid }),
    })
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
