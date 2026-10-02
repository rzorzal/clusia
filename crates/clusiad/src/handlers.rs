//! Command → reply. Connection handling lives in `connection`.

use std::sync::atomic::Ordering;

use clusia_protocol::{Command, DaemonStatus, ErrorCode, Outcome, ProtocolError, Reply};

use crate::state::Shared;

pub(crate) async fn handle(shared: &Shared, cmd: Command) -> Outcome {
    match cmd {
        Command::DaemonStatus => Outcome::Ok(Reply::Status(DaemonStatus {
            version: crate::VERSION.to_string(),
            pid: std::process::id(),
            uptime_secs: shared.started.elapsed().as_secs(),
            clients: shared.clients.load(Ordering::SeqCst),
            socket: shared.paths.socket().display().to_string(),
        })),
        // Subscribing is tracked per connection; shutdown is triggered after the reply is sent.
        Command::Shutdown | Command::Subscribe { .. } => Outcome::Ok(Reply::Ack),
        Command::GetConfig => Outcome::Ok(Reply::Config(shared.config.read().await.clone())),
        Command::GetConfigValue { .. } | Command::SetConfigValue { .. } => Outcome::Err(
            ProtocolError::new(ErrorCode::Internal, "not implemented yet"),
        ),
    }
}
