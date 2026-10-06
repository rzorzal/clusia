//! Every message that crosses the socket.

use clusia_core::{
    ActivitySummary, ChecksSummary, Config, DraftItem, DraftKind, FileDiff, PrConversation,
    PrDetail, PrFilter, PrRef, PrSummary, Review, ReviewState, Role, Side, ThreadRef, Verdict,
};
use serde::{Deserialize, Serialize};

/// Topics a client can subscribe to.
pub mod topics {
    pub const CONFIG: &str = "config";
    pub const PRS: &str = "prs";
    pub const SYNC: &str = "sync";
    pub const REVIEWS: &str = "reviews";
    /// Window requests from other launches (`OpenWindow`).
    pub const WINDOW: &str = "window";
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
    /// Stop background syncing until `ResumeSync` (not persisted across daemon restarts).
    PauseSync,
    ResumeSync,
    AuthStatus,
    /// Store a personal access token in the Keychain for the configured host.
    SetToken {
        token: Secret,
    },
    ClearToken,
    /// Fetch the PR head and create/update its worktree.
    PrepareWorktree {
        pr: PrRef,
    },
    /// Refresh everything from GitHub, prepare the worktree, and create or reopen the review.
    OpenReview {
        pr: PrRef,
    },
    /// The stored review file, without network.
    GetReview {
        pr: PrRef,
    },
    /// The review as the last successful open saw it, without network ("Open from cache").
    GetCachedReview {
        pr: PrRef,
    },
    /// Leave: saved if the draft has content, otherwise forgotten.
    CloseReview {
        pr: PrRef,
    },
    DiscardReview {
        pr: PrRef,
    },
    GetDiff {
        pr: PrRef,
    },
    GetConversation {
        pr: PrRef,
    },
    AddDraftItem {
        pr: PrRef,
        kind: DraftKind,
        anchor: Option<AnchorInput>,
        body: String,
        /// The review thread a `reply` or `resolve` points at.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<ThreadRef>,
    },
    UpdateDraftItem {
        pr: PrRef,
        id: String,
        body: String,
    },
    RemoveDraftItem {
        pr: PrRef,
        id: String,
    },
    Publish {
        pr: PrRef,
        verdict: Verdict,
        summary: String,
    },
    GetWhatsNew {
        pr: PrRef,
    },
    MarkSeen {
        pr: PrRef,
    },
    ListReviews,
    GetActivity,
    /// Ask an open window to show `target`; replies `Delivered(n)` with the number of windows
    /// that heard it (0: none is listening).
    OpenWindow {
        target: WindowTarget,
    },
    /// Open `path` at `line` in the configured editor. Only files inside the Clúsia home.
    OpenInEditor {
        path: String,
        #[serde(default)]
        line: Option<u32>,
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
    Review(Box<ReviewView>),
    ReviewFile(Box<Review>),
    Cached(Box<CachedReview>),
    Diff(Vec<FileDiff>),
    Conversation(PrConversation),
    DraftItem(DraftItem),
    Published(PublishResult),
    WhatsNew(Vec<NewsItem>),
    Reviews(Vec<ReviewSummary>),
    Activity(ActivitySummary),
    Delivered(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub clients: usize,
    pub socket: String,
}

/// A secret on the wire (e.g. a GitHub token). Serializes as a plain string; `Debug` never shows it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl std::ops::Deref for Secret {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SyncStatus {
    pub state: SyncState,
    pub last_sync_unix: Option<i64>,
    pub next_sync_unix: Option<i64>,
    pub message: Option<String>,
    /// Background syncing is paused (`PauseSync`); `SyncNow` still syncs once.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
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
    /// The PR moved under the draft.
    Conflict,
    /// E.g. no open review.
    InvalidState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    ConfigChanged {
        key: String,
        value: String,
    },
    PrsUpdated {
        assigned: usize,
        mine: usize,
    },
    SyncChanged(SyncStatus),
    LoadStep(LoadStep),
    ReviewChanged {
        pr: PrRef,
        state: ReviewState,
        items: usize,
    },
    ReviewOutdated {
        pr: PrRef,
        moved: usize,
        obsolete: usize,
    },
    WindowRequested {
        target: WindowTarget,
    },
    /// The daemon is shutting down on request (`Shutdown`); clients should close.
    Stopping,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorInput {
    pub path: String,
    pub line: u32,
    #[serde(default)]
    pub start_line: Option<u32>,
    pub side: Side,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSummary {
    pub path: String,
    pub previous_path: Option<String>,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
}

impl From<&FileDiff> for FileSummary {
    fn from(f: &FileDiff) -> Self {
        Self {
            path: f.path.clone(),
            previous_path: f.previous_path.clone(),
            status: f.status.clone(),
            additions: f.additions,
            deletions: f.deletions,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewView {
    pub review: Review,
    pub pr: PrDetail,
    pub files: Vec<FileSummary>,
    pub role: Role,
    pub worktree: Option<String>,
    pub viewer: Option<String>,
    /// Checks on the head commit; `None` when GitHub could not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<ChecksSummary>,
    /// Comments, reviews and review threads, read while opening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<PrConversation>,
    /// The files with their patches, exactly as fetched while opening.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diff: Vec<FileDiff>,
}

/// A review view rebuilt from the cache, and when its GitHub data was fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedReview {
    pub view: ReviewView,
    pub fetched_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadStepKind {
    Repo,
    Branch,
    Pr,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Running,
    Done,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadStep {
    pub pr: PrRef,
    pub step: LoadStepKind,
    pub status: StepStatus,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewsKind {
    Commits,
    Comment,
    Review,
    Checks,
    Local,
    Moved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewsItem {
    pub kind: NewsKind,
    /// Where it came from: `GitHub`, `GitHub Actions`, `You via clusia`, `Clúsia`.
    pub source: String,
    pub who: Option<String>,
    pub at: i64,
    pub summary: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub pr: PrRef,
    pub title: String,
    pub state: ReviewState,
    pub items: usize,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishResult {
    pub url: Option<String>,
    pub closed: bool,
    /// Review threads marked to resolve that GitHub left open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<String>,
    /// The review was published but closing the pull request failed (`closed` is false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_error: Option<String>,
}

/// What the window should show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowTarget {
    Home,
    Config,
    Review { pr: PrRef },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_messages_wire_format() {
        let pause = ClientMessage::Request {
            id: 4,
            cmd: Command::PauseSync,
        };
        assert_eq!(
            wire(&pause),
            r#"{"type":"request","id":4,"cmd":"pause_sync"}"#
        );
        let resume = ClientMessage::Request {
            id: 5,
            cmd: Command::ResumeSync,
        };
        assert_eq!(
            wire(&resume),
            r#"{"type":"request","id":5,"cmd":"resume_sync"}"#
        );
        let stopping = ServerMessage::Event {
            topic: topics::SYNC.into(),
            event: Event::Stopping,
        };
        assert_eq!(
            wire(&stopping),
            r#"{"type":"event","topic":"sync","event":"stopping"}"#
        );
        let paused = SyncStatus {
            paused: true,
            ..SyncStatus::default()
        };
        assert_eq!(
            serde_json::to_string(&paused).unwrap(),
            r#"{"state":"not_yet","last_sync_unix":null,"next_sync_unix":null,"message":null,"paused":true}"#
        );
        round_trip(Command::PauseSync);
        round_trip(Command::ResumeSync);
        round_trip(Event::Stopping);
        round_trip(paused);
    }

    #[test]
    fn paused_flag_is_omitted_when_false() {
        assert_eq!(
            serde_json::to_string(&SyncStatus::default()).unwrap(),
            r#"{"state":"not_yet","last_sync_unix":null,"next_sync_unix":null,"message":null}"#
        );
        let old: SyncStatus = serde_json::from_str(
            r#"{"state":"online","last_sync_unix":1,"next_sync_unix":2,"message":null}"#,
        )
        .unwrap();
        assert!(!old.paused, "old daemons are never paused");
    }

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

    #[test]
    fn m5_messages_wire_format() {
        let open = |id, target| ClientMessage::Request {
            id,
            cmd: Command::OpenWindow { target },
        };
        assert_eq!(
            wire(&open(1, WindowTarget::Home)),
            r#"{"type":"request","id":1,"cmd":{"open_window":{"target":"home"}}}"#
        );
        assert_eq!(
            wire(&open(2, WindowTarget::Review { pr: acme7() })),
            r#"{"type":"request","id":2,"cmd":{"open_window":{"target":{"review":{"pr":"acme/widgets#7"}}}}}"#
        );
        let editor = ClientMessage::Request {
            id: 3,
            cmd: Command::OpenInEditor {
                path: "/w/src/lib.rs".into(),
                line: Some(12),
            },
        };
        assert_eq!(
            wire(&editor),
            r#"{"type":"request","id":3,"cmd":{"open_in_editor":{"path":"/w/src/lib.rs","line":12}}}"#
        );
        let delivered = ServerMessage::Response {
            id: 1,
            result: Outcome::Ok(Reply::Delivered(1)),
        };
        assert_eq!(
            wire(&delivered),
            r#"{"type":"response","id":1,"result":{"ok":{"delivered":1}}}"#
        );
        let event = ServerMessage::Event {
            topic: topics::WINDOW.into(),
            event: Event::WindowRequested {
                target: WindowTarget::Config,
            },
        };
        assert_eq!(
            wire(&event),
            r#"{"type":"event","topic":"window","event":{"window_requested":{"target":"config"}}}"#
        );
    }

    #[test]
    fn m5_messages_round_trip() {
        for target in [
            WindowTarget::Home,
            WindowTarget::Config,
            WindowTarget::Review { pr: acme7() },
        ] {
            round_trip(Command::OpenWindow {
                target: target.clone(),
            });
            round_trip(Event::WindowRequested { target });
        }
        round_trip(Command::OpenInEditor {
            path: "/w/x".into(),
            line: None,
        });
        round_trip(Reply::Delivered(0));
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
                paused: false,
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
            closed: false,
            merged: false,
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

    #[test]
    fn set_token_debug_is_redacted() {
        let msg = ClientMessage::Request {
            id: 1,
            cmd: Command::SetToken {
                token: "ghp_supersecret".into(),
            },
        };
        let dbg = format!("{msg:?}");
        assert!(!dbg.contains("supersecret"), "{dbg}");
        assert!(dbg.contains("Secret(***)"));
    }

    #[test]
    fn set_token_wire_format() {
        let msg = ClientMessage::Request {
            id: 9,
            cmd: Command::SetToken {
                token: "ghp_x".into(),
            },
        };
        assert_eq!(
            wire(&msg),
            r#"{"type":"request","id":9,"cmd":{"set_token":{"token":"ghp_x"}}}"#
        );
    }

    #[test]
    fn m3_messages_wire_format() {
        let open = ClientMessage::Request {
            id: 1,
            cmd: Command::OpenReview { pr: acme7() },
        };
        assert_eq!(
            wire(&open),
            r#"{"type":"request","id":1,"cmd":{"open_review":{"pr":"acme/widgets#7"}}}"#
        );

        let add = ClientMessage::Request {
            id: 2,
            cmd: Command::AddDraftItem {
                pr: acme7(),
                kind: clusia_core::DraftKind::LineComment,
                anchor: Some(AnchorInput {
                    path: "a.rs".into(),
                    line: 3,
                    start_line: None,
                    side: clusia_core::Side::Right,
                }),
                body: "nit".into(),
                thread: None,
            },
        };
        assert_eq!(
            wire(&add),
            r#"{"type":"request","id":2,"cmd":{"add_draft_item":{"pr":"acme/widgets#7","kind":"line_comment","anchor":{"path":"a.rs","line":3,"start_line":null,"side":"right"},"body":"nit"}}}"#
        );

        let publish = ClientMessage::Request {
            id: 3,
            cmd: Command::Publish {
                pr: acme7(),
                verdict: clusia_core::Verdict::RequestChanges,
                summary: "fix".into(),
            },
        };
        assert_eq!(
            wire(&publish),
            r#"{"type":"request","id":3,"cmd":{"publish":{"pr":"acme/widgets#7","verdict":"request_changes","summary":"fix"}}}"#
        );

        let step = ServerMessage::Event {
            topic: topics::REVIEWS.into(),
            event: Event::LoadStep(LoadStep {
                pr: acme7(),
                step: LoadStepKind::Branch,
                status: StepStatus::Done,
                message: None,
            }),
        };
        assert_eq!(
            wire(&step),
            r#"{"type":"event","topic":"reviews","event":{"load_step":{"pr":"acme/widgets#7","step":"branch","status":"done","message":null}}}"#
        );

        let outdated = ServerMessage::Event {
            topic: topics::REVIEWS.into(),
            event: Event::ReviewOutdated {
                pr: acme7(),
                moved: 2,
                obsolete: 1,
            },
        };
        assert_eq!(
            wire(&outdated),
            r#"{"type":"event","topic":"reviews","event":{"review_outdated":{"pr":"acme/widgets#7","moved":2,"obsolete":1}}}"#
        );
    }

    #[test]
    fn m3_messages_round_trip() {
        let review = clusia_core::Review::new(acme7(), "Fix".into(), "b".into(), "h".into(), 1);
        for cmd in [
            Command::OpenReview { pr: acme7() },
            Command::GetReview { pr: acme7() },
            Command::CloseReview { pr: acme7() },
            Command::DiscardReview { pr: acme7() },
            Command::GetDiff { pr: acme7() },
            Command::GetConversation { pr: acme7() },
            Command::UpdateDraftItem {
                pr: acme7(),
                id: "i1".into(),
                body: "b".into(),
            },
            Command::RemoveDraftItem {
                pr: acme7(),
                id: "i1".into(),
            },
            Command::GetWhatsNew { pr: acme7() },
            Command::MarkSeen { pr: acme7() },
            Command::ListReviews,
            Command::GetActivity,
        ] {
            round_trip(ClientMessage::Request { id: 1, cmd });
        }
        let file = clusia_core::FileDiff {
            path: "a.rs".into(),
            previous_path: None,
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            patch: None,
        };
        assert_eq!(FileSummary::from(&file).path, "a.rs");
        for reply in [
            Reply::ReviewFile(Box::new(review.clone())),
            Reply::Diff(vec![file]),
            Reply::Conversation(clusia_core::PrConversation::default()),
            Reply::Published(PublishResult {
                url: Some("u".into()),
                closed: false,
                unresolved: vec![],
                close_error: None,
            }),
            Reply::WhatsNew(vec![NewsItem {
                kind: NewsKind::Commits,
                source: "GitHub".into(),
                who: Some("maria".into()),
                at: 5,
                summary: "2 new commits".into(),
                url: None,
            }]),
            Reply::Reviews(vec![ReviewSummary {
                pr: acme7(),
                title: "Fix".into(),
                state: clusia_core::ReviewState::Saved,
                items: 2,
                updated_at: 9,
            }]),
            Reply::Activity(clusia_core::ActivitySummary {
                heatmap: vec![],
                published_this_week: 0,
                published_total: 0,
                avg_review_secs: None,
            }),
        ] {
            round_trip(ServerMessage::Response {
                id: 2,
                result: Outcome::Ok(reply),
            });
        }
        for code in [ErrorCode::Conflict, ErrorCode::InvalidState] {
            round_trip(ServerMessage::Response {
                id: 3,
                result: Outcome::Err(ProtocolError::new(code, "m")),
            });
        }
        round_trip(ServerMessage::Event {
            topic: topics::REVIEWS.into(),
            event: Event::ReviewChanged {
                pr: acme7(),
                state: clusia_core::ReviewState::Active,
                items: 1,
            },
        });
    }

    fn view() -> ReviewView {
        let review = clusia_core::Review::new(acme7(), "Fix".into(), "b".into(), "h".into(), 1);
        ReviewView {
            review,
            pr: clusia_core::PrDetail {
                summary: summary(),
                base_ref: "main".into(),
                head_ref: "fix".into(),
                base_sha: "b".into(),
                head_sha: "h".into(),
                additions: 1,
                deletions: 0,
                changed_files: 1,
                clone_url: "https://github.com/acme/widgets.git".into(),
                closed: false,
                merged: false,
            },
            files: vec![],
            role: Role::Reviewer,
            worktree: None,
            viewer: Some("octo".into()),
            checks: None,
            conversation: None,
            diff: vec![],
        }
    }

    #[test]
    fn m5b_messages_wire_format() {
        let reply = ClientMessage::Request {
            id: 4,
            cmd: Command::AddDraftItem {
                pr: acme7(),
                kind: DraftKind::Reply,
                anchor: None,
                body: "Agreed.".into(),
                thread: Some(ThreadRef {
                    id: "PRRT_1".into(),
                    author: "mona".into(),
                    path: Some("src/a.rs".into()),
                    line: Some(41),
                }),
            },
        };
        assert_eq!(
            wire(&reply),
            r#"{"type":"request","id":4,"cmd":{"add_draft_item":{"pr":"acme/widgets#7","kind":"reply","anchor":null,"body":"Agreed.","thread":{"id":"PRRT_1","author":"mona","path":"src/a.rs","line":41}}}}"#
        );
        let old: Command = serde_json::from_str(
            r#"{"add_draft_item":{"pr":"acme/widgets#7","kind":"general","anchor":null,"body":"x"}}"#,
        )
        .unwrap();
        assert!(matches!(old, Command::AddDraftItem { thread: None, .. }));

        let published = PublishResult {
            url: Some("u".into()),
            closed: false,
            unresolved: vec!["PRRT_2".into()],
            close_error: None,
        };
        assert_eq!(
            wire(&published),
            r#"{"url":"u","closed":false,"unresolved":["PRRT_2"]}"#
        );
        let all_resolved = PublishResult {
            unresolved: vec![],
            ..published
        };
        assert_eq!(wire(&all_resolved), r#"{"url":"u","closed":false}"#);
        let old: PublishResult = serde_json::from_str(r#"{"url":null,"closed":true}"#).unwrap();
        assert!(old.unresolved.is_empty());
        assert_eq!(old.close_error, None);
        let close_failed = PublishResult {
            url: Some("u".into()),
            closed: false,
            unresolved: vec!["PRRT_2".into()],
            close_error: Some("GitHub said no".into()),
        };
        assert_eq!(
            wire(&close_failed),
            r#"{"url":"u","closed":false,"unresolved":["PRRT_2"],"close_error":"GitHub said no"}"#
        );
    }

    #[test]
    fn review_view_new_fields_are_optional_on_the_wire() {
        let bare = serde_json::to_value(view()).unwrap();
        for key in ["checks", "conversation", "diff"] {
            assert!(bare.get(key).is_none(), "{key} is omitted when empty");
        }
        let back: ReviewView = serde_json::from_value(bare).unwrap();
        assert_eq!(back, view());

        let full = ReviewView {
            checks: Some(clusia_core::ChecksSummary {
                total: 2,
                passed: 2,
                failed: 0,
                pending: 0,
            }),
            conversation: Some(PrConversation::default()),
            diff: vec![FileDiff {
                path: "a.rs".into(),
                previous_path: None,
                status: "modified".into(),
                additions: 1,
                deletions: 0,
                patch: Some("@@ -1 +1 @@\n-a\n+b".into()),
            }],
            ..view()
        };
        let v = serde_json::to_value(&full).unwrap();
        assert_eq!(
            v["checks"].to_string(),
            r#"{"failed":0,"passed":2,"pending":0,"total":2}"#
        );
        assert_eq!(
            v["conversation"].to_string(),
            r#"{"comments":[],"review_threads":[],"reviews":[],"threads":[]}"#
        );
        assert_eq!(
            v["diff"].to_string(),
            r#"[{"additions":1,"deletions":0,"patch":"@@ -1 +1 @@\n-a\n+b","path":"a.rs","previous_path":null,"status":"modified"}]"#
        );
        round_trip(Reply::Review(Box::new(full)));
    }

    #[test]
    fn cached_review_wire_format() {
        let get = ClientMessage::Request {
            id: 5,
            cmd: Command::GetCachedReview { pr: acme7() },
        };
        assert_eq!(
            wire(&get),
            r#"{"type":"request","id":5,"cmd":{"get_cached_review":{"pr":"acme/widgets#7"}}}"#
        );
        let cached = ServerMessage::Response {
            id: 5,
            result: Outcome::Ok(Reply::Cached(Box::new(CachedReview {
                view: view(),
                fetched_at: 9,
            }))),
        };
        let text = wire(&cached);
        assert!(
            text.starts_with(
                r#"{"type":"response","id":5,"result":{"ok":{"cached":{"view":{"review":{"#
            ),
            "{text}"
        );
        assert!(
            text.ends_with(r#""viewer":"octo"},"fetched_at":9}}}}"#),
            "{text}"
        );
        round_trip(cached);
        round_trip(Command::GetCachedReview { pr: acme7() });
    }
}
