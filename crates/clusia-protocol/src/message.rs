//! Every message that crosses the socket.

pub use clusia_core::OpenTarget;
use clusia_core::{
    ActivitySummary, ChecksSummary, Config, DraftItem, DraftKind, EventKind, FileDiff, MediaKind,
    PrConversation, PrDetail, PrFilter, PrRef, PrSummary, Review, ReviewState, Role, Side,
    ThreadRef, Verdict,
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
    /// What the tray shows and posts: `Notify` and `InboxChanged`.
    pub const TRAY: &str = "tray";
    /// The review agent's chat: every `Agent*` event and `SessionState`.
    pub const AGENT: &str = "agent";
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
    /// Download an image or GIF into the media cache and answer with its local file.
    FetchMedia {
        url: String,
    },
    /// Giphy search (`query` empty: trending), 24 results from `offset`.
    SearchGifs {
        query: String,
        offset: u32,
    },
    /// Store the Giphy API key in the Keychain.
    SetGiphyKey {
        key: Secret,
    },
    ClearGiphyKey,
    /// Whether a Giphy key is stored (the key itself never leaves the daemon).
    GiphyKeyStatus,
    /// What the first-run screen needs: the GitHub login, repository folders and agent CLIs.
    FirstRunStatus,
    /// The tray's report of what macOS lets Clúsia do; sent on start and whenever it changes.
    NotificationPermission {
        status: PermissionStatus,
    },
    /// Send a notification through the whole path, regardless of the event settings.
    TestNotification,
    /// Turn the login item on or off (rewrites its `RunAtLoad`).
    SetStartAtLogin {
        on: bool,
    },
    /// The notification inbox, newest first.
    GetInbox,
    /// Mark inbox rows seen; no ids marks them all.
    MarkInboxSeen {
        ids: Vec<String>,
    },
    /// A message to the review's agent. It runs now, waits behind the running turn, or is
    /// refused with `ErrorCode::Busy` when one is already waiting.
    AgentSend {
        pr: PrRef,
        text: String,
    },
    /// Stop the review's running turn and drop the one waiting behind it.
    AgentCancel {
        pr: PrRef,
    },
    /// Turn a suggestion into a draft item, with `body` instead of the agent's text when set.
    AcceptSuggestion {
        pr: PrRef,
        id: String,
        #[serde(default)]
        body: Option<String>,
    },
    /// Never show this suggestion again.
    DismissSuggestion {
        pr: PrRef,
        id: String,
    },
    /// Everything the agent chat of this review said, oldest first.
    GetAgentLog {
        pr: PrRef,
    },
    /// Ask the configured agent program for its version.
    HarnessProbe,
    /// The agent wants to use a tool it is not already allowed to use. Sent only by
    /// `clusiad permission-bridge`; answered with `Reply::PermissionDecision`.
    PermissionAsk {
        pr: PrRef,
        turn: u64,
        tool: String,
        input: serde_json::Value,
    },
    /// The reviewer's answer to a `PermissionRequested`. The first answer wins; a later one is
    /// refused with `ErrorCode::NotFound`.
    PermissionAnswer {
        id: String,
        answer: PermissionAnswerKind,
    },
    /// Stop allowing what a rule of this review allowed (`Bash(cargo test:*)`).
    RevokeRule {
        pr: PrRef,
        rule: String,
    },
    /// The rules this review's reviewer allowed so far.
    GetRules {
        pr: PrRef,
    },
    /// The requests of this review that still wait for an answer, for a window that was not
    /// listening when they were made.
    GetPermissions {
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
    Media(MediaFile),
    Gifs(GifPage),
    FirstRun(FirstRun),
    GiphyKeyStatus(GiphyKeyStatus),
    Inbox(Vec<InboxItem>),
    /// The id of the turn that `AgentSend` started or queued.
    AgentTurn {
        turn: u64,
    },
    AgentLog(Vec<AgentLogEntry>),
    Probe(ProbeResult),
    /// What the bridge tells the agent: run the tool, or refuse it with `message`.
    PermissionDecision {
        allow: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// The rule strings of `GetRules`, in name order.
    Rules(Vec<String>),
    /// The waiting requests of `GetPermissions`, oldest first.
    Permissions(Vec<PermissionRequest>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub clients: usize,
    pub socket: String,
    /// What the tray last reported about macOS notifications.
    #[serde(default)]
    pub notifications_permission: PermissionStatus,
}

/// Whether macOS lets Clúsia post notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionStatus {
    Allowed,
    Denied,
    /// macOS has not been asked yet.
    #[default]
    NotDetermined,
}

/// One row of the notification inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: String,
    pub kind: EventKind,
    pub pr: Option<PrRef>,
    pub title: String,
    pub body: String,
    pub at: i64,
    pub seen: bool,
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
    /// The request was understood but is not allowed (a host or file type media never loads).
    Refused,
    /// A needed setting is missing (e.g. no Giphy key).
    NotConfigured,
    /// The review's agent already has a turn running and one waiting.
    Busy,
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
    /// The Giphy key was set or removed (topic `config`).
    GiphyKeyChanged,
    /// A macOS notification for the tray to post (topic `tray`).
    Notify {
        id: String,
        title: String,
        subtitle: String,
        body: String,
        /// The id of the bundled sound to play; `None` is silent.
        sound: Option<String>,
        /// Where a click on the notification goes.
        open: OpenTarget,
        /// Break through a macOS Focus (interruption level `timeSensitive`); off is `active`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        time_sensitive: bool,
    },
    /// The number of unseen inbox rows changed (topic `tray`).
    InboxChanged {
        unseen: u32,
    },
    /// The daemon is shutting down on request (`Shutdown`); clients should close.
    Stopping,
    /// Part of the agent's answer (topic `agent`).
    AgentChunk {
        pr: PrRef,
        turn: u64,
        text: String,
    },
    /// The agent used a tool, e.g. "Read src/auth/store.rs" (topic `agent`).
    AgentToolUse {
        pr: PrRef,
        turn: u64,
        summary: String,
    },
    /// A tool the agent was not allowed to use (topic `agent`).
    AgentDenied {
        pr: PrRef,
        turn: u64,
        tool: String,
        detail: String,
    },
    /// A comment the agent suggests; it joins the draft only through `AcceptSuggestion`
    /// (topic `agent`).
    AgentSuggestion {
        pr: PrRef,
        turn: u64,
        suggestion: Suggestion,
    },
    /// The turn ended (topic `agent`).
    AgentDone {
        pr: PrRef,
        turn: u64,
        duration_ms: u64,
    },
    /// The turn failed or was interrupted (topic `agent`).
    AgentError {
        pr: PrRef,
        turn: u64,
        kind: AgentErrorKind,
        message: String,
    },
    /// Where the review's agent session stands (topic `agent`).
    SessionState {
        pr: PrRef,
        state: SessionStateKind,
    },
    /// The agent waits for the reviewer to decide (topic `agent`).
    PermissionRequested {
        id: String,
        pr: PrRef,
        turn: u64,
        tool: String,
        /// The exact command, or the path of the file.
        summary: String,
        /// Why the agent wants it, when it said.
        reason: Option<String>,
        /// What "Allow for this review" would grant; `None` when only Allow once is offered.
        prefix: Option<String>,
        /// Whether the turn runs in the sandbox.
        sandbox: bool,
        /// When the request is denied for lack of an answer, in Unix milliseconds.
        deadline: i64,
        /// An excerpt of what the tool would do (for an edit, the old and the new text), at
        /// most 2000 characters.
        detail: Option<String>,
    },
    /// A request ended, whoever or whatever ended it, or a rule or the worktree check decided
    /// it without asking (topic `agent`).
    PermissionResolved {
        id: String,
        pr: PrRef,
        tool: String,
        /// The command or the path, as in `PermissionRequested`.
        summary: String,
        outcome: PermissionOutcome,
    },
    /// The review's rules changed (topic `agent`).
    RulesChanged {
        pr: PrRef,
        rules: Vec<String>,
    },
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

/// A downloaded image in the daemon's media cache. The window reads `path` and nothing else
/// in that directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFile {
    pub path: String,
    pub kind: MediaKind,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GifItem {
    pub id: String,
    pub title: String,
    /// The small animated rendition for the picker grid.
    pub preview_url: String,
    /// The rendition inserted into the comment.
    pub url: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GifPage {
    pub items: Vec<GifItem>,
    /// Where the next page starts; `None` at the end.
    pub next_offset: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GiphyKeyStatus {
    pub configured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GithubLogin {
    SignedIn { login: String, scopes: Vec<String> },
    SignedOut,
    Error { message: String },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFolder {
    pub path: String,
    pub exists: bool,
    /// Repositories found up to two levels down.
    pub repos: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessKind {
    ClaudeCode,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Harness {
    pub kind: HarnessKind,
    pub path: Option<String>,
    pub version: Option<String>,
}

/// A review comment the agent proposes. The human accepts, edits or dismisses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    /// `sug-` and the first 12 hex digits of the hash of file, lines and body: the same
    /// suggestion always has the same id.
    pub id: String,
    /// Path relative to the repository root.
    pub file: String,
    /// A single line; `None` when the suggestion is a range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    pub body: String,
}

impl Suggestion {
    /// The line a comment on this suggestion sits on, and where its range starts: the single
    /// line when there is one, else the end of a range that has both ends. The draft item and
    /// the window's editor both anchor here.
    pub fn anchor_lines(&self) -> Option<(u32, Option<u32>)> {
        match (self.line, self.start_line, self.end_line) {
            (Some(line), _, _) => Some((line, None)),
            (None, Some(start), Some(end)) => Some((end, Some(start))),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentErrorKind {
    NotInstalled,
    NotSignedIn,
    UsageLimit,
    Crashed,
    Interrupted,
    Unparsed,
    Busy,
}

impl AgentErrorKind {
    /// The chat line for this failure when the program said nothing more specific.
    pub fn default_message(self) -> &'static str {
        match self {
            Self::NotInstalled => "Install Claude Code or set its path",
            Self::NotSignedIn => "Run `claude` once in a terminal",
            Self::UsageLimit => "Claude Code reached its usage limit",
            Self::Crashed => "Claude Code stopped unexpectedly",
            Self::Interrupted => "The turn was interrupted",
            Self::Unparsed => "Claude Code answered in a format Clúsia does not know",
            Self::Busy => "The agent is busy: wait for the current turn",
        }
    }
}

impl std::fmt::Display for AgentErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.default_message())
    }
}

/// Where a review's agent session stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStateKind {
    /// No session: nothing was asked yet, or the review ended.
    None,
    Ready,
    Running,
    /// A message is waiting for the running turn to end.
    Queued,
}

/// One line of the agent chat as the daemon logged it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentLogEntry {
    /// What the human sent.
    User {
        at: i64,
        turn: u64,
        text: String,
    },
    Text {
        at: i64,
        turn: u64,
        text: String,
    },
    ToolUse {
        at: i64,
        turn: u64,
        summary: String,
    },
    Denied {
        at: i64,
        turn: u64,
        tool: String,
        detail: String,
    },
    Suggestion {
        at: i64,
        turn: u64,
        suggestion: Suggestion,
    },
    Done {
        at: i64,
        turn: u64,
        duration_ms: u64,
    },
    Error {
        at: i64,
        turn: u64,
        kind: AgentErrorKind,
        message: String,
    },
    /// A tool request that needed the reviewer, or a rule, to decide.
    Permission {
        at: i64,
        turn: u64,
        tool: String,
        /// The command or the path.
        summary: String,
        outcome: PermissionOutcome,
    },
}

/// A permission request waiting for the reviewer: the fields of `Event::PermissionRequested`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub id: String,
    pub pr: PrRef,
    pub turn: u64,
    pub tool: String,
    pub summary: String,
    pub reason: Option<String>,
    pub prefix: Option<String>,
    pub sandbox: bool,
    /// Unix milliseconds.
    pub deadline: i64,
    pub detail: Option<String>,
}

/// What the reviewer chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAnswerKind {
    /// Allow this request only.
    Once,
    /// Allow it and everything its prefix covers for the rest of the review.
    Review,
    Deny,
}

/// How a permission request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    Allowed,
    AllowedForReview,
    Denied,
    /// Nobody answered before the deadline.
    Expired,
    /// The turn or the review ended first.
    Cancelled,
}

/// The answer to `HarnessProbe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub ok: bool,
    /// What the program printed for `--version`.
    pub version: Option<String>,
    /// The program that was run.
    pub program: String,
    pub elapsed_ms: u64,
    /// Why it failed, when `ok` is false.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirstRun {
    pub github: GithubLogin,
    pub folders: Vec<RepoFolder>,
    pub harnesses: Vec<Harness>,
}

/// What the window should show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowTarget {
    Home,
    Config,
    /// Config on one of its pages, by name (`git`, `notifications`); an unknown name shows
    /// Appearance.
    ConfigPage {
        page: String,
    },
    Review {
        pr: PrRef,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suggestion_anchors_at_its_line_first_then_at_its_range() {
        let s = |line, start_line, end_line| Suggestion {
            id: "sug-1".into(),
            file: "src/a.rs".into(),
            line,
            start_line,
            end_line,
            body: "Why?".into(),
        };
        assert_eq!(s(Some(44), None, None).anchor_lines(), Some((44, None)));
        assert_eq!(
            s(Some(44), Some(40), Some(46)).anchor_lines(),
            Some((44, None))
        );
        assert_eq!(
            s(None, Some(40), Some(44)).anchor_lines(),
            Some((44, Some(40)))
        );
        assert_eq!(s(None, Some(40), None).anchor_lines(), None);
        assert_eq!(s(None, None, Some(44)).anchor_lines(), None);
    }

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
                notifications_permission: PermissionStatus::NotDetermined,
            })),
        };
        assert_eq!(
            wire(&status),
            r#"{"type":"response","id":1,"result":{"ok":{"status":{"version":"0.1.0","pid":42,"uptime_secs":5,"clients":1,"socket":"/tmp/c/clusiad.sock","notifications_permission":"not_determined"}}}}"#
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
                notifications_permission: PermissionStatus::Allowed,
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
        assert_eq!(
            wire(&open(9, WindowTarget::ConfigPage { page: "git".into() })),
            r#"{"type":"request","id":9,"cmd":{"open_window":{"target":{"config_page":{"page":"git"}}}}}"#
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
            body: String::new(),
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
                body: String::new(),
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

    #[test]
    fn media_giphy_first_run_messages_wire_format() {
        let req = |id, cmd| wire(&ClientMessage::Request { id, cmd });
        assert_eq!(
            req(
                1,
                Command::FetchMedia {
                    url: "https://github.com/user-attachments/assets/a1".into()
                }
            ),
            r#"{"type":"request","id":1,"cmd":{"fetch_media":{"url":"https://github.com/user-attachments/assets/a1"}}}"#
        );
        assert_eq!(
            req(
                2,
                Command::SearchGifs {
                    query: "cat".into(),
                    offset: 24
                }
            ),
            r#"{"type":"request","id":2,"cmd":{"search_gifs":{"query":"cat","offset":24}}}"#
        );
        assert_eq!(
            req(3, Command::SetGiphyKey { key: "gk_x".into() }),
            r#"{"type":"request","id":3,"cmd":{"set_giphy_key":{"key":"gk_x"}}}"#
        );
        assert_eq!(
            req(4, Command::ClearGiphyKey),
            r#"{"type":"request","id":4,"cmd":"clear_giphy_key"}"#
        );
        assert_eq!(
            req(8, Command::GiphyKeyStatus),
            r#"{"type":"request","id":8,"cmd":"giphy_key_status"}"#
        );
        assert_eq!(
            wire(&ServerMessage::Response {
                id: 8,
                result: Outcome::Ok(Reply::GiphyKeyStatus(GiphyKeyStatus { configured: true })),
            }),
            r#"{"type":"response","id":8,"result":{"ok":{"giphy_key_status":{"configured":true}}}}"#
        );
        assert_eq!(
            req(5, Command::FirstRunStatus),
            r#"{"type":"request","id":5,"cmd":"first_run_status"}"#
        );
        let reply = |reply| {
            wire(&ServerMessage::Response {
                id: 6,
                result: Outcome::Ok(reply),
            })
        };
        assert_eq!(
            reply(Reply::Media(MediaFile {
                path: "/h/cache/media/ab.png".into(),
                kind: MediaKind::Png,
                bytes: 67
            })),
            r#"{"type":"response","id":6,"result":{"ok":{"media":{"path":"/h/cache/media/ab.png","kind":"png","bytes":67}}}}"#
        );
        assert_eq!(
            reply(Reply::Gifs(GifPage {
                items: vec![GifItem {
                    id: "g1".into(),
                    title: "Happy cat".into(),
                    preview_url: "https://media0.giphy.com/p.gif".into(),
                    url: "https://media0.giphy.com/d.gif".into(),
                    width: 200,
                    height: 150
                }],
                next_offset: Some(24)
            })),
            r#"{"type":"response","id":6,"result":{"ok":{"gifs":{"items":[{"id":"g1","title":"Happy cat","preview_url":"https://media0.giphy.com/p.gif","url":"https://media0.giphy.com/d.gif","width":200,"height":150}],"next_offset":24}}}}"#
        );
        assert_eq!(
            reply(Reply::FirstRun(FirstRun {
                github: GithubLogin::SignedIn {
                    login: "octo".into(),
                    scopes: vec!["repo".into()]
                },
                folders: vec![RepoFolder {
                    path: "~/Repos".into(),
                    exists: true,
                    repos: 3
                }],
                harnesses: vec![Harness {
                    kind: HarnessKind::ClaudeCode,
                    path: Some("/opt/homebrew/bin/claude".into()),
                    version: Some("2.0.1".into())
                }]
            })),
            r#"{"type":"response","id":6,"result":{"ok":{"first_run":{"github":{"state":"signed_in","login":"octo","scopes":["repo"]},"folders":[{"path":"~/Repos","exists":true,"repos":3}],"harnesses":[{"kind":"claude_code","path":"/opt/homebrew/bin/claude","version":"2.0.1"}]}}}}"#
        );
        for (github, text) in [
            (GithubLogin::SignedOut, r#"{"state":"signed_out"}"#),
            (GithubLogin::Unknown, r#"{"state":"unknown"}"#),
            (
                GithubLogin::Error {
                    message: "boom".into(),
                },
                r#"{"state":"error","message":"boom"}"#,
            ),
        ] {
            assert_eq!(wire(&github), text);
        }
        assert_eq!(
            wire(&ServerMessage::Response {
                id: 7,
                result: Outcome::Err(ProtocolError::new(ErrorCode::Refused, "svg is not shown")),
            }),
            r#"{"type":"response","id":7,"result":{"err":{"code":"refused","message":"svg is not shown"}}}"#
        );
        assert_eq!(wire(&ErrorCode::NotConfigured), r#""not_configured""#);
        assert_eq!(
            wire(&ServerMessage::Event {
                topic: topics::CONFIG.into(),
                event: Event::GiphyKeyChanged
            }),
            r#"{"type":"event","topic":"config","event":"giphy_key_changed"}"#
        );
    }

    #[test]
    fn media_giphy_first_run_messages_round_trip() {
        round_trip(Command::FetchMedia { url: "u".into() });
        round_trip(Command::SearchGifs {
            query: String::new(),
            offset: 0,
        });
        round_trip(Command::SetGiphyKey { key: "k".into() });
        round_trip(Command::ClearGiphyKey);
        round_trip(Command::FirstRunStatus);
        round_trip(Command::GiphyKeyStatus);
        round_trip(Reply::GiphyKeyStatus(GiphyKeyStatus { configured: false }));
        round_trip(Reply::Media(MediaFile {
            path: "p".into(),
            kind: MediaKind::Gif,
            bytes: 1,
        }));
        round_trip(Reply::Gifs(GifPage {
            items: vec![],
            next_offset: None,
        }));
        round_trip(Reply::FirstRun(FirstRun {
            github: GithubLogin::Unknown,
            folders: vec![],
            harnesses: vec![Harness {
                kind: HarnessKind::Codex,
                path: None,
                version: None,
            }],
        }));
        round_trip(Event::GiphyKeyChanged);
    }

    #[test]
    fn the_giphy_key_never_shows_in_debug() {
        let msg = Command::SetGiphyKey {
            key: "gk_supersecret".into(),
        };
        let dbg = format!("{msg:?}");
        assert!(!dbg.contains("supersecret"), "{dbg}");
    }

    #[test]
    fn notify_and_install_messages_wire_format() {
        let notify = ServerMessage::Event {
            topic: topics::TRAY.into(),
            event: Event::Notify {
                id: "n1".into(),
                title: "Review requested".into(),
                subtitle: "acme/widgets #7".into(),
                body: "@octo asked you to review".into(),
                sound: Some("leaf".into()),
                open: OpenTarget::Review {
                    pr: acme7(),
                    thread: Some("PRRT_1".into()),
                },
                time_sensitive: false,
            },
        };
        assert_eq!(
            wire(&notify),
            r#"{"type":"event","topic":"tray","event":{"notify":{"id":"n1","title":"Review requested","subtitle":"acme/widgets #7","body":"@octo asked you to review","sound":"leaf","open":{"review":{"pr":"acme/widgets#7","thread":"PRRT_1"}}}}}"#
        );
        let silent = Event::Notify {
            id: "n2".into(),
            title: "t".into(),
            subtitle: String::new(),
            body: "b".into(),
            sound: None,
            open: OpenTarget::Config { page: "git".into() },
            time_sensitive: false,
        };
        assert_eq!(
            wire(&silent),
            r#"{"notify":{"id":"n2","title":"t","subtitle":"","body":"b","sound":null,"open":{"config":{"page":"git"}}}}"#
        );
        let focus_override = Event::Notify {
            id: "n3".into(),
            title: "t".into(),
            subtitle: String::new(),
            body: "b".into(),
            sound: None,
            open: OpenTarget::Home { pr: None },
            time_sensitive: true,
        };
        assert_eq!(
            wire(&focus_override),
            r#"{"notify":{"id":"n3","title":"t","subtitle":"","body":"b","sound":null,"open":{"home":{}},"time_sensitive":true}}"#
        );
        let old_notify: Event = serde_json::from_str(
            r#"{"notify":{"id":"n2","title":"t","subtitle":"","body":"b","sound":null,"open":{"config":{"page":"git"}}}}"#,
        )
        .unwrap();
        assert!(matches!(
            old_notify,
            Event::Notify {
                time_sensitive: false,
                ..
            }
        ));
        assert_eq!(wire(&OpenTarget::Home { pr: None }), r#"{"home":{}}"#);
        assert_eq!(
            wire(&OpenTarget::Home { pr: Some(acme7()) }),
            r#"{"home":{"pr":"acme/widgets#7"}}"#
        );
        assert_eq!(
            wire(&OpenTarget::Review {
                pr: acme7(),
                thread: None
            }),
            r#"{"review":{"pr":"acme/widgets#7"}}"#
        );
        assert_eq!(
            wire(&Event::InboxChanged { unseen: 3 }),
            r#"{"inbox_changed":{"unseen":3}}"#
        );

        let req = |cmd| wire(&ClientMessage::Request { id: 1, cmd });
        assert_eq!(
            req(Command::NotificationPermission {
                status: PermissionStatus::NotDetermined
            }),
            r#"{"type":"request","id":1,"cmd":{"notification_permission":{"status":"not_determined"}}}"#
        );
        assert_eq!(
            req(Command::TestNotification),
            r#"{"type":"request","id":1,"cmd":"test_notification"}"#
        );
        assert_eq!(
            req(Command::SetStartAtLogin { on: false }),
            r#"{"type":"request","id":1,"cmd":{"set_start_at_login":{"on":false}}}"#
        );
        assert_eq!(
            req(Command::GetInbox),
            r#"{"type":"request","id":1,"cmd":"get_inbox"}"#
        );
        assert_eq!(
            req(Command::MarkInboxSeen {
                ids: vec!["n1".into()]
            }),
            r#"{"type":"request","id":1,"cmd":{"mark_inbox_seen":{"ids":["n1"]}}}"#
        );

        let inbox = Reply::Inbox(vec![InboxItem {
            id: "n1".into(),
            kind: EventKind::ReviewRequested,
            pr: Some(acme7()),
            title: "Review requested".into(),
            body: "b".into(),
            at: 100,
            seen: false,
        }]);
        assert_eq!(
            wire(&ServerMessage::Response {
                id: 2,
                result: Outcome::Ok(inbox)
            }),
            r#"{"type":"response","id":2,"result":{"ok":{"inbox":[{"id":"n1","kind":"review_requested","pr":"acme/widgets#7","title":"Review requested","body":"b","at":100,"seen":false}]}}}"#
        );
        for (status, text) in [
            (PermissionStatus::Allowed, r#""allowed""#),
            (PermissionStatus::Denied, r#""denied""#),
            (PermissionStatus::NotDetermined, r#""not_determined""#),
        ] {
            assert_eq!(wire(&status), text);
        }
        assert_eq!(PermissionStatus::default(), PermissionStatus::NotDetermined);
    }

    #[test]
    fn notify_and_install_messages_round_trip() {
        round_trip(Event::InboxChanged { unseen: 0 });
        round_trip(Event::Notify {
            id: "n".into(),
            title: "t".into(),
            subtitle: "s".into(),
            body: "b".into(),
            sound: Some("tick".into()),
            open: OpenTarget::Home { pr: Some(acme7()) },
            time_sensitive: false,
        });
        round_trip(Event::Notify {
            id: "n".into(),
            title: "t".into(),
            subtitle: String::new(),
            body: "b".into(),
            sound: None,
            open: OpenTarget::Home { pr: None },
            time_sensitive: true,
        });
        for cmd in [
            Command::TestNotification,
            Command::GetInbox,
            Command::SetStartAtLogin { on: true },
            Command::MarkInboxSeen { ids: vec![] },
            Command::NotificationPermission {
                status: PermissionStatus::Denied,
            },
        ] {
            round_trip(ClientMessage::Request { id: 1, cmd });
        }
    }

    #[test]
    fn older_daemons_and_caches_omit_the_new_fields() {
        let status: DaemonStatus = serde_json::from_str(
            r#"{"version":"0.1.0","pid":1,"uptime_secs":2,"clients":0,"socket":"/s"}"#,
        )
        .unwrap();
        assert_eq!(
            status.notifications_permission,
            PermissionStatus::NotDetermined
        );
        let review: clusia_core::ReviewInfo = serde_json::from_str(
            r#"{"id":1,"author":"ana","state":"APPROVED","body":"","submitted_at":null,"url":"u"}"#,
        )
        .unwrap();
        assert_eq!(review.commit_id, None);
        let with_commit = clusia_core::ReviewInfo {
            commit_id: Some("abc123".into()),
            ..review
        };
        assert!(wire(&with_commit).ends_with(r#""url":"u","commit_id":"abc123"}"#));
        let without = clusia_core::ReviewInfo {
            commit_id: None,
            ..with_commit
        };
        assert!(wire(&without).ends_with(r#""url":"u"}"#));
    }

    fn acme() -> PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    fn suggestion() -> Suggestion {
        Suggestion {
            id: "sug-0123456789ab".into(),
            file: "src/auth/store.rs".into(),
            line: Some(44),
            start_line: None,
            end_line: None,
            body: "Check the expiry.".into(),
        }
    }

    #[test]
    fn agent_messages_wire_format() {
        let request = |cmd| wire(&ClientMessage::Request { id: 4, cmd });
        assert_eq!(
            request(Command::AgentSend {
                pr: acme(),
                text: "is the expiry checked?".into()
            }),
            r#"{"type":"request","id":4,"cmd":{"agent_send":{"pr":"acme/widgets#7","text":"is the expiry checked?"}}}"#
        );
        assert_eq!(
            request(Command::AgentCancel { pr: acme() }),
            r#"{"type":"request","id":4,"cmd":{"agent_cancel":{"pr":"acme/widgets#7"}}}"#
        );
        assert_eq!(
            request(Command::AcceptSuggestion {
                pr: acme(),
                id: "sug-1".into(),
                body: None
            }),
            r#"{"type":"request","id":4,"cmd":{"accept_suggestion":{"pr":"acme/widgets#7","id":"sug-1","body":null}}}"#
        );
        assert_eq!(
            request(Command::AcceptSuggestion {
                pr: acme(),
                id: "sug-1".into(),
                body: Some("Better text".into())
            }),
            r#"{"type":"request","id":4,"cmd":{"accept_suggestion":{"pr":"acme/widgets#7","id":"sug-1","body":"Better text"}}}"#
        );
        assert_eq!(
            request(Command::DismissSuggestion {
                pr: acme(),
                id: "sug-1".into()
            }),
            r#"{"type":"request","id":4,"cmd":{"dismiss_suggestion":{"pr":"acme/widgets#7","id":"sug-1"}}}"#
        );
        assert_eq!(
            request(Command::GetAgentLog { pr: acme() }),
            r#"{"type":"request","id":4,"cmd":{"get_agent_log":{"pr":"acme/widgets#7"}}}"#
        );
        assert_eq!(
            request(Command::HarnessProbe),
            r#"{"type":"request","id":4,"cmd":"harness_probe"}"#
        );
        let old: Command =
            serde_json::from_str(r#"{"accept_suggestion":{"pr":"acme/widgets#7","id":"sug-1"}}"#)
                .unwrap();
        assert_eq!(
            old,
            Command::AcceptSuggestion {
                pr: acme(),
                id: "sug-1".into(),
                body: None
            },
            "body is optional on the wire"
        );
    }

    #[test]
    fn agent_turn_reply_wire_format() {
        assert_eq!(
            wire(&ServerMessage::Response {
                id: 4,
                result: Outcome::Ok(Reply::AgentTurn { turn: 3 }),
            }),
            r#"{"type":"response","id":4,"result":{"ok":{"agent_turn":{"turn":3}}}}"#
        );
        assert_eq!(
            wire(&ServerMessage::Response {
                id: 4,
                result: Outcome::Err(ProtocolError::new(
                    ErrorCode::Busy,
                    AgentErrorKind::Busy.default_message()
                )),
            }),
            r#"{"type":"response","id":4,"result":{"err":{"code":"busy","message":"The agent is busy: wait for the current turn"}}}"#
        );
    }

    #[test]
    fn agent_events_wire_format() {
        let event = |event| {
            wire(&ServerMessage::Event {
                topic: topics::AGENT.into(),
                event,
            })
        };
        assert_eq!(topics::AGENT, "agent");
        assert_eq!(
            event(Event::AgentChunk {
                pr: acme(),
                turn: 1,
                text: "Hi".into()
            }),
            r#"{"type":"event","topic":"agent","event":{"agent_chunk":{"pr":"acme/widgets#7","turn":1,"text":"Hi"}}}"#
        );
        assert_eq!(
            event(Event::AgentToolUse {
                pr: acme(),
                turn: 1,
                summary: "Read src/auth/store.rs".into()
            }),
            r#"{"type":"event","topic":"agent","event":{"agent_tool_use":{"pr":"acme/widgets#7","turn":1,"summary":"Read src/auth/store.rs"}}}"#
        );
        assert_eq!(
            event(Event::AgentDenied {
                pr: acme(),
                turn: 1,
                tool: "Bash".into(),
                detail: "cargo --version".into()
            }),
            r#"{"type":"event","topic":"agent","event":{"agent_denied":{"pr":"acme/widgets#7","turn":1,"tool":"Bash","detail":"cargo --version"}}}"#
        );
        assert_eq!(
            event(Event::AgentSuggestion {
                pr: acme(),
                turn: 1,
                suggestion: suggestion()
            }),
            r#"{"type":"event","topic":"agent","event":{"agent_suggestion":{"pr":"acme/widgets#7","turn":1,"suggestion":{"id":"sug-0123456789ab","file":"src/auth/store.rs","line":44,"body":"Check the expiry."}}}}"#
        );
        assert_eq!(
            event(Event::AgentDone {
                pr: acme(),
                turn: 1,
                duration_ms: 6000
            }),
            r#"{"type":"event","topic":"agent","event":{"agent_done":{"pr":"acme/widgets#7","turn":1,"duration_ms":6000}}}"#
        );
        assert_eq!(
            event(Event::AgentError {
                pr: acme(),
                turn: 2,
                kind: AgentErrorKind::NotSignedIn,
                message: "Run `claude` once in a terminal".into()
            }),
            r#"{"type":"event","topic":"agent","event":{"agent_error":{"pr":"acme/widgets#7","turn":2,"kind":"not_signed_in","message":"Run `claude` once in a terminal"}}}"#
        );
        assert_eq!(
            event(Event::SessionState {
                pr: acme(),
                state: SessionStateKind::Queued
            }),
            r#"{"type":"event","topic":"agent","event":{"session_state":{"pr":"acme/widgets#7","state":"queued"}}}"#
        );
    }

    #[test]
    fn agent_log_and_probe_wire_format() {
        let ok = |reply| {
            wire(&ServerMessage::Response {
                id: 5,
                result: Outcome::Ok(reply),
            })
        };
        assert_eq!(
            ok(Reply::AgentLog(vec![
                AgentLogEntry::User {
                    at: 100,
                    turn: 1,
                    text: "hi".into()
                },
                AgentLogEntry::Suggestion {
                    at: 101,
                    turn: 1,
                    suggestion: Suggestion {
                        line: None,
                        start_line: Some(40),
                        end_line: Some(44),
                        ..suggestion()
                    }
                },
                AgentLogEntry::Error {
                    at: 102,
                    turn: 1,
                    kind: AgentErrorKind::UsageLimit,
                    message: "limit".into()
                },
            ])),
            r#"{"type":"response","id":5,"result":{"ok":{"agent_log":[{"type":"user","at":100,"turn":1,"text":"hi"},{"type":"suggestion","at":101,"turn":1,"suggestion":{"id":"sug-0123456789ab","file":"src/auth/store.rs","start_line":40,"end_line":44,"body":"Check the expiry."}},{"type":"error","at":102,"turn":1,"kind":"usage_limit","message":"limit"}]}}}"#
        );
        assert_eq!(
            ok(Reply::Probe(ProbeResult {
                ok: true,
                version: Some("2.1.294 (Claude Code)".into()),
                program: "claude".into(),
                elapsed_ms: 410,
                error: None
            })),
            r#"{"type":"response","id":5,"result":{"ok":{"probe":{"ok":true,"version":"2.1.294 (Claude Code)","program":"claude","elapsed_ms":410,"error":null}}}}"#
        );
    }

    #[test]
    fn agent_enums_use_snake_case_and_read_aloud() {
        let kinds = [
            (AgentErrorKind::NotInstalled, "not_installed"),
            (AgentErrorKind::NotSignedIn, "not_signed_in"),
            (AgentErrorKind::UsageLimit, "usage_limit"),
            (AgentErrorKind::Crashed, "crashed"),
            (AgentErrorKind::Interrupted, "interrupted"),
            (AgentErrorKind::Unparsed, "unparsed"),
            (AgentErrorKind::Busy, "busy"),
        ];
        for (kind, name) in kinds {
            assert_eq!(wire(&kind), format!("\"{name}\""));
            assert_eq!(kind.to_string(), kind.default_message());
            assert!(!kind.default_message().is_empty());
        }
        assert_eq!(
            AgentErrorKind::NotInstalled.default_message(),
            "Install Claude Code or set its path"
        );
        for (state, name) in [
            (SessionStateKind::None, "none"),
            (SessionStateKind::Ready, "ready"),
            (SessionStateKind::Running, "running"),
            (SessionStateKind::Queued, "queued"),
        ] {
            assert_eq!(wire(&state), format!("\"{name}\""));
        }
        assert_eq!(wire(&ErrorCode::Busy), r#""busy""#);
    }

    #[test]
    fn every_agent_message_round_trips() {
        for cmd in [
            Command::AgentSend {
                pr: acme(),
                text: "t".into(),
            },
            Command::AgentCancel { pr: acme() },
            Command::AcceptSuggestion {
                pr: acme(),
                id: "s".into(),
                body: Some("b".into()),
            },
            Command::DismissSuggestion {
                pr: acme(),
                id: "s".into(),
            },
            Command::GetAgentLog { pr: acme() },
            Command::HarnessProbe,
        ] {
            round_trip(ClientMessage::Request { id: 1, cmd });
        }
        for event in [
            Event::AgentChunk {
                pr: acme(),
                turn: 1,
                text: "t".into(),
            },
            Event::AgentSuggestion {
                pr: acme(),
                turn: 1,
                suggestion: suggestion(),
            },
            Event::AgentError {
                pr: acme(),
                turn: 1,
                kind: AgentErrorKind::Crashed,
                message: "m".into(),
            },
            Event::SessionState {
                pr: acme(),
                state: SessionStateKind::None,
            },
        ] {
            round_trip(ServerMessage::Event {
                topic: topics::AGENT.into(),
                event,
            });
        }
        for reply in [
            Reply::AgentTurn { turn: 9 },
            Reply::AgentLog(vec![AgentLogEntry::Done {
                at: 1,
                turn: 1,
                duration_ms: 2,
            }]),
            Reply::Probe(ProbeResult {
                ok: false,
                version: None,
                program: "claude".into(),
                elapsed_ms: 3,
                error: Some("not found".into()),
            }),
        ] {
            round_trip(ServerMessage::Response {
                id: 2,
                result: Outcome::Ok(reply),
            });
        }
    }

    #[test]
    fn permission_messages_wire_format() {
        let request = |cmd| wire(&ClientMessage::Request { id: 4, cmd });
        assert_eq!(
            request(Command::PermissionAsk {
                pr: acme(),
                turn: 3,
                tool: "Bash".into(),
                input: serde_json::json!({"command": "cargo test"}),
            }),
            r#"{"type":"request","id":4,"cmd":{"permission_ask":{"pr":"acme/widgets#7","turn":3,"tool":"Bash","input":{"command":"cargo test"}}}}"#
        );
        assert_eq!(
            request(Command::PermissionAnswer {
                id: "perm-1".into(),
                answer: PermissionAnswerKind::Review,
            }),
            r#"{"type":"request","id":4,"cmd":{"permission_answer":{"id":"perm-1","answer":"review"}}}"#
        );
        assert_eq!(
            request(Command::RevokeRule {
                pr: acme(),
                rule: "Bash(cargo test:*)".into(),
            }),
            r#"{"type":"request","id":4,"cmd":{"revoke_rule":{"pr":"acme/widgets#7","rule":"Bash(cargo test:*)"}}}"#
        );
        assert_eq!(
            request(Command::GetRules { pr: acme() }),
            r#"{"type":"request","id":4,"cmd":{"get_rules":{"pr":"acme/widgets#7"}}}"#
        );
        assert_eq!(
            request(Command::GetPermissions { pr: acme() }),
            r#"{"type":"request","id":4,"cmd":{"get_permissions":{"pr":"acme/widgets#7"}}}"#
        );
        let reply = |reply| {
            wire(&ServerMessage::Response {
                id: 4,
                result: Outcome::Ok(reply),
            })
        };
        assert_eq!(
            reply(Reply::PermissionDecision {
                allow: true,
                message: None
            }),
            r#"{"type":"response","id":4,"result":{"ok":{"permission_decision":{"allow":true}}}}"#
        );
        assert_eq!(
            reply(Reply::PermissionDecision {
                allow: false,
                message: Some("The reviewer said no.".into())
            }),
            r#"{"type":"response","id":4,"result":{"ok":{"permission_decision":{"allow":false,"message":"The reviewer said no."}}}}"#
        );
        assert_eq!(
            reply(Reply::Rules(vec!["Edit".into()])),
            r#"{"type":"response","id":4,"result":{"ok":{"rules":["Edit"]}}}"#
        );
        assert_eq!(
            reply(Reply::Permissions(vec![PermissionRequest {
                id: "perm-1".into(),
                pr: acme(),
                turn: 3,
                tool: "Edit".into(),
                summary: "src/lib.rs".into(),
                reason: None,
                prefix: Some("Edit".into()),
                sandbox: false,
                deadline: 5,
                detail: Some("a\n→\nb".into()),
            }])),
            r#"{"type":"response","id":4,"result":{"ok":{"permissions":[{"id":"perm-1","pr":"acme/widgets#7","turn":3,"tool":"Edit","summary":"src/lib.rs","reason":null,"prefix":"Edit","sandbox":false,"deadline":5,"detail":"a\n→\nb"}]}}}"#
        );
    }

    #[test]
    fn permission_events_wire_format() {
        let event = |event| {
            wire(&ServerMessage::Event {
                topic: topics::AGENT.into(),
                event,
            })
        };
        assert_eq!(
            event(Event::PermissionRequested {
                id: "perm-1".into(),
                pr: acme(),
                turn: 3,
                tool: "Bash".into(),
                summary: "cargo test -p clusia-core".into(),
                reason: Some("To check the expiry test.".into()),
                prefix: Some("cargo test".into()),
                sandbox: true,
                deadline: 1_760_000_120_000,
                detail: None,
            }),
            r#"{"type":"event","topic":"agent","event":{"permission_requested":{"id":"perm-1","pr":"acme/widgets#7","turn":3,"tool":"Bash","summary":"cargo test -p clusia-core","reason":"To check the expiry test.","prefix":"cargo test","sandbox":true,"deadline":1760000120000,"detail":null}}}"#
        );
        assert_eq!(
            event(Event::PermissionResolved {
                id: "perm-1".into(),
                pr: acme(),
                tool: "Bash".into(),
                summary: "cargo test".into(),
                outcome: PermissionOutcome::AllowedForReview,
            }),
            r#"{"type":"event","topic":"agent","event":{"permission_resolved":{"id":"perm-1","pr":"acme/widgets#7","tool":"Bash","summary":"cargo test","outcome":"allowed_for_review"}}}"#
        );
        assert_eq!(
            event(Event::RulesChanged {
                pr: acme(),
                rules: vec!["Bash(cargo test:*)".into()],
            }),
            r#"{"type":"event","topic":"agent","event":{"rules_changed":{"pr":"acme/widgets#7","rules":["Bash(cargo test:*)"]}}}"#
        );
    }

    #[test]
    fn permission_enums_and_log_line_wire_format() {
        for (kind, text) in [
            (PermissionAnswerKind::Once, "once"),
            (PermissionAnswerKind::Review, "review"),
            (PermissionAnswerKind::Deny, "deny"),
        ] {
            assert_eq!(wire(&kind), format!("\"{text}\""));
        }
        for (outcome, text) in [
            (PermissionOutcome::Allowed, "allowed"),
            (PermissionOutcome::AllowedForReview, "allowed_for_review"),
            (PermissionOutcome::Denied, "denied"),
            (PermissionOutcome::Expired, "expired"),
            (PermissionOutcome::Cancelled, "cancelled"),
        ] {
            assert_eq!(wire(&outcome), format!("\"{text}\""));
        }
        assert_eq!(
            wire(&ServerMessage::Response {
                id: 5,
                result: Outcome::Ok(Reply::AgentLog(vec![AgentLogEntry::Permission {
                    at: 100,
                    turn: 1,
                    tool: "Bash".into(),
                    summary: "cargo test".into(),
                    outcome: PermissionOutcome::Expired,
                }]))
            }),
            r#"{"type":"response","id":5,"result":{"ok":{"agent_log":[{"type":"permission","at":100,"turn":1,"tool":"Bash","summary":"cargo test","outcome":"expired"}]}}}"#
        );
    }

    #[test]
    fn every_permission_message_round_trips() {
        for cmd in [
            Command::PermissionAsk {
                pr: acme(),
                turn: 1,
                tool: "Edit".into(),
                input: serde_json::json!({"file_path": "src/lib.rs"}),
            },
            Command::PermissionAnswer {
                id: "p".into(),
                answer: PermissionAnswerKind::Once,
            },
            Command::RevokeRule {
                pr: acme(),
                rule: "Edit".into(),
            },
            Command::GetRules { pr: acme() },
            Command::GetPermissions { pr: acme() },
        ] {
            round_trip(ClientMessage::Request { id: 1, cmd });
        }
        for event in [
            Event::PermissionRequested {
                id: "p".into(),
                pr: acme(),
                turn: 1,
                tool: "Bash".into(),
                summary: "ls".into(),
                reason: None,
                prefix: None,
                sandbox: false,
                deadline: 5,
                detail: Some("x".into()),
            },
            Event::PermissionResolved {
                id: "p".into(),
                pr: acme(),
                tool: "Edit".into(),
                summary: "src/lib.rs".into(),
                outcome: PermissionOutcome::Cancelled,
            },
            Event::RulesChanged {
                pr: acme(),
                rules: vec![],
            },
        ] {
            round_trip(ServerMessage::Event {
                topic: topics::AGENT.into(),
                event,
            });
        }
        for reply in [
            Reply::PermissionDecision {
                allow: false,
                message: Some("no".into()),
            },
            Reply::Rules(vec!["Edit".into()]),
            Reply::Permissions(Vec::new()),
        ] {
            round_trip(ServerMessage::Response {
                id: 2,
                result: Outcome::Ok(reply),
            });
        }
    }
}
