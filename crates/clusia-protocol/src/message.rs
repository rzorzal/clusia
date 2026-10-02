//! Every message that crosses the socket.

use clusia_core::Config;
use serde::{Deserialize, Serialize};

/// Topics a client can subscribe to.
pub mod topics {
    pub const CONFIG: &str = "config";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Must be the first message on every connection.
    Hello {
        protocol: u32,
        client: String,
        version: String,
    },
    Request {
        id: u64,
        cmd: Command,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        protocol: u32,
        daemon: String,
    },
    /// Sent instead of `Welcome` when protocol versions differ; the daemon then closes the connection.
    Incompatible {
        daemon_protocol: u32,
        message: String,
    },
    Response {
        id: u64,
        result: Outcome,
    },
    Event {
        topic: String,
        event: Event,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    DaemonStatus,
    Shutdown,
    GetConfig,
    GetConfigValue { key: String },
    SetConfigValue { key: String, value: String },
    Subscribe { topics: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok(Reply),
    Err(ProtocolError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Ack,
    Status(DaemonStatus),
    Config(Config),
    Value(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub clients: usize,
    pub socket: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
}

impl ProtocolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    UnknownConfigKey,
    InvalidConfigValue,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    ConfigChanged { key: String, value: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire<T: Serialize>(m: &T) -> String {
        serde_json::to_string(m).unwrap()
    }

    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(m: T) {
        let back: T = serde_json::from_str(&wire(&m)).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn client_messages_wire_format() {
        let hello = ClientMessage::Hello {
            protocol: 1,
            client: "clusia".into(),
            version: "0.1.0".into(),
        };
        assert_eq!(
            wire(&hello),
            r#"{"type":"hello","protocol":1,"client":"clusia","version":"0.1.0"}"#
        );

        let status = ClientMessage::Request {
            id: 7,
            cmd: Command::DaemonStatus,
        };
        assert_eq!(
            wire(&status),
            r#"{"type":"request","id":7,"cmd":"daemon_status"}"#
        );

        let set = ClientMessage::Request {
            id: 8,
            cmd: Command::SetConfigValue {
                key: "github.poll_interval_secs".into(),
                value: "120".into(),
            },
        };
        assert_eq!(
            wire(&set),
            r#"{"type":"request","id":8,"cmd":{"set_config_value":{"key":"github.poll_interval_secs","value":"120"}}}"#
        );

        let sub = ClientMessage::Request {
            id: 9,
            cmd: Command::Subscribe {
                topics: vec![topics::CONFIG.into()],
            },
        };
        assert_eq!(
            wire(&sub),
            r#"{"type":"request","id":9,"cmd":{"subscribe":{"topics":["config"]}}}"#
        );
    }

    #[test]
    fn server_messages_wire_format() {
        let welcome = ServerMessage::Welcome {
            protocol: 1,
            daemon: "0.1.0".into(),
        };
        assert_eq!(
            wire(&welcome),
            r#"{"type":"welcome","protocol":1,"daemon":"0.1.0"}"#
        );

        let incompatible = ServerMessage::Incompatible {
            daemon_protocol: 1,
            message: "x".into(),
        };
        assert_eq!(
            wire(&incompatible),
            r#"{"type":"incompatible","daemon_protocol":1,"message":"x"}"#
        );

        let ack = ServerMessage::Response {
            id: 7,
            result: Outcome::Ok(Reply::Ack),
        };
        assert_eq!(
            wire(&ack),
            r#"{"type":"response","id":7,"result":{"ok":"ack"}}"#
        );

        let value = ServerMessage::Response {
            id: 7,
            result: Outcome::Ok(Reply::Value("120".into())),
        };
        assert_eq!(
            wire(&value),
            r#"{"type":"response","id":7,"result":{"ok":{"value":"120"}}}"#
        );

        let err = ServerMessage::Response {
            id: 9,
            result: Outcome::Err(ProtocolError::new(
                ErrorCode::UnknownConfigKey,
                "unknown config key \"x\"",
            )),
        };
        assert_eq!(
            wire(&err),
            r#"{"type":"response","id":9,"result":{"err":{"code":"unknown_config_key","message":"unknown config key \"x\""}}}"#
        );

        let status = ServerMessage::Response {
            id: 1,
            result: Outcome::Ok(Reply::Status(DaemonStatus {
                version: "0.1.0".into(),
                pid: 42,
                uptime_secs: 5,
                clients: 1,
                socket: "/tmp/c/clusiad.sock".into(),
            })),
        };
        assert_eq!(
            wire(&status),
            r#"{"type":"response","id":1,"result":{"ok":{"status":{"version":"0.1.0","pid":42,"uptime_secs":5,"clients":1,"socket":"/tmp/c/clusiad.sock"}}}}"#
        );

        let event = ServerMessage::Event {
            topic: topics::CONFIG.into(),
            event: Event::ConfigChanged {
                key: "appearance.theme".into(),
                value: "dark".into(),
            },
        };
        assert_eq!(
            wire(&event),
            r#"{"type":"event","topic":"config","event":{"config_changed":{"key":"appearance.theme","value":"dark"}}}"#
        );
    }

    #[test]
    fn every_message_round_trips() {
        round_trip(ClientMessage::Hello {
            protocol: 1,
            client: "t".into(),
            version: "0".into(),
        });
        for cmd in [
            Command::DaemonStatus,
            Command::Shutdown,
            Command::GetConfig,
            Command::GetConfigValue { key: "a.b".into() },
            Command::SetConfigValue {
                key: "a.b".into(),
                value: "1".into(),
            },
            Command::Subscribe {
                topics: vec!["config".into()],
            },
        ] {
            round_trip(ClientMessage::Request { id: 1, cmd });
        }
        for reply in [
            Reply::Ack,
            Reply::Config(Config::default()),
            Reply::Value("v".into()),
            Reply::Status(DaemonStatus {
                version: "0".into(),
                pid: 1,
                uptime_secs: 2,
                clients: 3,
                socket: "/s".into(),
            }),
        ] {
            round_trip(ServerMessage::Response {
                id: 2,
                result: Outcome::Ok(reply),
            });
        }
        round_trip(ServerMessage::Welcome {
            protocol: 1,
            daemon: "0".into(),
        });
        round_trip(ServerMessage::Incompatible {
            daemon_protocol: 1,
            message: "m".into(),
        });
        round_trip(ServerMessage::Event {
            topic: "config".into(),
            event: Event::ConfigChanged {
                key: "k".into(),
                value: "v".into(),
            },
        });
        for code in [
            ErrorCode::BadRequest,
            ErrorCode::UnknownConfigKey,
            ErrorCode::InvalidConfigValue,
            ErrorCode::Internal,
        ] {
            round_trip(ServerMessage::Response {
                id: 3,
                result: Outcome::Err(ProtocolError::new(code, "m")),
            });
        }
    }
}
