//! The agent the commands talk about.

use clusia_core::PrRef;
use clusia_core::config::HarnessKind;
use clusia_protocol::{Client, Command as Request, Reply};

/// The harness `pr` runs on: the one its session started with, else the one Config names, else
/// the default kind when the daemon does not answer.
pub(crate) async fn kind_of(client: &mut Client, pr: &PrRef) -> HarnessKind {
    if let Ok(Reply::ReviewFile(review)) =
        client.request(Request::GetReview { pr: pr.clone() }).await
        && let Some(kind) = review.harness_session_kind
    {
        return kind;
    }
    match client.request(Request::GetConfig).await {
        Ok(Reply::Config(config)) => config.harness.kind,
        _ => HarnessKind::default(),
    }
}
