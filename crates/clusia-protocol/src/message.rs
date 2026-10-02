//! Every message that crosses the socket.

use clusia_core::{Config, PrDetail, PrFilter, PrRef, PrSummary};
use serde::{Deserialize, Serialize};

/// Topics a client can subscribe to.
pub mod topics {
    pub const CONFIG: &str = "config";
    pub const PRS: &str = "prs";
    pub const SYNC: &str = "sync";
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
#[allow(clippy::large_enum_variant)] // PrDetail is large; boxing would change the pinned test API
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
    GetConfigValue {
        key: String,
    },
    SetConfigValue {
        key: String,
        value: String,
    },
    Subscribe {
        topics: Vec<String>,
    },
    /// Cached list; refreshed by the background sync or `SyncNow`.
    ListPrs {
        filter: PrFilter,
    },
    /// Live fetch of one pull request.
    GetPr {
        pr: PrRef,
    },
    /// Sync with GitHub now and return the resulting status.
    SyncNow,
    GetSyncStatus,
    AuthStatus,
    /// Store a personal access token in the Keychain for the configured host.
    SetToken {
        token: String,
    },
    ClearToken,
    /// Fetch the PR head and create/update its worktree.
    PrepareWorktree {
        pr: PrRef,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // PrDetail is large; boxing would change the pinned test API
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
    Prs(Vec<PrSummary>),
    Pr(PrDetail),
    Sync(SyncStatus),
    Auth(AuthInfo),
    Worktree(WorktreeInfo),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub clients: usize,
    pub socket: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SyncStatus {
    pub state: SyncState,
    pub last_sync_unix: Option<i64>,
    pub next_sync_unix: Option<i64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    #[default]
    NotYet,
    Online,
    Offline,
    RateLimited,
    Unauthorized,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthInfo {
    pub source: Option<TokenSource>,
    pub login: Option<String>,
    pub scopes: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenSource {
    Env,
    GhCli,
    Pat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    pub head_sha: String,
    /// The repository the worktree belongs to (the user's clone, or Clúsia's cache clone).
    pub clone: String,
    /// True when Clúsia had to clone because no local clone was found.
    pub cloned: bool,
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
    Unauthorized,
    NotFound,
    RateLimited,
    Offline,
    /// GitHub answered with an unexpected error.
    Upstream,
    Git,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    ConfigChanged { key: String, value: String },
    PrsUpdated { assigned: usize, mine: usize },
    SyncChanged(SyncStatus),
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

    fn acme7() -> clusia_core::PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    fn summary() -> clusia_core::PrSummary {
        clusia_core::PrSummary {
            pr: acme7(),
            title: "Fix cache".into(),
            author: "maria".into(),
            url: "https://github.com/acme/widgets/pull/7".into(),
            draft: false,
            updated_at: "2026-10-01T12:00:00Z".into(),
            comments: 3,
        }
    }

    #[test]
    fn m2_messages_wire_format() {
        let list = ClientMessage::Request {
            id: 1,
            cmd: Command::ListPrs {
                filter: clusia_core::PrFilter::Assigned,
            },
        };
        assert_eq!(
            wire(&list),
            r#"{"type":"request","id":1,"cmd":{"list_prs":{"filter":"assigned"}}}"#
        );

        let get = ClientMessage::Request {
            id: 2,
            cmd: Command::GetPr { pr: acme7() },
        };
        assert_eq!(
            wire(&get),
            r#"{"type":"request","id":2,"cmd":{"get_pr":{"pr":"acme/widgets#7"}}}"#
        );

        let sync = ClientMessage::Request {
            id: 3,
            cmd: Command::SyncNow,
        };
        assert_eq!(wire(&sync), r#"{"type":"request","id":3,"cmd":"sync_now"}"#);

        let status = ServerMessage::Response {
            id: 3,
            result: Outcome::Ok(Reply::Sync(SyncStatus {
                state: SyncState::RateLimited,
                last_sync_unix: Some(100),
                next_sync_unix: Some(160),
                message: Some("rate limit".into()),
            })),
        };
        assert_eq!(
            wire(&status),
            r#"{"type":"response","id":3,"result":{"ok":{"sync":{"state":"rate_limited","last_sync_unix":100,"next_sync_unix":160,"message":"rate limit"}}}}"#
        );

        let auth = ServerMessage::Response {
            id: 4,
            result: Outcome::Ok(Reply::Auth(AuthInfo {
                source: Some(TokenSource::GhCli),
                login: Some("octo".into()),
                scopes: vec!["repo".into()],
                error: None,
            })),
        };
        assert_eq!(
            wire(&auth),
            r#"{"type":"response","id":4,"result":{"ok":{"auth":{"source":"gh-cli","login":"octo","scopes":["repo"],"error":null}}}}"#
        );

        let updated = ServerMessage::Event {
            topic: topics::PRS.into(),
            event: Event::PrsUpdated {
                assigned: 2,
                mine: 1,
            },
        };
        assert_eq!(
            wire(&updated),
            r#"{"type":"event","topic":"prs","event":{"prs_updated":{"assigned":2,"mine":1}}}"#
        );

        let err = ServerMessage::Response {
            id: 5,
            result: Outcome::Err(ProtocolError::new(ErrorCode::Unauthorized, "m")),
        };
        assert_eq!(
            wire(&err),
            r#"{"type":"response","id":5,"result":{"err":{"code":"unauthorized","message":"m"}}}"#
        );
    }

    #[test]
    fn m2_messages_round_trip() {
        for cmd in [
            Command::ListPrs {
                filter: clusia_core::PrFilter::Mine,
            },
            Command::GetPr { pr: acme7() },
            Command::SyncNow,
            Command::GetSyncStatus,
            Command::AuthStatus,
            Command::SetToken { token: "t".into() },
            Command::ClearToken,
            Command::PrepareWorktree { pr: acme7() },
        ] {
            round_trip(ClientMessage::Request { id: 1, cmd });
        }
        let detail = clusia_core::PrDetail {
            summary: summary(),
            base_ref: "main".into(),
            head_ref: "fix".into(),
            base_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
            additions: 10,
            deletions: 2,
            changed_files: 3,
            clone_url: "https://github.com/acme/widgets.git".into(),
        };
        for reply in [
            Reply::Prs(vec![summary()]),
            Reply::Pr(detail),
            Reply::Sync(SyncStatus::default()),
            Reply::Auth(AuthInfo {
                source: None,
                login: None,
                scopes: vec![],
                error: Some("no token".into()),
            }),
            Reply::Worktree(WorktreeInfo {
                path: "/w".into(),
                head_sha: "b".repeat(40),
                clone: "/c".into(),
                cloned: false,
            }),
        ] {
            round_trip(ServerMessage::Response {
                id: 2,
                result: Outcome::Ok(reply),
            });
        }
        for code in [
            ErrorCode::Unauthorized,
            ErrorCode::NotFound,
            ErrorCode::RateLimited,
            ErrorCode::Offline,
            ErrorCode::Upstream,
            ErrorCode::Git,
        ] {
            round_trip(ServerMessage::Response {
                id: 3,
                result: Outcome::Err(ProtocolError::new(code, "m")),
            });
        }
        round_trip(ServerMessage::Event {
            topic: topics::SYNC.into(),
            event: Event::SyncChanged(SyncStatus {
                state: SyncState::Online,
                ..SyncStatus::default()
            }),
        });
        assert_eq!(SyncStatus::default().state, SyncState::NotYet);
    }
}
