//! Command → reply. Connection handling lives in `connection`.

use std::sync::atomic::Ordering;

use clusia_protocol::{
    Command, DaemonStatus, ErrorCode, Event, Outcome, ProtocolError, Reply, topics,
};
use clusia_store::{ConfigKeyError, get_value, save_config, set_value};

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
        Command::GetConfigValue { key } => match get_value(&*shared.config.read().await, &key) {
            Ok(value) => Outcome::Ok(Reply::Value(value)),
            Err(e) => key_error(e),
        },
        Command::SetConfigValue { key, value } => set_config_value(shared, key, value).await,
    }
}

fn key_error(e: ConfigKeyError) -> Outcome {
    let code = match e {
        ConfigKeyError::Unknown(_) => ErrorCode::UnknownConfigKey,
        ConfigKeyError::Invalid { .. } => ErrorCode::InvalidConfigValue,
    };
    Outcome::Err(ProtocolError::new(code, e.to_string()))
}

async fn set_config_value(shared: &Shared, key: String, raw: String) -> Outcome {
    let mut config = shared.config.write().await;
    let updated = match set_value(&config, &key, &raw) {
        Ok(c) => c,
        Err(e) => return key_error(e),
    };
    if let Err(e) = save_config(&shared.paths, &updated) {
        return Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("could not save config.toml: {e}"),
        ));
    }
    let rendered = get_value(&updated, &key).unwrap_or(raw);
    *config = updated;
    shared.publish(
        topics::CONFIG,
        Event::ConfigChanged {
            key,
            value: rendered.clone(),
        },
    );
    Outcome::Ok(Reply::Value(rendered))
}
