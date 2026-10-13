//! The name of the agent the commands talk about.

use clusia_protocol::{Client, Command as Request, Reply};

/// What the commands call the agent until the daemon says which one is set.
pub(crate) const DEFAULT_NAME: &str = "Claude Code";

/// The display name of the harness in Config; Claude Code's when the daemon does not answer.
pub(crate) async fn name_of(client: &mut Client) -> &'static str {
    match client.request(Request::GetConfig).await {
        Ok(Reply::Config(config)) => config.harness.kind.name(),
        _ => DEFAULT_NAME,
    }
}
