//! The name of the agent the commands talk about.

use clusia_core::config::HarnessKind;
use clusia_protocol::{Client, Command as Request, Reply};

/// The display name of the harness in Config; the default kind's when the daemon does not answer.
pub(crate) async fn name_of(client: &mut Client) -> &'static str {
    match client.request(Request::GetConfig).await {
        Ok(Reply::Config(config)) => config.harness.kind.name(),
        _ => HarnessKind::default().name(),
    }
}
