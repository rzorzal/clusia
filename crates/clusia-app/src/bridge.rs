//! The window's only link to the world (spec §3.1). One daemon connection runs on a background
//! thread with its own current-thread tokio runtime:
//! - `Tell`s go to Bevy over a crossbeam channel, and every one wakes the reactive event loop;
//! - `Ask`s come back over a tokio channel.
//!
//! `OpenReview`, `OpenCached`, `Publish` and `FetchMedia` run on a second, short-lived connection
//! (a task on the same runtime), so the main one keeps streaming `LoadStep` events while they
//! wait.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::window::RequestRedraw;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};
use clusia_calls::{CallGraph, StaticStep};
use clusia_core::checks::{CheckKind, CheckResult, Finding, Pass};
use clusia_core::diagram::{DiagramVersion, DiagramVersionInfo, EntryState};
use clusia_core::draft::Origin;
use clusia_core::revalidation::{self, Revalidation};
use clusia_core::story::StoryVersion;
use clusia_core::tests::{
    Approve, DiskReport, StepState, TestCaseResult, TestEngineInfo, TestRun, TestScope, TestsView,
};
use clusia_core::{
    Anchor, DraftKind, Paths, PrConversation, PrFilter, PrRef, Profile, Review, Side, ThreadRef,
    Verdict,
};
use clusia_protocol::{
    AgentErrorKind, AgentLogEntry, AnchorInput, CallsIndexState, CheckState, CheckStatus, Client,
    ClientError, Command, DiagramCode, DiagramsState, ErrorCode, Event, FunctionHit, GifPage,
    LoadStep, LoadStepKind, MediaFile, NewsItem, PermissionAnswerKind, PermissionOutcome,
    ProbeResult, ProtocolError, PublishResult, Reply, ReviewView, Secret, SessionStateKind,
    StepStatus, StoryState, StoryVersionInfo, Suggestion, WindowTarget, topics,
};
use crossbeam_channel::{Receiver, Sender};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::Mode;
use crate::clock::Clock;
use crate::fixture;
use crate::review_state::{self, ReviewEvent, ReviewTabs};
use crate::screens::review::agent::{Chats, PermissionQueue};
use crate::screens::review::calls::{Calls, CallsRequest};
use crate::screens::review::checks::{self, Checks, proposed_comment};
use crate::screens::review::diagrams::{Diagrams, modal};
use crate::screens::review::revalidation::Revalidations;
use crate::screens::review::story::model::Stories;
use crate::screens::review::tests_tab::{TestMachine, TestsModels};
use crate::snapshot::{self, GiphyKey, Refresh, Snapshot};
use crate::tab_memory::TabsRestored;
use crate::ui::media::MediaCache;
use crate::ui::people::People;

/// Events arriving within this window share one refresh.
pub const COALESCE: Duration = Duration::from_millis(100);
pub const TOAST_SECS: f64 = 4.0;
/// What a write ask says when the daemon is not there to take it.
pub const NOT_SENT: &str = "Not connected to clusiad — not sent";
/// What a tests write ask says in demo mode, where nothing reaches a daemon.
const DEMO_NOT_SENT: &str = "Demo mode: nothing is sent to the daemon";

/// The `Model.rejected` key under which a refusal Giphy gave for the stored key is kept.
pub const GIPHY_KEY_REFUSAL: &str = "giphy.key";

/// The config key `Ask::SetStartAtLogin` writes, and the `Model.rejected` key for its refusal.
pub const START_AT_LOGIN: &str = "general.start_at_login";

#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    SetConfig {
        key: String,
        value: String,
    },
    SetToken(Secret),
    ClearToken,
    SyncNow,
    RefreshAuth,
    /// The first-run screen is gone: stop asking for its status.
    FirstRunDone,
    /// The key goes straight to the Keychain; `GiphyKeyChanged` refreshes the snapshot.
    SetGiphyKey(Secret),
    ClearGiphyKey,
    OpenInEditor {
        path: String,
        line: Option<u32>,
    },
    /// After `Tell::Lost`: connect again.
    Reconnect,
    /// Worker connection: the cache first (`OpenedFromCache`), then `OpenReview` and
    /// `GetWhatsNew` (`Opened` or `OpenFailed`).
    OpenReview(PrRef),
    LoadConversation(PrRef),
    MarkSeen(PrRef),
    /// Answered by `Saved` or `Refused` with the same ticket.
    AddItem {
        pr: PrRef,
        kind: DraftKind,
        anchor: Option<AnchorInput>,
        thread: Option<ThreadRef>,
        body: String,
        ticket: u64,
    },
    UpdateItem {
        pr: PrRef,
        id: String,
        body: String,
        ticket: u64,
    },
    RemoveItem {
        pr: PrRef,
        id: String,
    },
    /// Worker connection: `Published` or `PublishFailed`.
    Publish {
        pr: PrRef,
        verdict: Verdict,
        summary: String,
    },
    /// Leave the review (saved when the draft has items): `Left`.
    CloseReview(PrRef),
    /// The window's tabs now, kept by the daemon so they come back with the window.
    SetOpenTabs(clusia_core::OpenTabs),
    Discard(PrRef),
    /// Worker connection: `Tell::Media` with this URL.
    FetchMedia(String),
    /// Worker connection: `Tell::Profile` with this login.
    FetchProfile(String),
    /// Giphy search (empty query: trending), answered by `Tell::Gifs`.
    SearchGifs {
        query: String,
        offset: u32,
    },
    /// Saves the setting and the login agent together; a refusal comes back as `Tell::Rejected`
    /// for `general.start_at_login`.
    SetStartAtLogin {
        on: bool,
    },
    /// Asks the tray to post one notification now.
    TestNotification,
    /// Reads the daemon's status again (the notification permission).
    RefreshStatus,
    /// A question for the review's agent. A refusal comes back as `AgentTell::Refused`.
    AgentSend {
        pr: PrRef,
        text: String,
    },
    /// Stops the running turn.
    AgentCancel {
        pr: PrRef,
    },
    /// Turns a suggestion into a draft item (`body`: the edited text, else the suggestion's own).
    AcceptSuggestion {
        pr: PrRef,
        id: String,
        body: Option<String>,
    },
    DismissSuggestion {
        pr: PrRef,
        id: String,
    },
    /// The review's chat log: `Tell::AgentLog`.
    AgentLog {
        pr: PrRef,
    },
    /// Tests the harness on a worker connection: `Tell::Probe`.
    Probe,
    /// The reviewer's answer to a permission request (first answer wins; a late one is ignored).
    PermissionAnswer {
        id: String,
        answer: PermissionAnswerKind,
    },
    /// Takes a rule granted for this review back.
    RevokeRule {
        pr: PrRef,
        rule: String,
    },
    /// Starts a check (or starts it again); the state comes back as `ChecksTell::State`.
    RunCheck {
        pr: PrRef,
        kind: CheckKind,
    },
    StopCheck {
        pr: PrRef,
        kind: CheckKind,
    },
    /// Turns a finding into a draft item (`body`: the edited comment, else the finding's own).
    AcceptFinding {
        pr: PrRef,
        id: String,
        body: Option<String>,
    },
    DismissFinding {
        pr: PrRef,
        id: String,
    },
    /// Both checks' results and states, and which findings were accepted or dismissed:
    /// `ChecksTell::Loaded`.
    GetChecks(PrRef),
    /// The comments hidden in the review: `Tell::Hidden`.
    GetHiddenComments(PrRef),
    /// Hides the comment or thread `key`, which holds `posts` posts now, or shows it again
    /// (`None`). The daemon keeps it on this machine; the set comes back as `Tell::Hidden`.
    SetCommentHidden {
        pr: PrRef,
        key: String,
        posts: Option<u32>,
    },
    /// Shows every comment in `keys` again: one change, one `Tell::Hidden`.
    ShowComments {
        pr: PrRef,
        keys: Vec<String>,
    },
    /// Runs the revalidation of the review's draft now (a round starts when the head moved and
    /// none exists); the state comes back as `RevalidationTell::Round`.
    Revalidate(PrRef),
    StopRevalidation(PrRef),
    /// The round of the review: `RevalidationTell::Round`.
    GetRevalidation(PrRef),
    /// Accepts or rejects the agent's proposal for one draft item. The refreshed review comes
    /// back as `Tell::ReviewFile`, then `RevalidationTell::Settled` or `Refused`.
    DecideProposal {
        pr: PrRef,
        item: String,
        accept: bool,
    },
    AcceptAllProposals(PrRef),
    /// Points a draft item at another line (`anchor: None`: makes it a general comment). A
    /// refusal comes back as a warning and `RevalidationTell::Refused`; success as `Reanchored`.
    ReanchorDraftItem {
        pr: PrRef,
        id: String,
        anchor: Option<AnchorInput>,
    },
    /// Plans and draws the review's diagrams; the progress comes back as `DiagramsTell`s. A
    /// second draw while one runs is refused with a warning.
    DrawDiagrams(PrRef),
    /// Draws one entry of the latest version again (a retry or a redraw).
    DrawDiagram {
        pr: PrRef,
        key: String,
    },
    StopDiagrams(PrRef),
    /// The versions and the latest drawing: `DiagramsTell::Loaded`.
    GetDiagrams(PrRef),
    /// One older version, whole: `DiagramsTell::Version`.
    GetDiagramVersion {
        pr: PrRef,
        number: u32,
    },
    /// The code behind a box at its version's commit: `DiagramsTell::Code`.
    ReadDiagramCode {
        pr: PrRef,
        number: u32,
        key: String,
        node: String,
    },
    /// A comment on a box, into the draft. Answered by `Saved` or `Refused` with the same
    /// ticket, like `AddItem`.
    AddDiagramComment {
        pr: PrRef,
        number: u32,
        key: String,
        node: String,
        body: String,
        ticket: u64,
    },
    /// Writes the review's Story with the agent; the progress comes back as `StoryTell`s.
    WriteStory(PrRef),
    /// Writes it again for the newest commit, keeping the old versions.
    RewriteStory(PrRef),
    StopStory(PrRef),
    /// The versions, the latest one, the static order and the read marks: `StoryTell::Loaded`.
    GetStory(PrRef),
    /// One older version, whole: `StoryTell::Version`.
    GetStoryVersion {
        pr: PrRef,
        number: u32,
    },
    /// Marks or unmarks every hunk of a step; a refusal comes back as `StoryTell::ReadRefused`.
    MarkStepRead {
        pr: PrRef,
        number: u32,
        step: String,
        read: bool,
    },
    /// A comment on a step, into the draft. Answered by `Saved` or `Refused` with the ticket.
    AddStepComment {
        pr: PrRef,
        number: u32,
        step: String,
        body: String,
        ticket: u64,
    },
    /// The graph around `center` (`None`: the first changed function): `CallsTell::Graph`.
    GetCalls {
        pr: PrRef,
        center: Option<String>,
        depth: Option<u8>,
        expand: Vec<i8>,
    },
    /// Up to 20 functions whose name holds `query`: `CallsTell::Found`.
    FindFunction {
        pr: PrRef,
        query: String,
    },
    /// The code of a function at the index's commit: `CallsTell::Code`.
    ReadFunctionCode {
        pr: PrRef,
        function: String,
    },
    /// A comment on a node, into the draft. Answered by `Saved` or `Refused` with the ticket.
    AddCallsComment {
        pr: PrRef,
        function: String,
        body: String,
        ticket: u64,
    },
    /// The tests of a review as the daemon sees them now: `TestsTell::Loaded`.
    GetTests(PrRef),
    /// Probes the environment and has the agent write the recipe; the view follows on topic `tests`.
    GenerateTests(PrRef),
    /// Sets up what `approve` names, for the plan that hashes to `recipe_hash`.
    PrepareTests {
        pr: PrRef,
        recipe_hash: String,
        approve: Approve,
    },
    /// Starts a run: `TestsTell::RunStarted`.
    RunTests {
        pr: PrRef,
        scope: TestScope,
    },
    /// Stops what the review's tests are doing.
    StopTests(PrRef),
    /// Lists the repository's tests: `TestsTell::Listed`.
    ListTests(PrRef),
    /// Writes the recipe the agent proposed over the reviewer's file: `TestsTell::Loaded`.
    AcceptRecipe(PrRef),
    /// What the test runs keep on this machine: `Tell::TestDisk`.
    GetTestDisk,
    /// Removes what the test runs keep of `repo`, or of every repository: `Tell::TestDisk`.
    CleanUpTests {
        repo: Option<String>,
    },
    /// The engine, the sandbox and the cores now: `Tell::TestEngine`.
    CheckTestEngine {
        pr: Option<PrRef>,
    },
}

/// Which tests ask a refusal or a click belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestsAskKind {
    Generate,
    Prepare,
    Run,
    Stop,
    List,
    Accept,
}

/// What the tests of a review did, from the daemon's events and the answers to the tests asks.
#[derive(Debug, Clone, PartialEq)]
pub enum TestsTell {
    Loaded {
        pr: PrRef,
        view: Box<TestsView>,
    },
    Step {
        pr: PrRef,
        step: String,
        state: StepState,
        tail: Option<String>,
    },
    Output {
        pr: PrRef,
        run: String,
        tail: String,
    },
    Result {
        pr: PrRef,
        run: String,
        result: TestCaseResult,
    },
    RunDone {
        pr: PrRef,
        run: Box<TestRun>,
    },
    RunStarted {
        pr: PrRef,
        run: String,
    },
    Listed {
        pr: PrRef,
        count: u32,
    },
    /// A write that did not go through: the button is free again.
    Refused {
        pr: PrRef,
        ask: TestsAskKind,
        message: String,
    },
}

impl TestsTell {
    pub fn pr(&self) -> &PrRef {
        match self {
            TestsTell::Loaded { pr, .. }
            | TestsTell::Step { pr, .. }
            | TestsTell::Output { pr, .. }
            | TestsTell::Result { pr, .. }
            | TestsTell::RunDone { pr, .. }
            | TestsTell::RunStarted { pr, .. }
            | TestsTell::Listed { pr, .. }
            | TestsTell::Refused { pr, .. } => pr,
        }
    }

    /// The tell for a tests event; `None` for every other event.
    pub fn from_event(event: &Event) -> Option<TestsTell> {
        Some(match event.clone() {
            Event::TestEnv { pr, view } => TestsTell::Loaded { pr, view },
            Event::TestStep {
                pr,
                step,
                state,
                tail,
            } => TestsTell::Step {
                pr,
                step,
                state,
                tail,
            },
            Event::TestOutput { pr, run, tail } => TestsTell::Output { pr, run, tail },
            Event::TestResult { pr, run, result } => TestsTell::Result { pr, run, result },
            Event::TestRunDone { pr, run } => TestsTell::RunDone { pr, run },
            _ => return None,
        })
    }
}

/// What the agent did, for the review's chat. Built from the daemon's agent events, or by the
/// bridge when an ask could not reach the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentTell {
    Chunk {
        pr: PrRef,
        turn: u64,
        text: String,
    },
    ToolUse {
        pr: PrRef,
        turn: u64,
        summary: String,
    },
    Denied {
        pr: PrRef,
        turn: u64,
        tool: String,
        detail: String,
    },
    Suggestion {
        pr: PrRef,
        turn: u64,
        suggestion: Suggestion,
    },
    Done {
        pr: PrRef,
        turn: u64,
        duration_ms: u64,
    },
    Error {
        pr: PrRef,
        turn: u64,
        kind: AgentErrorKind,
        message: String,
    },
    State {
        pr: PrRef,
        state: SessionStateKind,
    },
    /// A send that did not start a turn (queue full, no review, not connected).
    Refused {
        pr: PrRef,
        message: String,
    },
    /// An accept (`accepted`) or a dismiss went through.
    Handled {
        pr: PrRef,
        id: String,
        accepted: bool,
    },
}

impl AgentTell {
    pub fn pr(&self) -> &PrRef {
        match self {
            AgentTell::Chunk { pr, .. }
            | AgentTell::ToolUse { pr, .. }
            | AgentTell::Denied { pr, .. }
            | AgentTell::Suggestion { pr, .. }
            | AgentTell::Done { pr, .. }
            | AgentTell::Error { pr, .. }
            | AgentTell::State { pr, .. }
            | AgentTell::Refused { pr, .. }
            | AgentTell::Handled { pr, .. } => pr,
        }
    }

    /// The tell for an agent event; `None` for every other event.
    pub fn from_event(event: &Event) -> Option<AgentTell> {
        Some(match event.clone() {
            Event::AgentChunk { pr, turn, text } => AgentTell::Chunk { pr, turn, text },
            Event::AgentToolUse { pr, turn, summary } => AgentTell::ToolUse { pr, turn, summary },
            Event::AgentDenied {
                pr,
                turn,
                tool,
                detail,
            } => AgentTell::Denied {
                pr,
                turn,
                tool,
                detail,
            },
            Event::AgentSuggestion {
                pr,
                turn,
                suggestion,
            } => AgentTell::Suggestion {
                pr,
                turn,
                suggestion,
            },
            Event::AgentDone {
                pr,
                turn,
                duration_ms,
            } => AgentTell::Done {
                pr,
                turn,
                duration_ms,
            },
            Event::AgentError {
                pr,
                turn,
                kind,
                message,
            } => AgentTell::Error {
                pr,
                turn,
                kind,
                message,
            },
            Event::SessionState { pr, state } => AgentTell::State { pr, state },
            _ => return None,
        })
    }
}

/// A permission request as the daemon published it (also what `GetPermissions` lists).
pub use clusia_protocol::PermissionRequest;

/// What the daemon says about permissions, for the modal, the chat and the rules list.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionTell {
    Requested(PermissionRequest),
    Resolved {
        id: String,
        pr: PrRef,
        /// What was asked, so the chat can say it even for a request this window never queued.
        tool: String,
        summary: String,
        outcome: PermissionOutcome,
        /// Who asked: a check's requests are not part of the chat.
        origin: clusia_protocol::TurnOrigin,
    },
    /// The rules granted for the review, in full.
    Rules {
        pr: PrRef,
        rules: Vec<String>,
    },
    /// The requests still waiting for `pr`, as `GetPermissions` lists them.
    Pending {
        pr: PrRef,
        requests: Vec<PermissionRequest>,
    },
    /// The daemon refused the answer for `id` (and it is still waiting): it may be given again.
    AnswerRefused {
        id: String,
    },
}

impl PermissionTell {
    /// The tell for a permission event; `None` for every other event.
    pub fn from_event(event: &Event) -> Option<PermissionTell> {
        Some(match event.clone() {
            Event::PermissionRequested {
                id,
                pr,
                turn,
                tool,
                summary,
                reason,
                prefix,
                sandbox,
                deadline,
                detail,
                origin,
            } => PermissionTell::Requested(PermissionRequest {
                id,
                pr,
                turn,
                tool,
                summary,
                reason,
                prefix,
                sandbox,
                deadline,
                detail,
                origin,
            }),
            Event::PermissionResolved {
                id,
                pr,
                tool,
                summary,
                outcome,
                origin,
            } => PermissionTell::Resolved {
                id,
                pr,
                tool,
                summary,
                outcome,
                origin,
            },
            Event::RulesChanged { pr, rules } => PermissionTell::Rules { pr, rules },
            _ => return None,
        })
    }
}

/// What the checks of a review did, from the daemon's events and from `GetChecks`.
#[derive(Debug, Clone, PartialEq)]
pub enum ChecksTell {
    Loaded {
        pr: PrRef,
        results: Vec<CheckResult>,
        states: Vec<CheckStatus>,
        accepted: Vec<String>,
        dismissed: Vec<String>,
    },
    State {
        pr: PrRef,
        kind: CheckKind,
        state: CheckState,
    },
    Finding {
        pr: PrRef,
        kind: CheckKind,
        finding: Finding,
    },
    Pass {
        pr: PrRef,
        kind: CheckKind,
        pass: Pass,
    },
    Done {
        pr: PrRef,
        kind: CheckKind,
        result: CheckResult,
    },
    /// A finding was accepted into the draft (`accepted`) or dismissed.
    Settled {
        pr: PrRef,
        id: String,
        accepted: bool,
    },
    /// An accept or a dismiss that did not go through: the card is free again.
    Refused { pr: PrRef, id: String },
}

impl ChecksTell {
    pub fn pr(&self) -> &PrRef {
        match self {
            ChecksTell::Loaded { pr, .. }
            | ChecksTell::State { pr, .. }
            | ChecksTell::Finding { pr, .. }
            | ChecksTell::Pass { pr, .. }
            | ChecksTell::Done { pr, .. }
            | ChecksTell::Settled { pr, .. }
            | ChecksTell::Refused { pr, .. } => pr,
        }
    }

    /// The tell for a check event; `None` for every other event.
    pub fn from_event(event: &Event) -> Option<ChecksTell> {
        Some(match event.clone() {
            Event::CheckState { pr, kind, state } => ChecksTell::State { pr, kind, state },
            Event::CheckFinding { pr, kind, finding } => ChecksTell::Finding { pr, kind, finding },
            Event::CheckPass { pr, kind, pass } => ChecksTell::Pass { pr, kind, pass },
            Event::CheckDone { pr, kind, result } => ChecksTell::Done { pr, kind, result },
            Event::FindingSettled { pr, id, accepted } => ChecksTell::Settled { pr, id, accepted },
            _ => return None,
        })
    }
}

/// What the diagrams of a review did, from the daemon's events and from `GetDiagrams`.
#[derive(Debug, Clone, PartialEq)]
pub enum DiagramsTell {
    /// Everything the daemon holds: the light list of versions and the latest one whole.
    Loaded {
        pr: PrRef,
        versions: Vec<DiagramVersionInfo>,
        latest: Option<Box<DiagramVersion>>,
        state: DiagramsState,
        head: String,
        stale: bool,
        stale_reason: Option<String>,
    },
    /// An older version, whole.
    Version {
        pr: PrRef,
        version: Box<DiagramVersion>,
    },
    /// The daemon could not send an older version.
    Unreadable {
        pr: PrRef,
        number: u32,
    },
    State {
        pr: PrRef,
        state: DiagramsState,
    },
    /// A new version, its entries waiting.
    Planned {
        pr: PrRef,
        version: Box<DiagramVersion>,
    },
    Entry {
        pr: PrRef,
        number: u32,
        key: String,
        state: EntryState,
    },
    /// The code behind a box, or why there is none (`DiagramCode.unavailable`).
    Code {
        pr: PrRef,
        number: u32,
        key: String,
        node: String,
        code: DiagramCode,
    },
}

impl DiagramsTell {
    pub fn pr(&self) -> &PrRef {
        match self {
            DiagramsTell::Loaded { pr, .. }
            | DiagramsTell::Version { pr, .. }
            | DiagramsTell::Unreadable { pr, .. }
            | DiagramsTell::State { pr, .. }
            | DiagramsTell::Planned { pr, .. }
            | DiagramsTell::Entry { pr, .. }
            | DiagramsTell::Code { pr, .. } => pr,
        }
    }

    /// The tell for a diagrams event; `None` for every other event.
    pub fn from_event(event: &Event) -> Option<DiagramsTell> {
        Some(match event.clone() {
            Event::DiagramsChanged { pr, state } => DiagramsTell::State { pr, state },
            Event::DiagramsPlanned { pr, version } => DiagramsTell::Planned { pr, version },
            Event::DiagramEntryChanged {
                pr,
                number,
                key,
                state,
            } => DiagramsTell::Entry {
                pr,
                number,
                key,
                state,
            },
            _ => return None,
        })
    }
}

/// What the Story of a review did, from the daemon's events and from `GetStory`.
#[derive(Debug, Clone, PartialEq)]
pub enum StoryTell {
    /// Everything the daemon holds: the light list of versions, the latest one whole, the static
    /// order and the read marks.
    Loaded {
        pr: PrRef,
        versions: Vec<StoryVersionInfo>,
        latest: Option<Box<StoryVersion>>,
        state: StoryState,
        static_steps: Vec<StaticStep>,
        read: Vec<String>,
        head: String,
        new_commits: u32,
    },
    /// An older version, whole.
    Version {
        pr: PrRef,
        version: Box<StoryVersion>,
    },
    /// The daemon could not send an older version.
    Unreadable {
        pr: PrRef,
        number: u32,
    },
    State {
        pr: PrRef,
        state: StoryState,
    },
    /// The daemon refused a read mark: the optimistic mark is undone.
    ReadRefused {
        pr: PrRef,
        step: String,
        read: bool,
    },
}

impl StoryTell {
    pub fn pr(&self) -> &PrRef {
        match self {
            StoryTell::Loaded { pr, .. }
            | StoryTell::Version { pr, .. }
            | StoryTell::Unreadable { pr, .. }
            | StoryTell::State { pr, .. }
            | StoryTell::ReadRefused { pr, .. } => pr,
        }
    }

    /// The tell for a story event; `None` for every other event.
    pub fn from_event(event: &Event) -> Option<StoryTell> {
        match event.clone() {
            Event::StoryChanged { pr, state } => Some(StoryTell::State { pr, state }),
            _ => None,
        }
    }
}

/// What the call graph of a review did, from the daemon's events and from `GetCalls`.
#[derive(Debug, Clone, PartialEq)]
pub enum CallsTell {
    /// The graph the daemon drew for `request`; none while the index is not ready.
    Graph {
        pr: PrRef,
        request: CallsRequest,
        graph: Option<Box<CallGraph>>,
        index: CallsIndexState,
        unsupported: Vec<String>,
    },
    /// The functions `FindFunction` found for `query`.
    Found {
        pr: PrRef,
        query: String,
        hits: Vec<FunctionHit>,
    },
    /// The code of a function, or why there is none (`DiagramCode.unavailable`).
    Code {
        pr: PrRef,
        function: String,
        code: DiagramCode,
    },
    Index {
        pr: PrRef,
        state: CallsIndexState,
    },
}

impl CallsTell {
    pub fn pr(&self) -> &PrRef {
        match self {
            CallsTell::Graph { pr, .. }
            | CallsTell::Found { pr, .. }
            | CallsTell::Code { pr, .. }
            | CallsTell::Index { pr, .. } => pr,
        }
    }

    /// The tell for a calls event; `None` for every other event.
    pub fn from_event(event: &Event) -> Option<CallsTell> {
        match event.clone() {
            Event::CallsIndexChanged { pr, state } => Some(CallsTell::Index { pr, state }),
            _ => None,
        }
    }
}

/// What the revalidation of a review did, from the daemon's events and from
/// `GetRevalidation`.
#[derive(Debug, Clone, PartialEq)]
pub enum RevalidationTell {
    /// The round as the daemon holds it now (`None`: there is none).
    Round {
        pr: PrRef,
        round: Option<Revalidation>,
    },
    /// A decision went through (`item: None`: *Accept all*): its buttons are free again.
    Settled { pr: PrRef, item: Option<String> },
    /// A decision that did not go through: the buttons are free again.
    Refused { pr: PrRef, item: Option<String> },
    /// A draft item has its new place (the refreshed review arrives as `Tell::ReviewFile`).
    Reanchored { pr: PrRef, item: String },
}

impl RevalidationTell {
    pub fn pr(&self) -> &PrRef {
        match self {
            RevalidationTell::Round { pr, .. }
            | RevalidationTell::Settled { pr, .. }
            | RevalidationTell::Refused { pr, .. }
            | RevalidationTell::Reanchored { pr, .. } => pr,
        }
    }

    /// The tell for a round event; `None` for every other event.
    pub fn from_event(event: &Event) -> Option<RevalidationTell> {
        match event.clone() {
            Event::RevalidationChanged { pr, revalidation } => Some(RevalidationTell::Round {
                pr,
                round: revalidation,
            }),
            _ => None,
        }
    }
}

/// The harness test on Config › Harness.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ProbeState {
    #[default]
    Idle,
    Testing,
    Done(ProbeResult),
}

/// What a refused `AgentSend` says in the chat.
fn refusal_text(code: ErrorCode, message: &str) -> String {
    if code == ErrorCode::Busy {
        // The busy line names no harness.
        AgentErrorKind::Busy.message_for("", "")
    } else {
        message.to_string()
    }
}

/// A probe that could not run, shown as the failed card.
fn failed_probe(message: impl Into<String>) -> ProbeResult {
    ProbeResult {
        ok: false,
        version: None,
        program: String::new(),
        elapsed_ms: 0,
        error: Some(message.into()),
        harness: String::new(),
        signed_in: None,
        account: None,
    }
}

/// A `Tell::Gifs` on its way to the GIF popover.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct GifsArrived(pub Result<GifPage, (ErrorCode, String)>);

/// Why the daemon has no file for a picture, and whether asking again later can help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaError {
    /// The connection or the daemon's network failed; the picture itself was not turned down.
    pub transient: bool,
    pub message: String,
}

impl MediaError {
    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            transient: true,
            message: message.into(),
        }
    }

    pub fn refused(message: impl Into<String>) -> Self {
        Self {
            transient: false,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tell {
    Snapshot(Box<Snapshot>),
    /// The answer to the last `Ask::SearchGifs`: a page, or the daemon's error code and message.
    Gifs(Result<GifPage, (ErrorCode, String)>),
    /// A config write was refused: the key and the daemon's message.
    Rejected {
        key: String,
        message: String,
    },
    /// A one-off outcome, shown as a toast.
    Notice {
        text: String,
        warning: bool,
    },
    /// Another launch asked this window to show something.
    Show(WindowTarget),
    /// The connection is gone; the UI offers `Ask::Reconnect`.
    Lost(String),
    /// The daemon is stopping on request (the tray's Quit): the window closes too.
    Quit,
    /// The tabs the window had open last time, once per window.
    Restored(clusia_core::OpenTabs),
    /// A `LoadStep` event, live while a review opens.
    Step(LoadStep),
    Opened {
        pr: PrRef,
        view: Box<ReviewView>,
        news: Vec<NewsItem>,
    },
    /// Like `Opened`, with the news still on its way: `News` follows, even when it is empty.
    /// The tab does not mark the review seen or open What's new before then.
    OpenedAwaitingNews {
        pr: PrRef,
        view: Box<ReviewView>,
    },
    /// The news of a review whose `OpenedAwaitingNews` came first.
    News {
        pr: PrRef,
        news: Vec<NewsItem>,
    },
    OpenedFromCache {
        pr: PrRef,
        view: Box<ReviewView>,
        fetched_at: i64,
    },
    /// `cache`: the daemon has a cached copy of this review.
    OpenFailed {
        pr: PrRef,
        message: String,
        cache: bool,
    },
    /// The stored review changed (draft items, state) for a review this window has open.
    ReviewFile(Box<Review>),
    Conversation {
        pr: PrRef,
        conversation: PrConversation,
    },
    /// A draft write with this ticket was saved.
    Saved {
        pr: PrRef,
        ticket: u64,
    },
    /// A draft write with this ticket was refused; the editor keeps its text.
    Refused {
        pr: PrRef,
        ticket: u64,
        message: String,
    },
    Published {
        pr: PrRef,
        result: PublishResult,
    },
    PublishFailed {
        pr: PrRef,
        code: ErrorCode,
        message: String,
    },
    /// The review was closed or discarded on the daemon.
    Left(PrRef),
    /// What the agent of a review did.
    Agent(AgentTell),
    /// A permission request, its end, or the review's rules.
    Permission(PermissionTell),
    /// What the review's security and audit checks did.
    Checks(ChecksTell),
    /// The comments hidden in a review, as the daemon keeps them.
    Hidden {
        pr: PrRef,
        set: clusia_core::HiddenComments,
    },
    /// What the review's diagrams did.
    Diagrams(DiagramsTell),
    /// What the review's Story did.
    Story(StoryTell),
    /// What the review's call graph did.
    Calls(CallsTell),
    /// What the review's tests did.
    Tests(TestsTell),
    /// What the test runs keep on this machine, or why that is not known.
    TestDisk(Result<DiskReport, String>),
    /// The engine and the sandbox the tests would use, or why that is not known.
    TestEngine(Result<TestEngineInfo, String>),
    /// What the revalidation of the review's draft did.
    Revalidation(RevalidationTell),
    /// The review's chat log, in answer to `Ask::AgentLog`.
    AgentLog {
        pr: PrRef,
        entries: Vec<AgentLogEntry>,
    },
    /// The harness test finished.
    Probe(ProbeResult),
    /// The daemon's answer to `FetchMedia`: the file in its media cache, or why there is none.
    Media {
        url: String,
        file: Result<MediaFile, MediaError>,
    },
    /// The daemon's answer to `FetchProfile`: the user's public profile, or why there is none.
    Profile {
        login: String,
        profile: Result<Profile, MediaError>,
    },
}

/// The ends Bevy keeps, and the connection thread.
pub struct Link {
    pub tell: Receiver<Tell>,
    pub ask: UnboundedSender<Ask>,
    pub thread: BridgeThread,
}

/// The connection thread. It ends once every `Ask` sender is gone and the asks already queued
/// are answered (the channel hands them out before it reports closed).
pub struct BridgeThread(std::thread::JoinHandle<()>);

impl BridgeThread {
    /// Waits up to `within` for the thread to end; `false` when it is still running.
    pub fn finish(self, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        while !self.0.is_finished() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.0.join();
        true
    }
}

/// Where `connect` leaves the bridge thread, so `app::run` can wait for it after the window
/// closed (leave choices sent on the way out must reach the daemon).
#[derive(Clone, Default)]
pub struct BridgeSlot(Arc<Mutex<Option<BridgeThread>>>);

impl BridgeSlot {
    pub fn take(&self) -> Option<BridgeThread> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    fn put(&self, thread: BridgeThread) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(thread);
    }
}

/// Starts the connection thread; `wake` runs after every `Tell`.
pub fn spawn(paths: Paths, home: Option<PathBuf>, wake: impl Fn() + Send + Sync + 'static) -> Link {
    let (tell_tx, tell_rx) = crossbeam_channel::unbounded();
    let (ask_tx, ask_rx) = unbounded_channel();
    let teller = Teller {
        tx: tell_tx,
        wake: Arc::new(wake),
    };
    let thread = std::thread::Builder::new()
        .name("clusia-bridge".into())
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(run(paths, home, teller, ask_rx)),
                Err(e) => teller.send(Tell::Lost(format!("cannot start the bridge: {e}"))),
            }
        })
        .expect("spawn the bridge thread");
    Link {
        tell: tell_rx,
        ask: ask_tx,
        thread: BridgeThread(thread),
    }
}

#[derive(Clone)]
struct Teller {
    tx: Sender<Tell>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Teller {
    fn send(&self, tell: Tell) {
        if self.tx.send(tell).is_ok() {
            (self.wake)();
        }
    }
}

/// The reviews this window has open on the daemon: their `ReviewChanged` / `ReviewOutdated`
/// events refetch the review file. Kept across reconnects, and shared with the workers so a
/// failed open forgets its PR.
type OpenSet = Arc<Mutex<HashSet<PrRef>>>;

fn open_set(open: &OpenSet) -> std::sync::MutexGuard<'_, HashSet<PrRef>> {
    open.lock().unwrap_or_else(|p| p.into_inner())
}

/// Sessions until Bevy drops its `Ask` sender; after a lost session, waits for `Reconnect`.
async fn run(
    paths: Paths,
    home: Option<PathBuf>,
    teller: Teller,
    mut asks: UnboundedReceiver<Ask>,
) {
    let open = OpenSet::default();
    let mut restored = false;
    loop {
        match session(
            &paths,
            home.as_deref(),
            &teller,
            &open,
            &mut restored,
            &mut asks,
        )
        .await
        {
            Ok(()) => return,
            Err(reason) => teller.send(Tell::Lost(reason)),
        }
        loop {
            match asks.recv().await {
                None => return,
                Some(Ask::Reconnect) => break,
                Some(Ask::SetConfig { key, .. }) => teller.send(Tell::Rejected {
                    key,
                    message: "Not connected to clusiad — not saved".into(),
                }),
                Some(ask) if offline(&ask, &teller) => {}
                Some(_) => teller.send(Tell::Notice {
                    text: "Not connected to clusiad — try again after reconnecting".into(),
                    warning: true,
                }),
            }
        }
    }
}

/// Answers a review ask while disconnected so its screen never waits: editors keep their text,
/// opening fails, publishing fails. `false` for the other asks.
fn offline(ask: &Ask, teller: &Teller) -> bool {
    const NOT_CONNECTED: &str = "Not connected to clusiad — not saved";
    match ask {
        Ask::AddItem { pr, ticket, .. } | Ask::UpdateItem { pr, ticket, .. } => {
            teller.send(Tell::Refused {
                pr: pr.clone(),
                ticket: *ticket,
                message: NOT_CONNECTED.into(),
            });
        }
        Ask::OpenReview(pr) => teller.send(Tell::OpenFailed {
            pr: pr.clone(),
            message: "Not connected to clusiad".into(),
            cache: false,
        }),
        Ask::Publish { pr, .. } => teller.send(Tell::PublishFailed {
            pr: pr.clone(),
            code: ErrorCode::Offline,
            message: "Not connected to clusiad — nothing was published".into(),
        }),
        Ask::FetchMedia(url) => teller.send(Tell::Media {
            url: url.clone(),
            file: Err(MediaError::transient("Not connected to clusiad")),
        }),
        Ask::FetchProfile(login) => teller.send(Tell::Profile {
            login: login.clone(),
            profile: Err(MediaError::transient("Not connected to clusiad")),
        }),
        Ask::AgentSend { pr, .. } => teller.send(Tell::Agent(AgentTell::Refused {
            pr: pr.clone(),
            message: "Not connected to clusiad — not sent".into(),
        })),
        Ask::Probe => teller.send(Tell::Probe(failed_probe("Not connected to clusiad"))),
        Ask::SearchGifs { .. } => teller.send(Tell::Gifs(Err((
            ErrorCode::Offline,
            "Not connected to clusiad".into(),
        )))),
        // A card waits for its accept or dismiss: free it, and say why nothing happened.
        Ask::AcceptFinding { pr, id, .. } | Ask::DismissFinding { pr, id } => {
            teller.send(Tell::Notice {
                text: "Not connected to clusiad — not sent".into(),
                warning: true,
            });
            teller.send(Tell::Checks(ChecksTell::Refused {
                pr: pr.clone(),
                id: id.clone(),
            }));
        }
        // The checks and the diagrams are read again after a reconnect: no toast for a read.
        Ask::GetChecks(_)
        | Ask::GetHiddenComments(_)
        | Ask::GetRevalidation(_)
        | Ask::GetDiagrams(_)
        | Ask::GetDiagramVersion { .. }
        | Ask::GetStory(_)
        | Ask::GetStoryVersion { .. }
        | Ask::GetCalls { .. }
        | Ask::FindFunction { .. } => {}
        Ask::WriteStory(_) | Ask::RewriteStory(_) | Ask::StopStory(_) => {
            teller.send(Tell::Notice {
                text: "Not connected to clusiad — not sent".into(),
                warning: true,
            });
        }
        // The card waits for its mark: undo it, and say why nothing happened.
        Ask::MarkStepRead { pr, step, read, .. } => {
            teller.send(Tell::Notice {
                text: "Not connected to clusiad — not sent".into(),
                warning: true,
            });
            teller.send(Tell::Story(StoryTell::ReadRefused {
                pr: pr.clone(),
                step: step.clone(),
                read: *read,
            }));
        }
        Ask::AddStepComment { pr, ticket, .. } | Ask::AddCallsComment { pr, ticket, .. } => {
            teller.send(Tell::Refused {
                pr: pr.clone(),
                ticket: *ticket,
                message: NOT_CONNECTED.into(),
            });
        }
        // The node waits for its code: tell it there is none.
        Ask::ReadFunctionCode { pr, function } => teller.send(Tell::Calls(CallsTell::Code {
            pr: pr.clone(),
            function: function.clone(),
            code: DiagramCode {
                path: String::new(),
                commit: String::new(),
                start_line: 0,
                lines: Vec::new(),
                unavailable: Some("Not connected to clusiad.".into()),
            },
        })),
        Ask::DrawDiagrams(_) | Ask::DrawDiagram { .. } | Ask::StopDiagrams(_) => {
            teller.send(Tell::Notice {
                text: "Not connected to clusiad — not sent".into(),
                warning: true,
            });
        }
        // The modal waits for its code: tell it there is none.
        Ask::ReadDiagramCode {
            pr,
            number,
            key,
            node,
        } => teller.send(Tell::Diagrams(DiagramsTell::Code {
            pr: pr.clone(),
            number: *number,
            key: key.clone(),
            node: node.clone(),
            code: DiagramCode {
                path: String::new(),
                commit: String::new(),
                start_line: 0,
                lines: Vec::new(),
                unavailable: Some("Not connected to clusiad.".into()),
            },
        })),
        Ask::AddDiagramComment { pr, ticket, .. } => teller.send(Tell::Refused {
            pr: pr.clone(),
            ticket: *ticket,
            message: NOT_CONNECTED.into(),
        }),
        // The row waits for its decision: free it, and say why nothing happened.
        Ask::DecideProposal { pr, item, .. } => {
            teller.send(Tell::Notice {
                text: "Not connected to clusiad — not sent".into(),
                warning: true,
            });
            teller.send(Tell::Revalidation(RevalidationTell::Refused {
                pr: pr.clone(),
                item: Some(item.clone()),
            }));
        }
        Ask::ReanchorDraftItem { pr, id, .. } => {
            teller.send(Tell::Notice {
                text: "Not connected to clusiad — not sent".into(),
                warning: true,
            });
            teller.send(Tell::Revalidation(RevalidationTell::Refused {
                pr: pr.clone(),
                item: Some(id.clone()),
            }));
        }
        Ask::AcceptAllProposals(pr) => {
            teller.send(Tell::Notice {
                text: "Not connected to clusiad — not sent".into(),
                warning: true,
            });
            teller.send(Tell::Revalidation(RevalidationTell::Refused {
                pr: pr.clone(),
                item: None,
            }));
        }
        // The window sends its tabs again once it is back.
        Ask::SetOpenTabs(_) => {}
        // The window already shows the change; hiding is not a write to GitHub, so there is
        // nothing to refuse.
        Ask::SetCommentHidden { .. } | Ask::ShowComments { .. } => {}
        Ask::Revalidate(_) | Ask::StopRevalidation(_) => teller.send(Tell::Notice {
            text: "Not connected to clusiad — not sent".into(),
            warning: true,
        }),
        // Reads are silent; the model asks again when the window is back.
        Ask::GetTests(_) => {}
        Ask::GenerateTests(pr) => tests_not_sent(teller, pr, TestsAskKind::Generate),
        Ask::PrepareTests { pr, .. } => tests_not_sent(teller, pr, TestsAskKind::Prepare),
        Ask::RunTests { pr, .. } => tests_not_sent(teller, pr, TestsAskKind::Run),
        Ask::StopTests(pr) => tests_not_sent(teller, pr, TestsAskKind::Stop),
        Ask::ListTests(pr) => tests_not_sent(teller, pr, TestsAskKind::List),
        Ask::AcceptRecipe(pr) => tests_not_sent(teller, pr, TestsAskKind::Accept),
        Ask::GetTestDisk => teller.send(Tell::TestDisk(Err("Not connected to clusiad".into()))),
        Ask::CleanUpTests { .. } => {
            teller.send(Tell::Notice {
                text: NOT_SENT.into(),
                warning: true,
            });
            teller.send(Tell::TestDisk(Err(NOT_SENT.into())));
        }
        Ask::CheckTestEngine { .. } => {
            teller.send(Tell::TestEngine(Err("Not connected to clusiad".into())));
        }
        _ => return false,
    }
    true
}

/// The answer to a GIF search; any reply but `Gifs` is a daemon fault, never silence.
fn gifs_tell(reply: Reply) -> Tell {
    match reply {
        Reply::Gifs(page) => Tell::Gifs(Ok(page)),
        _ => Tell::Gifs(Err((
            ErrorCode::Internal,
            "Unexpected reply from clusiad".into(),
        ))),
    }
}

/// `Ok(())` when the UI is gone; `Err(reason)` when the daemon is.
async fn session(
    paths: &Paths,
    home: Option<&Path>,
    teller: &Teller,
    open: &OpenSet,
    restored: &mut bool,
    asks: &mut UnboundedReceiver<Ask>,
) -> Result<(), String> {
    let (mut client, _) = clusia_protocol::launcher::ensure_daemon(paths, home, crate::CLIENT_NAME)
        .await
        .map_err(|e| format!("cannot reach clusiad: {e}"))?;
    let wanted = [
        topics::CONFIG,
        topics::PRS,
        topics::SYNC,
        topics::REVIEWS,
        topics::WINDOW,
        topics::AGENT,
        topics::TESTS,
    ];
    client
        .request(Command::Subscribe {
            topics: wanted.map(String::from).to_vec(),
        })
        .await
        .map_err(lost)?;
    let mut snap = Snapshot {
        daemon_version: client.daemon_version().to_string(),
        ..Snapshot::default()
    };
    fetch(&mut client, &mut snap, Refresh::STARTUP).await?;
    if let Some(Reply::Sync(s)) = request(&mut client, Command::GetSyncStatus).await? {
        snap.sync = Some(s);
    }
    teller.send(Tell::Snapshot(Box::new(snap.clone())));
    // Once per window: a reconnect must not close idle tabs the window still shows. A refusal
    // counts as no saved tabs, so the window keeps saving its own from now on: with nothing
    // saved, a closed window would leave its reviews open with no tab to close them from.
    if !*restored {
        let tabs = match request(&mut client, Command::RestoreTabs).await? {
            Some(Reply::OpenTabs(tabs)) => tabs,
            _ => clusia_core::OpenTabs::default(),
        };
        *restored = true;
        teller.send(Tell::Restored(tabs));
    }
    // A new connection holds nothing yet.
    let mut held = HashSet::new();
    hold_open_reviews(&mut client, open, &mut held).await?;
    // The daemon may have restarted while this window was away: its open reviews read again.
    let reopened: Vec<PrRef> = open_set(open).iter().cloned().collect();
    for pr in reopened {
        refetch(&mut client, &pr, teller).await?;
        fetch_log(&mut client, &mut snap, open, teller, pr).await?;
    }
    // Asking Giphy and `gh` and scanning the folders can take seconds: they come after the first
    // snapshot.
    let lists = Refresh {
        lists: true,
        giphy: true,
        first_run: true,
        harnesses: true,
        ..Refresh::default()
    };
    fetch(&mut client, &mut snap, lists).await?;
    teller.send(Tell::Snapshot(Box::new(snap.clone())));
    let mut last = snap.clone();
    loop {
        let mut todo = tokio::select! {
            event = client.next_event() => {
                let (_, event) = event.map_err(lost)?;
                follow(&mut client, open, &event, teller).await?;
                take(&mut snap, event, teller)
            }
            ask = asks.recv() => {
                let Some(ask) = ask else { return Ok(()) };
                answer(&mut client, &mut snap, teller, paths, open, ask).await?;
                Refresh::default()
            }
        };
        let deadline = tokio::time::Instant::now() + COALESCE;
        while todo.any() {
            match tokio::time::timeout_at(deadline, client.next_event()).await {
                Ok(Ok((_, event))) => {
                    follow(&mut client, open, &event, teller).await?;
                    todo.merge(take(&mut snap, event, teller));
                }
                Ok(Err(e)) => return Err(lost(e)),
                Err(_) => break,
            }
        }
        fetch(&mut client, &mut snap, todo).await?;
        hold_open_reviews(&mut client, open, &mut held).await?;
        if snap != last {
            teller.send(Tell::Snapshot(Box::new(snap.clone())));
            last = snap.clone();
        }
    }
}

/// Makes this connection hold exactly the reviews open in the window, so the daemon tells the
/// window, not the tray, about them. Opening runs on a worker connection, whose hold ends with
/// it; this one lasts as long as the window is connected.
async fn hold_open_reviews(
    client: &mut Client,
    open: &OpenSet,
    held: &mut HashSet<PrRef>,
) -> Result<(), String> {
    let wanted = open_set(open).clone();
    let changes: Vec<(PrRef, bool)> = wanted
        .difference(held)
        .map(|pr| (pr.clone(), true))
        .chain(held.difference(&wanted).map(|pr| (pr.clone(), false)))
        .collect();
    for (pr, hold) in changes {
        request(
            client,
            Command::HoldReview {
                pr: pr.clone(),
                held: hold,
            },
        )
        .await?;
        if hold {
            held.insert(pr);
        } else {
            held.remove(&pr);
        }
    }
    Ok(())
}

/// Review events for the open tabs: load steps go to Bevy as they come; a changed or outdated
/// review that is open here is fetched again.
async fn follow(
    client: &mut Client,
    open: &OpenSet,
    event: &Event,
    teller: &Teller,
) -> Result<(), String> {
    match event {
        Event::LoadStep(step) => teller.send(Tell::Step(step.clone())),
        Event::ReviewChanged { pr, .. } | Event::ReviewOutdated { pr, .. }
            if open_set(open).contains(pr) =>
        {
            refetch(client, pr, teller).await?;
        }
        other => {
            if let Some(tell) = AgentTell::from_event(other) {
                teller.send(Tell::Agent(tell));
            } else if let Some(tell) = PermissionTell::from_event(other) {
                teller.send(Tell::Permission(tell));
            } else if let Some(tell) = ChecksTell::from_event(other) {
                teller.send(Tell::Checks(tell));
            } else if let Some(tell) = DiagramsTell::from_event(other) {
                teller.send(Tell::Diagrams(tell));
            } else if let Some(tell) = StoryTell::from_event(other) {
                teller.send(Tell::Story(tell));
            } else if let Some(tell) = CallsTell::from_event(other) {
                teller.send(Tell::Calls(tell));
            } else if let Some(tell) = RevalidationTell::from_event(other) {
                teller.send(Tell::Revalidation(tell));
            } else if let Some(tell) = TestsTell::from_event(other) {
                teller.send(Tell::Tests(tell));
            }
        }
    }
    Ok(())
}

/// `GetReview` → `Tell::ReviewFile` (a refusal is only logged).
async fn refetch(client: &mut Client, pr: &PrRef, teller: &Teller) -> Result<(), String> {
    if let Some(Reply::ReviewFile(review)) =
        request(client, Command::GetReview { pr: pr.clone() }).await?
    {
        teller.send(Tell::ReviewFile(review));
    }
    Ok(())
}

fn take(snap: &mut Snapshot, event: Event, teller: &Teller) -> Refresh {
    if event == Event::Stopping {
        teller.send(Tell::Quit);
        return Refresh::default();
    }
    let (refresh, show) = snapshot::apply(snap, event);
    if let Some(target) = show {
        teller.send(Tell::Show(target));
    }
    refresh
}

fn lost(e: ClientError) -> String {
    match e {
        ClientError::Closed => "clusiad closed the connection".into(),
        e => e.to_string(),
    }
}

/// `Ok(None)` for a daemon-side error (logged; the session goes on); `Err` ends the session.
async fn request(client: &mut Client, cmd: Command) -> Result<Option<Reply>, String> {
    match client.request(cmd).await {
        Ok(reply) => Ok(Some(reply)),
        Err(ClientError::Server(e)) => {
            tracing::warn!(error = %e.message, "daemon request failed");
            Ok(None)
        }
        Err(e) => Err(lost(e)),
    }
}

async fn fetch(client: &mut Client, snap: &mut Snapshot, what: Refresh) -> Result<(), String> {
    if what.config
        && let Some(Reply::Config(c)) = request(client, Command::GetConfig).await?
    {
        snap.config = c;
    }
    if what.lists {
        let assigned = request(
            client,
            Command::ListPrs {
                filter: PrFilter::Assigned,
            },
        )
        .await?;
        let mine = request(
            client,
            Command::ListPrs {
                filter: PrFilter::Mine,
            },
        )
        .await?;
        if let (Some(Reply::Prs(a)), Some(Reply::Prs(m))) = (assigned, mine) {
            snap.assigned = a;
            snap.mine = m;
            snap.lists_loaded = true;
        }
    }
    if what.reviews
        && let Some(Reply::Reviews(r)) = request(client, Command::ListReviews).await?
    {
        snap.reviews = r;
    }
    if what.activity
        && let Some(Reply::Activity(a)) = request(client, Command::GetActivity).await?
    {
        snap.activity = Some(a);
    }
    if what.auth
        && let Some(Reply::Auth(a)) = request(client, Command::AuthStatus).await?
    {
        snap.auth = Some(a);
    }
    // One scan answers both: the first-run screen and the agents every other screen reads.
    let first_run = what.first_run && snap.first_run_wanted();
    if (first_run || what.harnesses)
        && let Some(Reply::FirstRun(f)) = request(client, Command::FirstRunStatus).await?
    {
        snap.harnesses = Some(f.harnesses.clone());
        if first_run {
            snap.first_run = Some(f);
        }
    }
    if what.status
        && let Some(Reply::Status(s)) = request(client, Command::DaemonStatus).await?
    {
        snap.notifications_permission = s.notifications_permission;
    }
    if what.giphy
        && let Some(Reply::GiphyKeyStatus(status)) =
            request(client, Command::GiphyKeyStatus).await?
    {
        snap.giphy_key = if status.configured {
            GiphyKey::Set
        } else {
            GiphyKey::Missing
        };
    }
    Ok(())
}

async fn answer(
    client: &mut Client,
    snap: &mut Snapshot,
    teller: &Teller,
    paths: &Paths,
    open: &OpenSet,
    ask: Ask,
) -> Result<(), String> {
    let auth = Refresh {
        auth: true,
        first_run: true,
        ..Refresh::default()
    };
    match ask {
        Ask::SetConfig { key, value } => {
            match client
                .request(Command::SetConfigValue {
                    key: key.clone(),
                    value,
                })
                .await
            {
                // The `ConfigChanged` event refreshes the snapshot.
                Ok(_) => {}
                Err(ClientError::Server(e)) => teller.send(Tell::Rejected {
                    key,
                    message: e.message,
                }),
                Err(e) => return Err(lost(e)),
            }
        }
        Ask::SetToken(token) => {
            notify(
                client,
                teller,
                Command::SetToken { token },
                "Token saved in the Keychain",
            )
            .await?;
            fetch(client, snap, auth).await?;
        }
        Ask::ClearToken => {
            notify(client, teller, Command::ClearToken, "Stored token removed").await?;
            fetch(client, snap, auth).await?;
        }
        Ask::SetStartAtLogin { on } => {
            match client.request(Command::SetStartAtLogin { on }).await {
                Ok(_) => {}
                Err(ClientError::Server(e)) => teller.send(Tell::Rejected {
                    key: START_AT_LOGIN.into(),
                    message: e.message,
                }),
                Err(e) => return Err(lost(e)),
            }
        }
        Ask::TestNotification => {
            notify(
                client,
                teller,
                Command::TestNotification,
                "Test notification sent",
            )
            .await?;
        }
        Ask::RefreshStatus => {
            fetch(
                client,
                snap,
                Refresh {
                    status: true,
                    ..Refresh::default()
                },
            )
            .await?;
        }
        Ask::SyncNow => {
            if let Some(Reply::Sync(s)) = request(client, Command::SyncNow).await? {
                snap.sync = Some(s);
            }
        }
        Ask::RefreshAuth => fetch(client, snap, auth).await?,
        Ask::FirstRunDone => snap.first_run_open = false,
        Ask::SetGiphyKey(key) => {
            notify(
                client,
                teller,
                Command::SetGiphyKey { key },
                "Giphy key saved in the Keychain",
            )
            .await?;
        }
        Ask::ClearGiphyKey => {
            notify(client, teller, Command::ClearGiphyKey, "Giphy key removed").await?;
        }
        Ask::OpenInEditor { path, line } => {
            notify(client, teller, Command::OpenInEditor { path, line }, "").await?;
        }
        Ask::Reconnect => {}
        ask if on_worker(&ask) => {
            if let Ask::OpenReview(pr) = &ask {
                open_set(open).insert(pr.clone());
            }
            spawn_worker(paths, teller, open, ask);
        }
        Ask::GetTests(_)
        | Ask::GenerateTests(_)
        | Ask::PrepareTests { .. }
        | Ask::RunTests { .. }
        | Ask::StopTests(_)
        | Ask::AcceptRecipe(_) => tests_ask(client, teller, ask).await?,
        Ask::LoadConversation(pr) => match client
            .request(Command::GetConversation { pr: pr.clone() })
            .await
        {
            Ok(Reply::Conversation(conversation)) => {
                teller.send(Tell::Conversation { pr, conversation });
            }
            Ok(_) => {}
            Err(ClientError::Server(e)) => teller.send(Tell::Notice {
                text: e.message,
                warning: true,
            }),
            Err(e) => return Err(lost(e)),
        },
        Ask::SearchGifs { query, offset } => {
            match client.request(Command::SearchGifs { query, offset }).await {
                Ok(reply) => teller.send(gifs_tell(reply)),
                Err(ClientError::Server(e)) => teller.send(Tell::Gifs(Err((e.code, e.message)))),
                Err(e) => {
                    teller.send(Tell::Gifs(Err((
                        ErrorCode::Offline,
                        "Not connected to clusiad".into(),
                    ))));
                    return Err(lost(e));
                }
            }
        }
        Ask::MarkSeen(pr) => {
            request(client, Command::MarkSeen { pr }).await?;
        }
        Ask::AddItem {
            pr,
            kind,
            anchor,
            thread,
            body,
            ticket,
        } => {
            let cmd = Command::AddDraftItem {
                pr: pr.clone(),
                kind,
                anchor,
                body,
                thread,
            };
            draft_write(client, teller, cmd, pr, ticket).await?;
        }
        Ask::UpdateItem {
            pr,
            id,
            body,
            ticket,
        } => {
            let cmd = Command::UpdateDraftItem {
                pr: pr.clone(),
                id,
                body,
            };
            draft_write(client, teller, cmd, pr, ticket).await?;
        }
        Ask::RemoveItem { pr, id } => {
            notify(client, teller, Command::RemoveDraftItem { pr, id }, "").await?;
        }
        Ask::SetOpenTabs(tabs) => {
            request(client, Command::SetOpenTabs { tabs }).await?;
        }
        Ask::CloseReview(pr) => {
            open_set(open).remove(&pr);
            // The tab closes whatever the daemon says; a refusal is only reported.
            notify(client, teller, Command::CloseReview { pr: pr.clone() }, "").await?;
            teller.send(Tell::Left(pr));
        }
        Ask::Discard(pr) => match client
            .request(Command::DiscardReview { pr: pr.clone() })
            .await
        {
            Ok(_) => {
                let left = Tell::Left(pr);
                forget_finished(open, &left);
                teller.send(left);
            }
            Err(ClientError::Server(e)) => teller.send(Tell::Notice {
                text: e.message,
                warning: true,
            }),
            Err(e) => return Err(lost(e)),
        },
        Ask::AgentSend { pr, text } => {
            match client
                .request(Command::AgentSend {
                    pr: pr.clone(),
                    text,
                })
                .await
            {
                Ok(_) => {}
                Err(ClientError::Server(e)) => teller.send(Tell::Agent(AgentTell::Refused {
                    pr,
                    message: refusal_text(e.code, &e.message),
                })),
                Err(e) => return Err(lost(e)),
            }
        }
        Ask::AgentCancel { pr } => {
            notify(client, teller, Command::AgentCancel { pr }, "").await?;
        }
        Ask::AcceptSuggestion { pr, id, body } => {
            let cmd = Command::AcceptSuggestion {
                pr: pr.clone(),
                id: id.clone(),
                body,
            };
            if suggestion_write(client, teller, cmd, pr.clone(), id, true).await? {
                fetch_log(client, snap, open, teller, pr).await?;
            }
        }
        Ask::DismissSuggestion { pr, id } => {
            let cmd = Command::DismissSuggestion {
                pr: pr.clone(),
                id: id.clone(),
            };
            suggestion_write(client, teller, cmd, pr, id, false).await?;
        }
        Ask::AgentLog { pr } => fetch_log(client, snap, open, teller, pr).await?,
        Ask::PermissionAnswer { id, answer } => {
            match client
                .request(Command::PermissionAnswer {
                    id: id.clone(),
                    answer,
                })
                .await
            {
                Ok(_) => {}
                Err(ClientError::Server(e)) => {
                    for tell in refused_answer(&id, &e) {
                        teller.send(tell);
                    }
                }
                Err(e) => return Err(lost(e)),
            }
        }
        Ask::RevokeRule { pr, rule } => {
            match client
                .request(Command::RevokeRule {
                    pr: pr.clone(),
                    rule,
                })
                .await
            {
                Ok(_) => {}
                // The chip is gone from the footer but the rule may still apply: show what the
                // daemon holds.
                Err(ClientError::Server(e)) => {
                    teller.send(Tell::Notice {
                        text: e.message,
                        warning: true,
                    });
                    send_rules(client, snap, open, teller, &pr).await?;
                }
                Err(e) => return Err(lost(e)),
            }
        }
        Ask::RunCheck { pr, kind } => {
            notify(client, teller, Command::RunCheck { pr, kind }, "").await?;
        }
        Ask::StopCheck { pr, kind } => {
            notify(client, teller, Command::StopCheck { pr, kind }, "").await?;
        }
        Ask::GetChecks(pr) => send_checks(client, teller, pr).await?,
        Ask::GetHiddenComments(pr) => {
            if let Some(Reply::HiddenComments(set)) =
                request(client, Command::GetHiddenComments { pr: pr.clone() }).await?
            {
                teller.send(Tell::Hidden { pr, set });
            }
        }
        Ask::ShowComments { pr, keys } => {
            let command = Command::ShowComments {
                pr: pr.clone(),
                keys,
            };
            if let Some(Reply::HiddenComments(set)) = request(client, command).await? {
                teller.send(Tell::Hidden { pr, set });
            }
        }
        Ask::SetCommentHidden { pr, key, posts } => {
            let command = Command::SetCommentHidden {
                pr: pr.clone(),
                key,
                posts,
            };
            if let Some(Reply::HiddenComments(set)) = request(client, command).await? {
                teller.send(Tell::Hidden { pr, set });
            }
        }
        Ask::DrawDiagrams(pr) => notify(client, teller, Command::DrawDiagrams { pr }, "").await?,
        Ask::DrawDiagram { pr, key } => {
            notify(client, teller, Command::DrawDiagram { pr, key }, "").await?;
        }
        Ask::StopDiagrams(pr) => notify(client, teller, Command::StopDiagrams { pr }, "").await?,
        Ask::GetDiagrams(pr) => send_diagrams(client, teller, pr).await?,
        Ask::GetDiagramVersion { pr, number } => {
            let tell = match request(
                client,
                Command::GetDiagramVersion {
                    pr: pr.clone(),
                    number,
                },
            )
            .await?
            {
                Some(Reply::DiagramVersion(version)) => DiagramsTell::Version { pr, version },
                _ => DiagramsTell::Unreadable { pr, number },
            };
            teller.send(Tell::Diagrams(tell));
        }
        Ask::ReadDiagramCode {
            pr,
            number,
            key,
            node,
        } => send_diagram_code(client, teller, pr, number, key, node).await?,
        Ask::AddDiagramComment {
            pr,
            number,
            key,
            node,
            body,
            ticket,
        } => {
            let cmd = Command::AddDiagramComment {
                pr: pr.clone(),
                number,
                key,
                node,
                body,
            };
            draft_write(client, teller, cmd, pr, ticket).await?;
        }
        Ask::WriteStory(pr) => notify(client, teller, Command::WriteStory { pr }, "").await?,
        Ask::RewriteStory(pr) => notify(client, teller, Command::RewriteStory { pr }, "").await?,
        Ask::StopStory(pr) => notify(client, teller, Command::StopStory { pr }, "").await?,
        Ask::GetStory(pr) => send_story(client, teller, pr).await?,
        Ask::GetStoryVersion { pr, number } => {
            let tell = match request(
                client,
                Command::GetStoryVersion {
                    pr: pr.clone(),
                    number,
                },
            )
            .await?
            {
                Some(Reply::StoryVersion(version)) => StoryTell::Version {
                    pr,
                    version: Box::new(version),
                },
                _ => StoryTell::Unreadable { pr, number },
            };
            teller.send(Tell::Story(tell));
        }
        Ask::MarkStepRead {
            pr,
            number,
            step,
            read,
        } => {
            let cmd = Command::MarkStepRead {
                pr: pr.clone(),
                number,
                step: step.clone(),
                read,
            };
            match client.request(cmd).await {
                Ok(_) => {}
                Err(ClientError::Server(e)) => {
                    teller.send(Tell::Notice {
                        text: e.message,
                        warning: true,
                    });
                    teller.send(Tell::Story(StoryTell::ReadRefused { pr, step, read }));
                }
                Err(e) => return Err(lost(e)),
            }
        }
        Ask::AddStepComment {
            pr,
            number,
            step,
            body,
            ticket,
        } => {
            let cmd = Command::AddStepComment {
                pr: pr.clone(),
                number,
                step,
                body,
            };
            draft_write(client, teller, cmd, pr, ticket).await?;
        }
        Ask::GetCalls {
            pr,
            center,
            depth,
            expand,
        } => {
            send_calls(client, teller, pr, center, depth, expand).await?;
        }
        Ask::FindFunction { pr, query } => {
            let cmd = Command::FindFunction {
                pr: pr.clone(),
                query: query.clone(),
            };
            if let Some(Reply::Functions(hits)) = request(client, cmd).await? {
                teller.send(Tell::Calls(CallsTell::Found { pr, query, hits }));
            }
        }
        Ask::ReadFunctionCode { pr, function } => {
            send_function_code(client, teller, pr, function).await?
        }
        Ask::AddCallsComment {
            pr,
            function,
            body,
            ticket,
        } => {
            let cmd = Command::AddCallsComment {
                pr: pr.clone(),
                function,
                body,
            };
            draft_write(client, teller, cmd, pr, ticket).await?;
        }
        Ask::Revalidate(pr) => notify(client, teller, Command::Revalidate { pr }, "").await?,
        Ask::StopRevalidation(pr) => {
            notify(client, teller, Command::StopRevalidation { pr }, "").await?;
        }
        Ask::GetRevalidation(pr) => send_revalidation(client, teller, pr).await?,
        Ask::DecideProposal { pr, item, accept } => {
            let cmd = Command::DecideProposal {
                pr: pr.clone(),
                item: item.clone(),
                accept,
            };
            decision_write(client, teller, cmd, pr, Some(item)).await?;
        }
        Ask::AcceptAllProposals(pr) => {
            let cmd = Command::AcceptAllProposals { pr: pr.clone() };
            decision_write(client, teller, cmd, pr, None).await?;
        }
        Ask::ReanchorDraftItem { pr, id, anchor } => {
            let cmd = Command::ReanchorDraftItem {
                pr: pr.clone(),
                id: id.clone(),
                anchor,
            };
            match client.request(cmd).await {
                Ok(_) => teller.send(Tell::Revalidation(RevalidationTell::Reanchored {
                    pr,
                    item: id,
                })),
                Err(ClientError::Server(e)) => {
                    teller.send(Tell::Notice {
                        text: e.message,
                        warning: true,
                    });
                    teller.send(Tell::Revalidation(RevalidationTell::Refused {
                        pr,
                        item: Some(id),
                    }));
                }
                Err(e) => return Err(lost(e)),
            }
        }
        Ask::AcceptFinding { pr, id, body } => {
            let cmd = Command::AcceptFinding {
                pr: pr.clone(),
                id: id.clone(),
                body,
            };
            finding_write(client, teller, cmd, pr, id, true).await?;
        }
        Ask::DismissFinding { pr, id } => {
            let cmd = Command::DismissFinding {
                pr: pr.clone(),
                id: id.clone(),
            };
            finding_write(client, teller, cmd, pr, id, false).await?;
        }
        // The asks `on_worker` names went to their own connection in the first arm.
        _ => {}
    }
    Ok(())
}

/// `AddDraftItem` / `UpdateDraftItem`: `Saved` or `Refused` with the editor's ticket.
async fn draft_write(
    client: &mut Client,
    teller: &Teller,
    cmd: Command,
    pr: PrRef,
    ticket: u64,
) -> Result<(), String> {
    match client.request(cmd).await {
        Ok(_) => teller.send(Tell::Saved { pr, ticket }),
        Err(ClientError::Server(e)) => teller.send(Tell::Refused {
            pr,
            ticket,
            message: e.message,
        }),
        Err(e) => return Err(lost(e)),
    }
    Ok(())
}

/// Asks for the chat log of `pr` and tells it. The agent events of `pr` that arrived while the
/// reply was awaited are dropped: the log has what they said up to the running answer's last
/// cut, and the chat asks the log again when that turn ends. Session states are not logged, so
/// they are told after the log (the daemon tells the state of `pr` whenever its log is read).
/// Every other buffered event, other reviews' agent events included, is handled as usual.
///
/// The review's rules and the permission requests still waiting are read next. Each of those
/// replies is told after the events that came before it, so an older list never replaces what
/// a later `RulesChanged` or `PermissionResolved` said.
async fn fetch_log(
    client: &mut Client,
    snap: &mut Snapshot,
    open: &OpenSet,
    teller: &Teller,
    pr: PrRef,
) -> Result<(), String> {
    let reply = request(client, Command::GetAgentLog { pr: pr.clone() }).await?;
    let mut states = Vec::new();
    for (_, event) in client.take_events() {
        if let Some(tell) = AgentTell::from_event(&event).filter(|tell| *tell.pr() == pr) {
            if matches!(tell, AgentTell::State { .. }) {
                states.push(tell);
            }
            continue;
        }
        follow(client, open, &event, teller).await?;
        let todo = take(snap, event, teller);
        fetch(client, snap, todo).await?;
    }
    if let Some(Reply::AgentLog(entries)) = reply {
        teller.send(Tell::AgentLog {
            pr: pr.clone(),
            entries,
        });
    }
    for state in states {
        teller.send(Tell::Agent(state));
    }
    send_rules(client, snap, open, teller, &pr).await?;
    let waiting = request(client, Command::GetPermissions { pr: pr.clone() }).await?;
    handle_buffered(client, snap, open, teller).await?;
    // A window that was not listening (the notification started it, or it reconnected) learns
    // what is still waiting here; this is also how its modal opens.
    if let Some(Reply::Permissions(requests)) = waiting {
        teller.send(Tell::Permission(PermissionTell::Pending { pr, requests }));
    }
    Ok(())
}

/// Tells the rules the daemon holds for `pr`, after the events that arrived before the reply.
async fn send_rules(
    client: &mut Client,
    snap: &mut Snapshot,
    open: &OpenSet,
    teller: &Teller,
    pr: &PrRef,
) -> Result<(), String> {
    let rules = request(client, Command::GetRules { pr: pr.clone() }).await?;
    handle_buffered(client, snap, open, teller).await?;
    if let Some(Reply::Rules(rules)) = rules {
        teller.send(Tell::Permission(PermissionTell::Rules {
            pr: pr.clone(),
            rules,
        }));
    }
    Ok(())
}

/// Handles the events that arrived while a reply was awaited, as the event loop would.
async fn handle_buffered(
    client: &mut Client,
    snap: &mut Snapshot,
    open: &OpenSet,
    teller: &Teller,
) -> Result<(), String> {
    for (_, event) in client.take_events() {
        follow(client, open, &event, teller).await?;
        let todo = take(snap, event, teller);
        fetch(client, snap, todo).await?;
    }
    Ok(())
}

/// `AcceptSuggestion` / `DismissSuggestion`: `Handled` when it went through, else a warning.
/// True when an accept found the suggestion no longer waiting, so the chat must be read again.
async fn suggestion_write(
    client: &mut Client,
    teller: &Teller,
    cmd: Command,
    pr: PrRef,
    id: String,
    accepted: bool,
) -> Result<bool, String> {
    match client.request(cmd).await {
        Ok(_) => teller.send(Tell::Agent(AgentTell::Handled { pr, id, accepted })),
        // The daemon says `NotFound` for a suggestion that is no longer waiting: accepted or
        // dismissed from another window or the CLI. A dismiss wanted it gone, which it is; an
        // accept added nothing, so the card must not say it did.
        Err(ClientError::Server(e)) if e.code == ErrorCode::NotFound => {
            if accepted {
                teller.send(Tell::Notice {
                    text: "This suggestion is no longer waiting — nothing was added to your draft"
                        .into(),
                    warning: true,
                });
                return Ok(true);
            }
            teller.send(Tell::Agent(AgentTell::Handled { pr, id, accepted }));
        }
        Err(ClientError::Server(e)) => teller.send(Tell::Notice {
            text: e.message,
            warning: true,
        }),
        Err(e) => return Err(lost(e)),
    }
    Ok(false)
}

/// `GetChecks` → `ChecksTell::Loaded` (a refusal is only logged).
async fn send_checks(client: &mut Client, teller: &Teller, pr: PrRef) -> Result<(), String> {
    if let Some(Reply::Checks {
        results,
        states,
        accepted,
        dismissed,
    }) = request(client, Command::GetChecks { pr: pr.clone() }).await?
    {
        teller.send(Tell::Checks(ChecksTell::Loaded {
            pr,
            results,
            states,
            accepted,
            dismissed,
        }));
    }
    Ok(())
}

/// `GetDiagrams` → `DiagramsTell::Loaded` (a refusal is only logged).
async fn send_diagrams(client: &mut Client, teller: &Teller, pr: PrRef) -> Result<(), String> {
    if let Some(Reply::Diagrams {
        versions,
        latest,
        state,
        head,
        stale,
        stale_reason,
    }) = request(client, Command::GetDiagrams { pr: pr.clone() }).await?
    {
        teller.send(Tell::Diagrams(DiagramsTell::Loaded {
            pr,
            versions,
            latest,
            state,
            head,
            stale,
            stale_reason,
        }));
    }
    Ok(())
}

/// `ReadDiagramCode` → `DiagramsTell::Code`. A refusal is told as code that is not available,
/// so the modal that waits for it says so.
async fn send_diagram_code(
    client: &mut Client,
    teller: &Teller,
    pr: PrRef,
    number: u32,
    key: String,
    node: String,
) -> Result<(), String> {
    let cmd = Command::ReadDiagramCode {
        pr: pr.clone(),
        number,
        key: key.clone(),
        node: node.clone(),
    };
    let code = match client.request(cmd).await {
        Ok(Reply::DiagramCode(code)) => code,
        Ok(_) => return Ok(()),
        Err(ClientError::Server(e)) => DiagramCode {
            path: String::new(),
            commit: String::new(),
            start_line: 0,
            lines: Vec::new(),
            unavailable: Some(e.message),
        },
        Err(e) => return Err(lost(e)),
    };
    teller.send(Tell::Diagrams(DiagramsTell::Code {
        pr,
        number,
        key,
        node,
        code,
    }));
    Ok(())
}

/// `GetStory` → `StoryTell::Loaded` (a refusal is only logged).
async fn send_story(client: &mut Client, teller: &Teller, pr: PrRef) -> Result<(), String> {
    if let Some(Reply::Story {
        versions,
        latest,
        state,
        static_steps,
        read,
        head,
        new_commits,
    }) = request(client, Command::GetStory { pr: pr.clone() }).await?
    {
        teller.send(Tell::Story(StoryTell::Loaded {
            pr,
            versions,
            latest: latest.map(Box::new),
            state,
            static_steps,
            read,
            head,
            new_commits,
        }));
    }
    Ok(())
}

/// `GetCalls` → `CallsTell::Graph`. A refusal is told as a graph that failed, so the view
/// that waits for it says why.
async fn send_calls(
    client: &mut Client,
    teller: &Teller,
    pr: PrRef,
    center: Option<String>,
    depth: Option<u8>,
    expand: Vec<i8>,
) -> Result<(), String> {
    let asked = CallsRequest {
        center: center.clone(),
        depth: depth.unwrap_or(0),
        expand: expand.clone(),
    };
    let cmd = Command::GetCalls {
        pr: pr.clone(),
        center,
        depth,
        expand,
    };
    let (graph, index, unsupported) = match client.request(cmd).await {
        Ok(Reply::Calls {
            graph,
            index,
            unsupported,
        }) => (graph.map(Box::new), index, unsupported),
        Ok(_) => return Ok(()),
        Err(ClientError::Server(e)) => (
            None,
            CallsIndexState::Failed { message: e.message },
            Vec::new(),
        ),
        Err(e) => return Err(lost(e)),
    };
    teller.send(Tell::Calls(CallsTell::Graph {
        pr,
        request: asked,
        graph,
        index,
        unsupported,
    }));
    Ok(())
}

/// `ReadFunctionCode` → `CallsTell::Code`. A refusal is told as code that is not available.
async fn send_function_code(
    client: &mut Client,
    teller: &Teller,
    pr: PrRef,
    function: String,
) -> Result<(), String> {
    let cmd = Command::ReadFunctionCode {
        pr: pr.clone(),
        function: function.clone(),
    };
    let code = match client.request(cmd).await {
        Ok(Reply::FunctionCode(code)) => code,
        Ok(_) => return Ok(()),
        Err(ClientError::Server(e)) => DiagramCode {
            path: String::new(),
            commit: String::new(),
            start_line: 0,
            lines: Vec::new(),
            unavailable: Some(e.message),
        },
        Err(e) => return Err(lost(e)),
    };
    teller.send(Tell::Calls(CallsTell::Code { pr, function, code }));
    Ok(())
}

/// `GetRevalidation` → `RevalidationTell::Round` (a refusal is only logged).
async fn send_revalidation(client: &mut Client, teller: &Teller, pr: PrRef) -> Result<(), String> {
    if let Some(Reply::Revalidation(round)) =
        request(client, Command::GetRevalidation { pr: pr.clone() }).await?
    {
        teller.send(Tell::Revalidation(RevalidationTell::Round { pr, round }));
    }
    Ok(())
}

/// `DecideProposal` / `AcceptAllProposals`: the refreshed review and `Settled` when it went
/// through, else a warning and `Refused`, so the row is free again. A proposal the daemon no
/// longer has (decided from another window, or dropped by a new head) reads the round again.
async fn decision_write(
    client: &mut Client,
    teller: &Teller,
    cmd: Command,
    pr: PrRef,
    item: Option<String>,
) -> Result<(), String> {
    match client.request(cmd).await {
        Ok(Reply::ReviewFile(review)) => {
            teller.send(Tell::ReviewFile(review));
            teller.send(Tell::Revalidation(RevalidationTell::Settled { pr, item }));
        }
        Ok(_) => teller.send(Tell::Revalidation(RevalidationTell::Settled { pr, item })),
        Err(ClientError::Server(e)) => {
            let reread = matches!(e.code, ErrorCode::NotFound | ErrorCode::Conflict);
            teller.send(Tell::Notice {
                text: e.message,
                warning: true,
            });
            teller.send(Tell::Revalidation(RevalidationTell::Refused {
                pr: pr.clone(),
                item,
            }));
            if reread {
                refetch(client, &pr, teller).await?;
                send_revalidation(client, teller, pr).await?;
            }
        }
        Err(e) => return Err(lost(e)),
    }
    Ok(())
}

/// `AcceptFinding` / `DismissFinding`: `Settled` when it went through, else a warning and
/// `Refused`, so the card is free again. A finding the daemon no longer has (settled from
/// another window or the CLI) reads the checks again, so the card shows what is true.
async fn finding_write(
    client: &mut Client,
    teller: &Teller,
    cmd: Command,
    pr: PrRef,
    id: String,
    accepted: bool,
) -> Result<(), String> {
    match client.request(cmd).await {
        Ok(_) => teller.send(Tell::Checks(ChecksTell::Settled { pr, id, accepted })),
        Err(ClientError::Server(e)) => {
            let gone = e.code == ErrorCode::NotFound;
            teller.send(Tell::Notice {
                text: e.message,
                warning: true,
            });
            teller.send(Tell::Checks(ChecksTell::Refused { pr: pr.clone(), id }));
            if gone {
                send_checks(client, teller, pr).await?;
            }
        }
        Err(e) => return Err(lost(e)),
    }
    Ok(())
}

/// Runs a long request on its own connection, so this one keeps reading events.
fn spawn_worker(paths: &Paths, teller: &Teller, open: &OpenSet, ask: Ask) {
    tokio::spawn(work(paths.socket(), teller.clone(), open.clone(), ask));
}

/// Stops following a review that is no longer open on the daemon: it failed to open, was
/// published (its file is gone), or was left.
fn forget_finished(open: &OpenSet, tell: &Tell) {
    match tell {
        // A failed refresh leaves the cached copy on screen, and that copy still follows changes.
        Tell::OpenFailed { cache: true, .. } => {}
        Tell::OpenFailed { pr, .. } | Tell::Published { pr, .. } | Tell::Left(pr) => {
            open_set(open).remove(pr);
        }
        _ => {}
    }
}

async fn work(socket: PathBuf, teller: Teller, open: OpenSet, ask: Ask) {
    match ask {
        Ask::OpenReview(pr) => {
            let tell = open_review(&socket, &teller, pr).await;
            forget_finished(&open, &tell);
            teller.send(tell);
        }
        Ask::FetchMedia(url) => teller.send(fetch_media(&socket, url).await),
        Ask::FetchProfile(login) => teller.send(fetch_profile(&socket, login).await),
        Ask::Publish {
            pr,
            verdict,
            summary,
        } => {
            let tell = publish(&socket, pr, verdict, summary).await;
            forget_finished(&open, &tell);
            teller.send(tell);
        }
        Ask::Probe => teller.send(probe(&socket).await),
        ask @ (Ask::ListTests(_)
        | Ask::GetTestDisk
        | Ask::CleanUpTests { .. }
        | Ask::CheckTestEngine { .. }) => tests_on_worker(&socket, &teller, ask).await,
        _ => {}
    }
}

/// The asks that run on a connection of their own, so the main one keeps reading events and a
/// `StopTests` is never queued behind a slow ask.
fn on_worker(ask: &Ask) -> bool {
    matches!(
        ask,
        Ask::OpenReview(_)
            | Ask::Publish { .. }
            | Ask::FetchMedia(_)
            | Ask::FetchProfile(_)
            | Ask::Probe
            | Ask::ListTests(_)
            | Ask::GetTestDisk
            | Ask::CleanUpTests { .. }
            | Ask::CheckTestEngine { .. }
    )
}

/// One tests ask on `client`: the answer becomes the tell the model waits for, a refusal of a
/// write becomes a warning and a `Refused` that frees the button. A lost connection is `Err`.
async fn tests_ask(client: &mut Client, teller: &Teller, ask: Ask) -> Result<(), String> {
    let (cmd, pr, kind) = match &ask {
        Ask::GetTests(pr) => (Command::GetTests { pr: pr.clone() }, Some(pr.clone()), None),
        Ask::GenerateTests(pr) => (
            Command::GenerateTests { pr: pr.clone() },
            Some(pr.clone()),
            Some(TestsAskKind::Generate),
        ),
        Ask::PrepareTests {
            pr,
            recipe_hash,
            approve,
        } => (
            Command::PrepareTests {
                pr: pr.clone(),
                recipe_hash: recipe_hash.clone(),
                approve: approve.clone(),
            },
            Some(pr.clone()),
            Some(TestsAskKind::Prepare),
        ),
        Ask::RunTests { pr, scope } => (
            Command::RunTests {
                pr: pr.clone(),
                scope: scope.clone(),
            },
            Some(pr.clone()),
            Some(TestsAskKind::Run),
        ),
        Ask::StopTests(pr) => (
            Command::StopTests { pr: pr.clone() },
            Some(pr.clone()),
            Some(TestsAskKind::Stop),
        ),
        Ask::ListTests(pr) => (
            Command::ListTests { pr: pr.clone() },
            Some(pr.clone()),
            Some(TestsAskKind::List),
        ),
        Ask::AcceptRecipe(pr) => (
            Command::AcceptRecipe { pr: pr.clone() },
            Some(pr.clone()),
            Some(TestsAskKind::Accept),
        ),
        Ask::GetTestDisk => (Command::GetTestDisk, None, None),
        Ask::CleanUpTests { repo } => (Command::CleanUpTests { repo: repo.clone() }, None, None),
        Ask::CheckTestEngine { pr } => (Command::CheckTestEngine { pr: pr.clone() }, None, None),
        _ => return Ok(()),
    };
    match client.request(cmd).await {
        Ok(Reply::Tests(view)) => {
            if let Some(pr) = pr {
                teller.send(Tell::Tests(TestsTell::Loaded { pr, view }));
            }
        }
        Ok(Reply::TestRunStarted { run }) => {
            if let Some(pr) = pr {
                teller.send(Tell::Tests(TestsTell::RunStarted { pr, run }));
            }
        }
        Ok(Reply::TestList(tests)) => {
            if let Some(pr) = pr {
                teller.send(Tell::Tests(TestsTell::Listed {
                    pr,
                    count: tests.len() as u32,
                }));
            }
        }
        Ok(Reply::TestDisk(report)) => teller.send(Tell::TestDisk(Ok(report))),
        Ok(Reply::TestEngine(info)) => teller.send(Tell::TestEngine(Ok(info))),
        Ok(_) => {}
        Err(ClientError::Server(e)) => tests_refused(teller, &ask, pr, kind, e.message),
        Err(e) => return Err(lost(e)),
    }
    Ok(())
}

fn tests_refused(
    teller: &Teller,
    ask: &Ask,
    pr: Option<PrRef>,
    kind: Option<TestsAskKind>,
    message: String,
) {
    let warn = |message: &str| {
        teller.send(Tell::Notice {
            text: message.to_string(),
            warning: true,
        });
    };
    match (pr, kind, ask) {
        (Some(pr), Some(kind), _) => {
            warn(&message);
            teller.send(Tell::Tests(TestsTell::Refused {
                pr,
                ask: kind,
                message,
            }));
        }
        (_, _, Ask::CleanUpTests { .. }) => {
            warn(&message);
            teller.send(Tell::TestDisk(Err(message)));
        }
        (_, _, Ask::GetTestDisk) => teller.send(Tell::TestDisk(Err(message))),
        (_, _, Ask::CheckTestEngine { .. }) => teller.send(Tell::TestEngine(Err(message))),
        // A refused read says nothing: the tab asks again when it has a reason to.
        _ => {}
    }
}

/// A worker's connection failing is the same as being offline for that ask.
async fn tests_on_worker(socket: &Path, teller: &Teller, ask: Ask) {
    let reached = match worker(socket).await {
        Ok(mut client) => tests_ask(&mut client, teller, ask.clone()).await.is_ok(),
        Err(_) => false,
    };
    if !reached {
        offline(&ask, teller);
    }
}

fn tests_not_sent(teller: &Teller, pr: &PrRef, kind: TestsAskKind) {
    teller.send(Tell::Notice {
        text: NOT_SENT.into(),
        warning: true,
    });
    teller.send(Tell::Tests(TestsTell::Refused {
        pr: pr.clone(),
        ask: kind,
        message: NOT_SENT.into(),
    }));
}

async fn probe(socket: &Path) -> Tell {
    let mut client = match worker(socket).await {
        Ok(client) => client,
        Err(message) => return Tell::Probe(failed_probe(message)),
    };
    match client.request(Command::HarnessProbe).await {
        Ok(Reply::Probe(result)) => Tell::Probe(result),
        Ok(_) => Tell::Probe(failed_probe("Unexpected reply from clusiad")),
        Err(e) => Tell::Probe(failed_probe(message_of(e))),
    }
}

async fn worker(socket: &Path) -> Result<Client, String> {
    Client::connect(socket, crate::CLIENT_NAME)
        .await
        .map_err(|e| format!("cannot reach clusiad: {e}"))
}

fn message_of(e: ClientError) -> String {
    match e {
        ClientError::Server(e) => e.message,
        e => lost(e),
    }
}

/// Sends `OpenedFromCache` when there is a cached copy, so the tab shows it at once. The fresh
/// copy is sent as `OpenedAwaitingNews` the moment it arrives and the news after it, so the tab
/// does not wait for the news. Gives back the last tell: `News`, or `OpenFailed`.
async fn open_review(socket: &Path, teller: &Teller, pr: PrRef) -> Tell {
    let mut client = match worker(socket).await {
        Ok(c) => c,
        Err(message) => {
            return Tell::OpenFailed {
                pr,
                message,
                cache: false,
            };
        }
    };
    let mut cache = false;
    if let Ok(Reply::Cached(cached)) = client
        .request(Command::GetCachedReview { pr: pr.clone() })
        .await
    {
        cache = true;
        let cached = *cached;
        teller.send(Tell::OpenedFromCache {
            pr: pr.clone(),
            view: Box::new(cached.view),
            fetched_at: cached.fetched_at,
        });
    }
    match client.request(Command::OpenReview { pr: pr.clone() }).await {
        Ok(Reply::Review(view)) => {
            teller.send(Tell::OpenedAwaitingNews {
                pr: pr.clone(),
                view,
            });
            let news = match client
                .request(Command::GetWhatsNew { pr: pr.clone() })
                .await
            {
                Ok(Reply::WhatsNew(news)) => news,
                _ => Vec::new(),
            };
            Tell::News { pr, news }
        }
        Ok(_) => Tell::OpenFailed {
            pr,
            message: "clusiad sent an unexpected reply".into(),
            cache,
        },
        Err(e) => Tell::OpenFailed {
            pr,
            message: message_of(e),
            cache,
        },
    }
}

/// `Media`: the daemon's cached file for `url`, or the reason it has none.
async fn fetch_media(socket: &Path, url: String) -> Tell {
    let file = match worker(socket).await {
        Err(message) => Err(MediaError::transient(message)),
        Ok(mut client) => match client
            .request(Command::FetchMedia { url: url.clone() })
            .await
        {
            Ok(Reply::Media(file)) => Ok(file),
            Ok(_) => Err(MediaError::transient("clusiad sent an unexpected reply")),
            Err(e) => Err(media_error(e)),
        },
    };
    Tell::Media { url, file }
}

/// `Profile`: the daemon's answer for `login`, or the reason it has none.
async fn fetch_profile(socket: &Path, login: String) -> Tell {
    let profile = match worker(socket).await {
        Err(message) => Err(MediaError::transient(message)),
        Ok(mut client) => match client
            .request(Command::GetProfile {
                login: login.clone(),
            })
            .await
        {
            Ok(Reply::Profile(profile)) => Ok(profile),
            Ok(_) => Err(MediaError::transient("clusiad sent an unexpected reply")),
            Err(e) => Err(profile_error(e)),
        },
    };
    Tell::Profile { login, profile }
}

/// A profile is final only when GitHub has no such user; a rate limit, a refused token or a
/// failing server can pass, so the next hover asks again.
fn profile_error(e: ClientError) -> MediaError {
    match e {
        ClientError::Server(e) => MediaError {
            transient: e.code != ErrorCode::NotFound,
            message: e.message,
        },
        e => MediaError::transient(lost(e)),
    }
}

/// Only the daemon's error code says whether asking again can help; every other failure is the
/// connection.
fn media_error(e: ClientError) -> MediaError {
    match e {
        ClientError::Server(e) => MediaError {
            transient: matches!(e.code, ErrorCode::Offline | ErrorCode::Internal),
            message: e.message,
        },
        e => MediaError::transient(lost(e)),
    }
}

/// `Published` or `PublishFailed`.
async fn publish(socket: &Path, pr: PrRef, verdict: Verdict, summary: String) -> Tell {
    let mut client = match worker(socket).await {
        Ok(c) => c,
        Err(message) => {
            return Tell::PublishFailed {
                pr,
                code: ErrorCode::Offline,
                message,
            };
        }
    };
    let cmd = Command::Publish {
        pr: pr.clone(),
        verdict,
        summary,
    };
    match client.request(cmd).await {
        Ok(Reply::Published(result)) => Tell::Published { pr, result },
        Ok(_) => Tell::PublishFailed {
            pr,
            code: ErrorCode::Internal,
            message: "clusiad sent an unexpected reply".into(),
        },
        Err(ClientError::Server(e)) => Tell::PublishFailed {
            pr,
            code: e.code,
            message: e.message,
        },
        Err(e) => Tell::PublishFailed {
            pr,
            code: ErrorCode::Offline,
            message: format!(
                "Lost clusiad while publishing ({}). The review may have been posted — check \
                 GitHub before publishing again.",
                lost(e)
            ),
        },
    }
}

/// What an answer the daemon refused tells the window.
fn refused_answer(id: &str, e: &ProtocolError) -> Vec<Tell> {
    if e.code == ErrorCode::NotFound {
        // Another window or the deadline answered first: the modal closes on the
        // `PermissionResolved` that is already on its way.
        return Vec::new();
    }
    vec![
        Tell::Permission(PermissionTell::AnswerRefused { id: id.into() }),
        Tell::Notice {
            text: e.message.clone(),
            warning: true,
        },
    ]
}

/// Sends `cmd`. A refusal becomes a warning notice, and success shows `ok` (when not empty).
async fn notify(
    client: &mut Client,
    teller: &Teller,
    cmd: Command,
    ok: &str,
) -> Result<(), String> {
    match client.request(cmd).await {
        Ok(_) => {
            if !ok.is_empty() {
                teller.send(Tell::Notice {
                    text: ok.into(),
                    warning: false,
                });
            }
            Ok(())
        }
        Err(ClientError::Server(e)) => {
            teller.send(Tell::Notice {
                text: e.message,
                warning: true,
            });
            Ok(())
        }
        Err(e) => Err(lost(e)),
    }
}

// ---- Bevy side ----

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Connection {
    #[default]
    Connecting,
    Live,
    Lost(String),
}

/// Everything the screens read.
#[derive(Resource, Debug, Default)]
pub struct Model {
    pub snapshot: Snapshot,
    pub connection: Connection,
    /// The daemon's last refusal per config key, shown next to that field.
    pub rejected: HashMap<String, String>,
    pub probe: ProbeState,
}

/// Where UI actions go: the bridge in live mode, `recorded` otherwise (demo mode, tests).
#[derive(Resource, Default)]
pub struct Asks {
    tx: Option<UnboundedSender<Ask>>,
    pub recorded: Vec<Ask>,
}

impl Asks {
    pub fn live(tx: UnboundedSender<Ask>) -> Self {
        Self {
            tx: Some(tx),
            recorded: Vec::new(),
        }
    }

    pub fn send(&mut self, ask: Ask) {
        match &self.tx {
            Some(tx) => {
                let _ = tx.send(ask);
            }
            None => self.recorded.push(ask),
        }
    }
}

/// Writes a config value and forgets that key's previous refusal.
pub fn set_config(asks: &mut Asks, model: &mut Model, key: &str, value: impl Into<String>) {
    model.rejected.remove(key);
    asks.send(Ask::SetConfig {
        key: key.to_string(),
        value: value.into(),
    });
}

#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub text: String,
    pub warning: bool,
    /// `Time::elapsed_secs_f64` after which it disappears.
    pub until: f64,
}

#[derive(Resource, Debug, Default)]
pub struct Toasts(pub Vec<Toast>);

/// Another launch asked for this (through the daemon).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ShowRequested(pub WindowTarget);

/// Where `pump` reads tells from: the bridge thread, or `Outbox` (demo mode and tests).
#[derive(Resource)]
pub(crate) struct Inbox(Receiver<Tell>);

/// Tells produced inside the app (demo answers, tests); `pump` applies them next frame.
#[derive(Resource, Clone)]
pub(crate) struct Outbox(pub Sender<Tell>);

pub(crate) fn local_link() -> (Inbox, Outbox) {
    let (tx, rx) = crossbeam_channel::unbounded();
    (Inbox(rx), Outbox(tx))
}

pub struct BridgePlugin {
    pub mode: Mode,
    pub paths: Paths,
    pub home: Option<PathBuf>,
    /// Receives the connection thread in live mode.
    pub thread: BridgeSlot,
}

impl Plugin for BridgePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Model>()
            .init_resource::<Toasts>()
            .add_message::<ShowRequested>()
            .add_message::<TabsRestored>()
            .add_message::<GifsArrived>()
            .add_systems(PreUpdate, pump);
        match self.mode {
            Mode::Live => {
                let (paths, home) = (self.paths.clone(), self.home.clone());
                let slot = self.thread.clone();
                app.add_systems(Startup, move |world: &mut World| {
                    connect(world, paths.clone(), home.clone(), &slot);
                });
            }
            Mode::Demo {
                theme,
                ref settings,
            } => {
                let settings = settings.clone();
                let (inbox, outbox) = local_link();
                app.init_resource::<Asks>()
                    .insert_resource(inbox)
                    .insert_resource(outbox)
                    .add_systems(
                        Startup,
                        move |mut model: ResMut<Model>, clock: Res<Clock>| {
                            model.snapshot = fixture::demo(clock.now());
                            if let Some(theme) = theme {
                                model.snapshot.config.appearance.theme = theme;
                            }
                            let config = &mut model.snapshot.config;
                            for (key, value) in &settings {
                                if let Err(e) =
                                    crate::snapshot::apply_config_locally(config, key, value)
                                {
                                    tracing::warn!(%key, error = %e, "a demo setting was refused");
                                }
                            }
                            model.connection = Connection::Live;
                        },
                    )
                    .add_systems(Update, demo_answers);
            }
        }
    }
}

fn connect(world: &mut World, paths: Paths, home: Option<PathBuf>, slot: &BridgeSlot) {
    let proxy = world
        .get_resource::<EventLoopProxyWrapper>()
        .map(|p| (**p).clone());
    let link = spawn(paths, home, move || {
        if let Some(proxy) = &proxy {
            let _ = proxy.send_event(WinitUserEvent::WakeUp);
        }
    });
    world.insert_resource(Inbox(link.tell));
    world.insert_resource(Asks::live(link.ask));
    slot.put(link.thread);
}

/// What the daemon says about a review besides its draft, and about the people on it, as one
/// parameter of `pump`.
#[derive(SystemParam)]
pub(crate) struct ReviewModels<'w> {
    checks: ResMut<'w, Checks>,
    diagrams: ResMut<'w, Diagrams>,
    revalidations: ResMut<'w, Revalidations>,
    stories: ResMut<'w, Stories>,
    calls: ResMut<'w, Calls>,
    people: ResMut<'w, People>,
    tests: ResMut<'w, TestsModels>,
    machine: ResMut<'w, TestMachine>,
}

pub(crate) fn pump(
    inbox: Option<Res<Inbox>>,
    mut model: ResMut<Model>,
    mut toasts: ResMut<Toasts>,
    mut tabs: ResMut<ReviewTabs>,
    mut asks: ResMut<Asks>,
    mut show: MessageWriter<ShowRequested>,
    mut exit: MessageWriter<AppExit>,
    mut restored: MessageWriter<TabsRestored>,
    mut reviews: MessageWriter<ReviewEvent>,
    mut media: ResMut<MediaCache>,
    mut gifs: MessageWriter<GifsArrived>,
    mut chats: ResMut<Chats>,
    mut permissions: ResMut<PermissionQueue>,
    models: ReviewModels,
    clock: Res<Clock>,
    time: Res<Time>,
) {
    let Some(inbox) = inbox else { return };
    let ReviewModels {
        mut checks,
        mut diagrams,
        mut revalidations,
        mut stories,
        mut calls,
        mut people,
        mut tests,
        mut machine,
    } = models;
    // Tells are mostly about other things than the tabs, and every rebuild system wakes for a
    // tab change: the tabs are marked changed only for the tells that wrote one.
    let mut changed = false;
    let tabs_in = tabs.bypass_change_detection();
    for tell in inbox.0.try_iter() {
        let mut events = Vec::new();
        let applied = review_state::apply(tabs_in, &mut asks, tell, &mut events);
        changed |= applied.changed;
        reviews.write_batch(events);
        let Some(tell) = applied.rest else { continue };
        match tell {
            Tell::Snapshot(s) => {
                if matches!(model.connection, Connection::Lost(_)) {
                    media.retry_failed();
                    people.retry_failed();
                }
                // A refusal belongs to the key that was refused; a new status means a new verdict.
                if s.giphy_key != model.snapshot.giphy_key {
                    model.rejected.remove(GIPHY_KEY_REFUSAL);
                }
                // A Test result describes the harness it was run on: another one makes it stale.
                let (old, new) = (&model.snapshot.config.harness, &s.config.harness);
                if old.kind != new.kind || old.program != new.program {
                    model.probe = ProbeState::Idle;
                }
                model.snapshot = *s;
                model.connection = Connection::Live;
            }
            Tell::Gifs(answer) => {
                match &answer {
                    Err((ErrorCode::Unauthorized, message)) => {
                        model
                            .rejected
                            .insert(GIPHY_KEY_REFUSAL.to_string(), message.clone());
                    }
                    Ok(_) => {
                        model.rejected.remove(GIPHY_KEY_REFUSAL);
                    }
                    Err(_) => {}
                }
                gifs.write(GifsArrived(answer));
            }
            Tell::Rejected { key, message } => {
                model.rejected.insert(key, message);
            }
            Tell::Notice { text, warning } => toasts.0.push(Toast {
                text,
                warning,
                until: time.elapsed_secs_f64() + TOAST_SECS,
            }),
            Tell::Show(target) => {
                show.write(ShowRequested(target));
            }
            Tell::Lost(reason) => {
                model.connection = Connection::Lost(reason);
                // The daemon denied what it held; a reconnect in this same batch must not
                // meet requests that no longer exist.
                permissions.0.clear();
                // The answers still owed are lost: read the checks again on reconnect.
                for model in checks.0.values_mut() {
                    model.asked = false;
                }
                for tab in tabs_in.0.values_mut() {
                    changed |= std::mem::take(&mut tab.ui.hidden_asked);
                }
                for model in diagrams.0.values_mut() {
                    model.asked = false;
                    model.requested.clear();
                    model.failed.clear();
                }
                // So are the rounds, and a decision in flight is never answered.
                for model in revalidations.0.values_mut() {
                    model.asked = false;
                    model.pending.clear();
                    model.reanchor.clear();
                    model.reanchoring.clear();
                    model.all_pending = false;
                }
                for model in stories.0.values_mut() {
                    model.asked = false;
                    model.write_sent = false;
                    model.requested.clear();
                    model.failed.clear();
                }
                for model in calls.0.values_mut() {
                    model.asked = false;
                    model.asking = None;
                    model.requested_code.clear();
                }
                tests.lost();
                machine.lost();
            }
            Tell::Quit => {
                exit.write(AppExit::Success);
            }
            Tell::Restored(tabs) => {
                restored.write(TabsRestored(tabs));
            }
            Tell::Refused { message, .. } => toasts.0.push(Toast {
                text: message,
                warning: true,
                until: time.elapsed_secs_f64() + TOAST_SECS,
            }),
            Tell::Media { url, file } => media.arrive(url, file),
            Tell::Profile { login, profile } => people.arrive(login, profile),
            Tell::Agent(agent) => {
                changed |= crate::screens::review::agent::suggestion::close_edited(tabs_in, &agent);
                if chats.apply(&agent) {
                    asks.send(Ask::AgentLog {
                        pr: agent.pr().clone(),
                    });
                }
            }
            Tell::AgentLog { pr, entries } => chats.replay(&pr, &entries),
            Tell::Permission(tell) => permissions.apply(tell, &mut chats, clock.now_ms()),
            Tell::Checks(tell) => {
                changed |= checks::close_edited(tabs_in, &tell);
                checks.apply(&tell);
                changed |= checks::reconcile_editors(
                    tabs_in,
                    &checks,
                    &mut toasts,
                    time.elapsed_secs_f64(),
                );
            }
            Tell::Hidden { pr, set } => {
                if let Some(tab) = tabs_in.0.get_mut(&pr) {
                    tab.ui.hidden = set;
                    tab.ui.hidden_asked = true;
                    changed = true;
                }
            }
            Tell::Tests(tell) => tests.apply(&tell),
            Tell::TestDisk(report) => machine.disk_arrived(report),
            Tell::TestEngine(info) => machine.engine_arrived(info),
            Tell::Diagrams(tell) => diagrams.apply(&tell),
            Tell::Story(tell) => stories.apply(&tell),
            Tell::Calls(tell) => {
                if let CallsTell::Index {
                    pr,
                    state: CallsIndexState::Ready { .. } | CallsIndexState::Failed { .. },
                } = &tell
                {
                    stories.index_finished(pr);
                }
                calls.apply(&tell);
            }
            Tell::Revalidation(tell) => {
                // A round that starts again shows its screen again, even after *Back to review*.
                if let RevalidationTell::Round {
                    pr,
                    round: Some(new),
                } = &tell
                    && crate::screens::review::revalidation::reopens(
                        revalidations.0.get(pr).and_then(|m| m.round.as_ref()),
                        new,
                    )
                    && let Some(tab) = tabs_in.0.get_mut(pr)
                {
                    tab.ui.revalidation = crate::review_state::RevalView::Auto;
                    changed = true;
                }
                // *Keep and re-anchor* leaves for the Diff only once the daemon took the decision.
                if let RevalidationTell::Settled {
                    pr,
                    item: Some(item),
                } = &tell
                    && revalidations.take_reanchor(pr, item)
                    && let Some(tab) = tabs_in.0.get_mut(pr)
                {
                    crate::screens::review::revalidation::enter_reanchor(tab, item);
                    changed = true;
                }
                // The item has its new place: the Diff stops asking for one.
                if let RevalidationTell::Reanchored { pr, item } = &tell
                    && let Some(tab) = tabs_in.0.get_mut(pr)
                    && tab.ui.reanchor.as_deref() == Some(item)
                {
                    tab.ui.reanchor = None;
                    changed = true;
                }
                revalidations.apply(&tell);
            }
            // An answer nobody is waiting for (the harness changed meanwhile) is dropped.
            Tell::Probe(result) if model.probe == ProbeState::Testing => {
                model.probe = ProbeState::Done(result);
            }
            _ => {} // the other review tells were applied above
        }
    }
    if changed {
        tabs.set_changed();
    }
}

/// Demo mode: config writes apply locally (refusals show like the daemon's); the review asks
/// are answered from `fixture::demo_review` through `Outbox`, so they go through `pump` like
/// the daemon's answers; other asks only say they were not sent.
pub(crate) fn demo_answers(
    mut asks: ResMut<Asks>,
    mut model: ResMut<Model>,
    mut toasts: ResMut<Toasts>,
    tabs: Res<ReviewTabs>,
    revalidations: Res<Revalidations>,
    diagrams: Option<Res<Diagrams>>,
    outbox: Res<Outbox>,
    queue: Res<PermissionQueue>,
    clock: Res<Clock>,
    time: Res<Time>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut rule_granted: Local<bool>,
) {
    if asks.recorded.is_empty() {
        return;
    }
    let now = clock.now();
    let mut tells = Vec::new();
    for ask in std::mem::take(&mut asks.recorded) {
        match ask {
            Ask::SetConfig { key, value } => {
                if let Err(message) =
                    snapshot::apply_config_locally(&mut model.snapshot.config, &key, &value)
                {
                    model.rejected.insert(key, message);
                }
            }
            Ask::SetStartAtLogin { on } => {
                if let Err(message) = snapshot::apply_config_locally(
                    &mut model.snapshot.config,
                    START_AT_LOGIN,
                    &on.to_string(),
                ) {
                    model.rejected.insert(START_AT_LOGIN.into(), message);
                }
            }
            Ask::SetGiphyKey(_) => model.snapshot.giphy_key = GiphyKey::Set,
            Ask::ClearGiphyKey => model.snapshot.giphy_key = GiphyKey::Missing,
            Ask::OpenReview(pr) => tells.extend(demo_open(pr, now)),
            Ask::LoadConversation(pr) if pr == fixture::demo_pr() => {
                if let Some(conversation) = fixture::demo_review(now).0.conversation {
                    tells.push(Tell::Conversation { pr, conversation });
                }
            }
            Ask::MarkSeen(_) | Ask::RefreshStatus => {}
            Ask::AgentLog { pr } if pr == fixture::demo_pr() => {
                tells.push(Tell::AgentLog {
                    pr: pr.clone(),
                    entries: fixture::demo_agent_log(now),
                });
                tells.push(Tell::Agent(AgentTell::State {
                    pr,
                    state: SessionStateKind::Ready,
                }));
            }
            Ask::AgentLog { .. } => {}
            Ask::AgentSend { pr, text } => {
                tells.extend(demo_agent_reply(
                    &pr,
                    &text,
                    now * 1000,
                    *rule_granted,
                    &model.snapshot.config.harness.display_name(),
                ));
            }
            Ask::PermissionAnswer { id, answer } => {
                *rule_granted |= answer == PermissionAnswerKind::Review && queue.get(&id).is_some();
                tells.extend(demo_permission_answer(&queue, &id, answer));
            }
            Ask::RevokeRule { pr, .. } => {
                *rule_granted = false;
                tells.push(Tell::Permission(PermissionTell::Rules {
                    pr,
                    rules: Vec::new(),
                }));
            }
            Ask::AgentCancel { pr } => tells.push(Tell::Agent(AgentTell::State {
                pr,
                state: SessionStateKind::Ready,
            })),
            Ask::AcceptSuggestion { pr, id, body } => {
                tells.extend(demo_accept(&tabs, &pr, &id, body, now));
            }
            Ask::DismissSuggestion { pr, id } => tells.push(Tell::Agent(AgentTell::Handled {
                pr,
                id,
                accepted: false,
            })),
            // What the draft already holds from a check is accepted: a read after an accept (or a
            // reconnect) must not give the card its buttons back.
            Ask::GetChecks(pr) if pr == fixture::demo_pr() => {
                if let Some(ready) = tabs.0.get(&pr).and_then(|t| t.ready()) {
                    tells.push(fixture::demo_checks_tell(
                        &pr,
                        now,
                        &ready.view.review.draft,
                    ));
                }
            }
            // Any other review has no checks: its tabs say not checked instead of loading.
            Ask::GetChecks(pr) => tells.push(Tell::Checks(ChecksTell::Loaded {
                pr,
                results: Vec::new(),
                states: Vec::new(),
                accepted: Vec::new(),
                dismissed: Vec::new(),
            })),
            Ask::RunCheck { pr, kind } => {
                let (results, _) = fixture::demo_checks(now);
                if let Some(result) = results.into_iter().find(|r| r.kind == kind) {
                    tells.push(Tell::Checks(ChecksTell::Done { pr, kind, result }));
                }
            }
            Ask::StopCheck { pr, kind } => tells.push(Tell::Checks(ChecksTell::State {
                pr,
                kind,
                state: CheckState::NotRun,
            })),
            Ask::AcceptFinding { pr, id, body } => {
                tells.extend(demo_accept_finding(&tabs, &pr, &id, body, now));
            }
            Ask::DismissFinding { pr, id } => tells.push(Tell::Checks(ChecksTell::Settled {
                pr,
                id,
                accepted: false,
            })),
            // The demo's rounds come from scenes and tests; any other review has none.
            Ask::GetRevalidation(pr) => tells.push(Tell::Revalidation(RevalidationTell::Round {
                round: tabs
                    .0
                    .get(&pr)
                    .and_then(|t| t.ready())
                    .and_then(|r| r.view.review.revalidation.clone()),
                pr,
            })),
            Ask::Revalidate(_) | Ask::StopRevalidation(_) => {}
            Ask::DecideProposal { pr, item, accept } => {
                tells.extend(demo_decide(&tabs, &revalidations, &pr, Some(&item), accept));
            }
            Ask::AcceptAllProposals(pr) => {
                tells.extend(demo_decide(&tabs, &revalidations, &pr, None, true));
            }
            Ask::ReanchorDraftItem { pr, id, anchor } => {
                tells.extend(demo_reanchor(&tabs, &pr, &id, anchor));
            }
            Ask::GetDiagrams(pr) if pr == fixture::demo_pr() => {
                tells.push(fixture::demo_diagrams_tell(&pr, now));
            }
            // Any other review has none: its tab offers to draw them instead of loading.
            Ask::GetDiagrams(pr) => tells.push(Tell::Diagrams(DiagramsTell::Loaded {
                pr,
                versions: Vec::new(),
                latest: None,
                state: DiagramsState::Idle,
                head: String::new(),
                stale: false,
                stale_reason: None,
            })),
            Ask::GetDiagramVersion { pr, number } => {
                if let Some(version) = fixture::demo_diagrams(now, &pr)
                    .into_iter()
                    .find(|v| v.number == number)
                {
                    tells.push(Tell::Diagrams(DiagramsTell::Version {
                        pr,
                        version: Box::new(version),
                    }));
                }
            }
            // The demo draws at once: every version arrives whole.
            Ask::DrawDiagrams(pr) if pr == fixture::demo_pr() => {
                tells.push(fixture::demo_diagrams_tell(&pr, now));
            }
            Ask::DrawDiagram { pr, key } if pr == fixture::demo_pr() => {
                let state = fixture::demo_diagrams(now, &pr)
                    .pop()
                    .and_then(|v| v.entry(&key).map(|e| (v.number, e.state.clone())));
                if let Some((number, state)) = state {
                    tells.push(Tell::Diagrams(DiagramsTell::Entry {
                        pr,
                        number,
                        key,
                        state,
                    }));
                }
            }
            Ask::StopDiagrams(pr) => tells.push(Tell::Diagrams(DiagramsTell::State {
                pr,
                state: DiagramsState::Idle,
            })),
            Ask::Probe => tells.push(Tell::Probe(fixture::demo_probe())),
            Ask::AddItem {
                pr,
                kind,
                anchor,
                thread,
                body,
                ticket,
            } => tells.extend(demo_edit(&tabs, &pr, ticket, |review| {
                let anchor = anchor.map(|a| Anchor {
                    commit: match a.side {
                        Side::Right => review.head_sha.clone(),
                        Side::Left => review.base_sha.clone(),
                    },
                    path: a.path,
                    line: a.line,
                    start_line: a.start_line,
                    side: a.side,
                });
                review
                    .draft
                    .add(kind, anchor, thread, &body, now)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })),
            Ask::UpdateItem {
                pr,
                id,
                body,
                ticket,
            } => tells.extend(demo_edit(&tabs, &pr, ticket, |review| {
                review
                    .draft
                    .update_body(&id, &body)
                    .map_err(|e| e.to_string())
            })),
            Ask::ReadDiagramCode {
                pr,
                number,
                key,
                node,
            } => {
                let ready = tabs.0.get(&pr).and_then(|t| t.ready());
                let model = diagrams.as_ref().and_then(|d| d.0.get(&pr));
                let code = modal::demo_code_for(model, ready, number, &key, &node);
                tells.push(Tell::Diagrams(DiagramsTell::Code {
                    pr,
                    number,
                    key,
                    node,
                    code,
                }));
            }
            Ask::AddDiagramComment {
                pr,
                number,
                key,
                node,
                body,
                ticket,
            } => {
                let anchor = diagrams
                    .as_ref()
                    .and_then(|d| d.0.get(&pr))
                    .and_then(|m| m.loaded.get(&number))
                    .and_then(|v| modal::anchor_of(v, &key, &node));
                let patch = anchor.as_ref().and_then(|a| {
                    let ready = tabs.0.get(&pr)?.ready()?;
                    ready
                        .view
                        .diff
                        .iter()
                        .find(|f| f.path == a.path)?
                        .patch
                        .clone()
                });
                tells.extend(demo_edit(&tabs, &pr, ticket, |review| {
                    modal::diagram_comment(review, anchor.as_ref(), patch.as_deref(), &body, now)
                }));
            }
            Ask::GetStory(pr) if pr == fixture::demo_pr() => {
                tells.push(fixture::demo_story_tell(&pr, now))
            }
            Ask::GetStory(pr) => tells.push(Tell::Story(StoryTell::Loaded {
                pr,
                versions: Vec::new(),
                latest: None,
                state: StoryState::Idle,
                static_steps: Vec::new(),
                read: Vec::new(),
                head: String::new(),
                new_commits: 0,
            })),
            Ask::GetStoryVersion { pr, number } => {
                if let Some(version) = fixture::demo_story_version(now, number) {
                    tells.push(Tell::Story(StoryTell::Version {
                        pr,
                        version: Box::new(version),
                    }));
                }
            }
            Ask::WriteStory(_) | Ask::RewriteStory(_) | Ask::MarkStepRead { .. } => {}
            Ask::StopStory(pr) => tells.push(Tell::Story(StoryTell::State {
                pr,
                state: StoryState::Idle,
            })),
            Ask::AddStepComment {
                pr,
                number,
                step,
                body,
                ticket,
            } => {
                let title = fixture::demo_story_version(now, number)
                    .and_then(|v| v.spec)
                    .and_then(|s| s.steps.into_iter().find(|x| x.id == step).map(|x| x.title))
                    .unwrap_or(step);
                tells.extend(demo_edit(&tabs, &pr, ticket, |review| {
                    crate::screens::review::story::demo_step_comment(review, &title, &body, now)
                }));
            }
            Ask::GetCalls {
                pr,
                center,
                depth,
                expand,
            } if pr == fixture::demo_pr() => {
                let depth = depth.unwrap_or(model.snapshot.config.diff.calls_depth);
                tells.push(fixture::demo_calls_graph(center.as_deref(), depth, &expand));
            }
            Ask::GetCalls {
                pr,
                center,
                depth,
                expand,
            } => tells.push(Tell::Calls(CallsTell::Graph {
                pr,
                request: CallsRequest {
                    center,
                    depth: depth.unwrap_or(0),
                    expand,
                },
                graph: None,
                index: CallsIndexState::Idle,
                unsupported: Vec::new(),
            })),
            Ask::FindFunction { pr, query } => {
                let hits = fixture::demo_find(&query);
                tells.push(Tell::Calls(CallsTell::Found { pr, query, hits }));
            }
            Ask::ReadFunctionCode { pr, function } => {
                let code = fixture::demo_function_code(&function);
                tells.push(Tell::Calls(CallsTell::Code { pr, function, code }));
            }
            Ask::AddCallsComment {
                pr,
                function,
                body,
                ticket,
            } => {
                let index = fixture::demo_calls();
                let changed = fixture::demo_changed(&index);
                let path = index
                    .functions
                    .iter()
                    .find(|f| f.id.0 == function)
                    .map(|f| f.path.clone());
                let patch = path.and_then(|p| {
                    tabs.0
                        .get(&pr)?
                        .ready()?
                        .view
                        .diff
                        .iter()
                        .find(|f| f.path == p)?
                        .patch
                        .clone()
                });
                tells.extend(demo_edit(&tabs, &pr, ticket, |review| {
                    crate::screens::review::calls::demo_node_comment(
                        review,
                        &index,
                        &changed,
                        &function,
                        patch.as_deref(),
                        &body,
                        now,
                    )
                }));
            }
            Ask::RemoveItem { pr, id } => {
                let review = tabs.0.get(&pr).and_then(|t| t.ready()).map(|r| {
                    let mut review = r.view.review.clone();
                    let _ = review.draft.remove(&id);
                    review
                });
                if let Some(review) = review {
                    tells.push(Tell::ReviewFile(Box::new(review)));
                }
            }
            Ask::Publish { pr, verdict, .. } => tells.push(Tell::Published {
                result: PublishResult {
                    url: Some(format!(
                        "https://github.com/{}/{}/pull/{}#pullrequestreview-1",
                        pr.owner, pr.repo, pr.number
                    )),
                    closed: verdict == Verdict::ClosePr,
                    unresolved: Vec::new(),
                    close_error: None,
                },
                pr,
            }),
            Ask::CloseReview(pr) | Ask::Discard(pr) => tells.push(Tell::Left(pr)),
            // The window already applied the change to its copy of the set; the demo has no
            // other store, so the daemon's answer is that copy.
            Ask::GetHiddenComments(pr)
            | Ask::SetCommentHidden { pr, .. }
            | Ask::ShowComments { pr, .. } => {
                let set = tabs
                    .0
                    .get(&pr)
                    .map(|t| t.ui.hidden.clone())
                    .unwrap_or_default();
                tells.push(Tell::Hidden { pr, set });
            }
            // Nothing keeps the demo's tabs.
            Ask::SetOpenTabs(_) => {}
            Ask::SearchGifs { query, offset } => {
                tells.push(Tell::Gifs(Ok(fixture::demo_gif_page(&query, offset))));
            }
            Ask::FetchMedia(url) => tells.push(demo_media_tell(url)),
            Ask::GetTests(pr) => {
                let head = tabs
                    .0
                    .get(&pr)
                    .and_then(|t| t.ready())
                    .map(|r| r.view.review.head_sha.clone());
                if let Some(head) = head {
                    tells.push(Tell::Tests(TestsTell::Loaded {
                        pr,
                        view: Box::new(TestsView::new(head)),
                    }));
                }
            }
            Ask::GetTestDisk => tells.push(Tell::TestDisk(Ok(fixture::demo_test_disk()))),
            Ask::CheckTestEngine { .. } => {
                tells.push(Tell::TestEngine(Ok(fixture::demo_test_engine())));
            }
            Ask::GenerateTests(pr) => {
                demo_refused(&mut toasts, &time, &mut tells, pr, TestsAskKind::Generate);
            }
            Ask::PrepareTests { pr, .. } => {
                demo_refused(&mut toasts, &time, &mut tells, pr, TestsAskKind::Prepare);
            }
            Ask::RunTests { pr, .. } => {
                demo_refused(&mut toasts, &time, &mut tells, pr, TestsAskKind::Run);
            }
            Ask::StopTests(pr) => {
                demo_refused(&mut toasts, &time, &mut tells, pr, TestsAskKind::Stop);
            }
            Ask::ListTests(pr) => {
                demo_refused(&mut toasts, &time, &mut tells, pr, TestsAskKind::List);
            }
            Ask::AcceptRecipe(pr) => {
                demo_refused(&mut toasts, &time, &mut tells, pr, TestsAskKind::Accept);
            }
            Ask::CleanUpTests { .. } => {
                demo_toast(&mut toasts, &time);
                tells.push(Tell::TestDisk(Err(DEMO_NOT_SENT.into())));
            }
            Ask::FetchProfile(login) => tells.push(Tell::Profile {
                profile: fixture::demo_profile(&login)
                    .ok_or_else(|| MediaError::refused("Demo mode knows no such person")),
                login,
            }),
            _ => demo_toast(&mut toasts, &time),
        }
    }
    if !tells.is_empty() {
        for tell in tells {
            let _ = outbox.0.send(tell);
        }
        redraw.write(RequestRedraw);
    }
}

fn demo_toast(toasts: &mut Toasts, time: &Time) {
    toasts.0.push(Toast {
        text: DEMO_NOT_SENT.into(),
        warning: false,
        until: time.elapsed_secs_f64() + TOAST_SECS,
    });
}

/// A tests write ask in demo mode: the toast, and a refusal so the button is free again.
fn demo_refused(
    toasts: &mut Toasts,
    time: &Time,
    tells: &mut Vec<Tell>,
    pr: PrRef,
    ask: TestsAskKind,
) {
    demo_toast(toasts, time);
    tells.push(Tell::Tests(TestsTell::Refused {
        pr,
        ask,
        message: DEMO_NOT_SENT.into(),
    }));
}

/// The demo's copy of `url`, written under the temp folder where the window may read it.
fn demo_media_tell(url: String) -> Tell {
    let file = fixture::demo_media(&url)
        .ok_or_else(|| MediaError::refused("Demo mode has no copy of this picture"))
        .and_then(|(kind, bytes)| {
            let dir = std::env::temp_dir().join("clusia-demo-media");
            let path = dir.join(format!(
                "{}.{}",
                clusia_core::media::cache_key(&url),
                kind.extension()
            ));
            std::fs::create_dir_all(&dir)
                .and_then(|()| std::fs::write(&path, &bytes))
                .map_err(|e| MediaError::transient(e.to_string()))?;
            Ok(MediaFile {
                path: path.display().to_string(),
                kind,
                bytes: bytes.len() as u64,
            })
        });
    Tell::Media { url, file }
}

/// The demo review opens at once (every step done, no harness); any other PR fails at the
/// repository step, which shows the failure screen.
fn demo_open(pr: PrRef, now: i64) -> Vec<Tell> {
    let step = |step, status, message: &str| {
        Tell::Step(LoadStep {
            pr: pr.clone(),
            step,
            status,
            message: Some(message.to_string()),
        })
    };
    if pr != fixture::demo_pr() {
        return vec![
            step(
                LoadStepKind::Repo,
                StepStatus::Failed,
                "Demo mode has no copy of this repository",
            ),
            Tell::OpenFailed {
                message: format!(
                    "Demo mode only has {} — nothing is fetched from GitHub.",
                    fixture::demo_pr()
                ),
                pr,
                cache: false,
            },
        ];
    }
    let (view, news) = fixture::demo_review(now);
    vec![
        step(LoadStepKind::Repo, StepStatus::Done, "~/Repos/clusia"),
        step(
            LoadStepKind::Branch,
            StepStatus::Done,
            view.worktree.as_deref().unwrap_or(""),
        ),
        step(LoadStepKind::Pr, StepStatus::Done, "7 files"),
        step(
            LoadStepKind::Agent,
            StepStatus::Skipped,
            "no harness set up",
        ),
        Tell::Opened {
            pr,
            view: Box::new(view),
            news,
        },
    ]
}

/// The demo agent's answer to a question: it works, reads a file, says what it is, and is
/// ready again. Nothing leaves the window. A question about tests stops at the permission
/// request instead, unless a rule granted for the review covers it.
fn demo_agent_reply(
    pr: &PrRef,
    text: &str,
    now_ms: i64,
    rule_granted: bool,
    name: &str,
) -> Vec<Tell> {
    let turn = 3;
    let agent = Tell::Agent;
    let mut tells = vec![
        agent(AgentTell::State {
            pr: pr.clone(),
            state: SessionStateKind::Running,
        }),
        agent(AgentTell::ToolUse {
            pr: pr.clone(),
            turn,
            summary: "Read src/auth/refresh.rs".into(),
        }),
    ];
    // A rule granted for the review already covers `cargo test`, so nothing is asked again.
    if text.to_lowercase().contains("test") && !rule_granted {
        // The turn waits here until `demo_permission_answer` says how the request ended.
        tells.push(Tell::Permission(PermissionTell::Requested(
            fixture::demo_permission_request(now_ms),
        )));
        return tells;
    }
    let chunk = |text: String| {
        agent(AgentTell::Chunk {
            pr: pr.clone(),
            turn,
            text,
        })
    };
    tells.extend([
        chunk(format!("This is demo data: nothing was sent to {name}. ")),
        chunk(format!(
            "With a real session, the answer to “{}” would stream here.",
            text.trim()
        )),
        agent(AgentTell::Done {
            pr: pr.clone(),
            turn,
            duration_ms: 900,
        }),
        agent(AgentTell::State {
            pr: pr.clone(),
            state: SessionStateKind::Ready,
        }),
    ]);
    tells
}

/// How the demo request ends for each answer, then the rest of the turn. An id nobody asked
/// about changes nothing (the daemon's first-answer-wins rule).
fn demo_permission_answer(
    queue: &PermissionQueue,
    id: &str,
    answer: PermissionAnswerKind,
) -> Vec<Tell> {
    let Some(pending) = queue.get(id) else {
        return Vec::new();
    };
    let (pr, turn) = (pending.request.pr.clone(), pending.request.turn);
    let (outcome, ran) = match answer {
        PermissionAnswerKind::Once => (PermissionOutcome::Allowed, true),
        PermissionAnswerKind::Review => (PermissionOutcome::AllowedForReview, true),
        PermissionAnswerKind::Deny => (PermissionOutcome::Denied, false),
    };
    let mut tells = vec![Tell::Permission(PermissionTell::Resolved {
        id: id.to_string(),
        pr: pr.clone(),
        tool: pending.request.tool.clone(),
        summary: pending.request.summary.clone(),
        outcome,
        origin: pending.request.origin,
    })];
    if answer == PermissionAnswerKind::Review {
        tells.push(Tell::Permission(PermissionTell::Rules {
            pr: pr.clone(),
            rules: fixture::demo_permission_rules(),
        }));
    }
    let said = if ran {
        "This is demo data: nothing was run. With a real session the tests would run here and their result would stream."
    } else {
        "This is demo data. You denied the command, so the agent goes on without running it."
    };
    tells.extend([
        Tell::Agent(AgentTell::Chunk {
            pr: pr.clone(),
            turn,
            text: said.into(),
        }),
        Tell::Agent(AgentTell::Done {
            pr: pr.clone(),
            turn,
            duration_ms: 1_200,
        }),
        Tell::Agent(AgentTell::State {
            pr,
            state: SessionStateKind::Ready,
        }),
    ]);
    tells
}

/// The demo's accept of a finding: a draft item with the finding's origin (a line comment, or
/// a general note when the finding has no anchor), then `Settled`.
fn demo_accept_finding(
    tabs: &ReviewTabs,
    pr: &PrRef,
    id: &str,
    body: Option<String>,
    now: i64,
) -> Vec<Tell> {
    // The card waits for the answer: a refusal frees it.
    let warn = |text: String| {
        vec![
            Tell::Notice {
                text,
                warning: true,
            },
            Tell::Checks(ChecksTell::Refused {
                pr: pr.clone(),
                id: id.to_string(),
            }),
        ]
    };
    let (results, _) = fixture::demo_checks(now);
    let Some(finding) = results
        .into_iter()
        .flat_map(|r| r.findings)
        .find(|f| f.id == id)
    else {
        return warn(format!("Demo mode has no finding {id}"));
    };
    let Some(ready) = tabs.0.get(pr).and_then(|t| t.ready()) else {
        return warn("This review is not open".into());
    };
    let mut review = ready.view.review.clone();
    let (kind, anchor) = match (finding.anchored, finding.line.or(finding.end_line)) {
        (true, Some(line)) => (
            DraftKind::LineComment,
            Some(Anchor {
                commit: review.head_sha.clone(),
                path: finding.file.clone(),
                line,
                start_line: finding.start_line,
                side: Side::Right,
            }),
        ),
        _ => (DraftKind::General, None),
    };
    let origin = match finding.kind {
        CheckKind::Security => Origin::Security,
        CheckKind::Audit => Origin::Audit,
    };
    // What the card shows is what is added.
    let text = body
        .unwrap_or_else(|| clusia_core::printable::printable_lines(&proposed_comment(&finding)));
    match review
        .draft
        .add_as(origin, kind, anchor, None, &text, now)
        .map(|_| ())
        .map_err(|e| e.to_string())
    {
        Ok(()) => vec![
            Tell::ReviewFile(Box::new(review)),
            Tell::Checks(ChecksTell::Settled {
                pr: pr.clone(),
                id: id.to_string(),
                accepted: true,
            }),
        ],
        Err(message) => warn(message),
    }
}

/// Accepting a demo suggestion adds its draft item the way the daemon does: a line comment
/// on the head side, the agent's, accepted. `body` is the edited text, if any.
fn demo_accept(
    tabs: &ReviewTabs,
    pr: &PrRef,
    id: &str,
    body: Option<String>,
    now: i64,
) -> Vec<Tell> {
    let warn = |text: String| {
        vec![Tell::Notice {
            text,
            warning: true,
        }]
    };
    let Some(s) = fixture::demo_suggestions().into_iter().find(|s| s.id == id) else {
        return warn(format!("Demo mode has no suggestion {id}"));
    };
    let Some(ready) = tabs.0.get(pr).and_then(|t| t.ready()) else {
        return warn("This review is not open".into());
    };
    let mut review = ready.view.review.clone();
    let Some(line) = s.end_line.or(s.line) else {
        return warn("The suggestion has no line".into());
    };
    let anchor = Anchor {
        commit: review.head_sha.clone(),
        path: s.file.clone(),
        line,
        start_line: s.start_line,
        side: Side::Right,
    };
    let text = body.unwrap_or(s.body);
    let added = review
        .draft
        .add_as(
            Origin::Agent,
            DraftKind::LineComment,
            Some(anchor),
            None,
            &text,
            now,
        )
        .map(|_| ())
        .map_err(|e| e.to_string());
    match added {
        Ok(()) => vec![
            Tell::ReviewFile(Box::new(review)),
            Tell::Agent(AgentTell::Handled {
                pr: pr.clone(),
                id: id.to_string(),
                accepted: true,
            }),
        ],
        Err(message) => warn(message),
    }
}

/// A draft write on the demo review: `ReviewFile` + `Saved`, or `Refused` with the message.
/// Demo mode: decides one proposal (or all of them, `item: None`) with the daemon's own rules,
/// on a copy of the review the tab holds.
fn demo_decide(
    tabs: &ReviewTabs,
    revalidations: &Revalidations,
    pr: &PrRef,
    item: Option<&str>,
    accept: bool,
) -> Vec<Tell> {
    let refuse = |text: String| {
        vec![
            Tell::Notice {
                text,
                warning: true,
            },
            Tell::Revalidation(RevalidationTell::Refused {
                pr: pr.clone(),
                item: item.map(String::from),
            }),
        ]
    };
    let Some(ready) = tabs.0.get(pr).and_then(|t| t.ready()) else {
        return refuse("This review is not open".into());
    };
    let mut review = ready.view.review.clone();
    // The round the screen shows is the one that is decided.
    if let Some(round) = revalidations.0.get(pr).and_then(|m| m.round.clone()) {
        review.revalidation = Some(round);
    }
    let done = match item {
        Some(id) => revalidation::decide(&mut review, id, accept, &ready.view.diff)
            .map_err(|e| e.to_string()),
        None => {
            revalidation::accept_all(&mut review, &ready.view.diff);
            Ok(())
        }
    };
    match done {
        Ok(()) => vec![
            Tell::Revalidation(RevalidationTell::Round {
                pr: pr.clone(),
                round: review.revalidation.clone(),
            }),
            Tell::ReviewFile(Box::new(review)),
            Tell::Revalidation(RevalidationTell::Settled {
                pr: pr.clone(),
                item: item.map(String::from),
            }),
        ],
        Err(message) => refuse(message),
    }
}

/// Demo mode: points a draft item elsewhere with the daemon's own rule, on a copy of the review.
fn demo_reanchor(
    tabs: &ReviewTabs,
    pr: &PrRef,
    id: &str,
    anchor: Option<AnchorInput>,
) -> Vec<Tell> {
    let refuse = |text: String| {
        vec![
            Tell::Notice {
                text,
                warning: true,
            },
            Tell::Revalidation(RevalidationTell::Refused {
                pr: pr.clone(),
                item: Some(id.to_string()),
            }),
        ]
    };
    let Some(ready) = tabs.0.get(pr).and_then(|t| t.ready()) else {
        return refuse("This review is not open".into());
    };
    let mut review = ready.view.review.clone();
    let anchor = anchor.map(|a| Anchor {
        commit: match a.side {
            Side::Right => review.head_sha.clone(),
            Side::Left => review.base_sha.clone(),
        },
        path: a.path,
        line: a.line,
        start_line: a.start_line,
        side: a.side,
    });
    match review.draft.reanchor(id, anchor) {
        Ok(()) => {
            // A comment that has a place again is no longer a removal.
            review.removed.remove(id);
            vec![
                Tell::ReviewFile(Box::new(review)),
                Tell::Revalidation(RevalidationTell::Reanchored {
                    pr: pr.clone(),
                    item: id.to_string(),
                }),
            ]
        }
        Err(e) => refuse(e.to_string()),
    }
}

fn demo_edit(
    tabs: &ReviewTabs,
    pr: &PrRef,
    ticket: u64,
    edit: impl FnOnce(&mut Review) -> Result<(), String>,
) -> Vec<Tell> {
    let Some(ready) = tabs.0.get(pr).and_then(|t| t.ready()) else {
        return vec![Tell::Refused {
            pr: pr.clone(),
            ticket,
            message: "This review is not open".into(),
        }];
    };
    let mut review = ready.view.review.clone();
    match edit(&mut review) {
        Ok(()) => vec![
            Tell::ReviewFile(Box::new(review)),
            Tell::Saved {
                pr: pr.clone(),
                ticket,
            },
        ],
        Err(message) => vec![Tell::Refused {
            pr: pr.clone(),
            ticket,
            message,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_answer_can_be_tried_again_unless_the_request_is_gone() {
        let gone = ProtocolError::new(ErrorCode::NotFound, "no such request");
        assert!(refused_answer("perm-1", &gone).is_empty());
        let busy = ProtocolError::new(ErrorCode::Busy, "try later");
        assert_eq!(
            refused_answer("perm-1", &busy),
            [
                Tell::Permission(PermissionTell::AnswerRefused {
                    id: "perm-1".into()
                }),
                Tell::Notice {
                    text: "try later".into(),
                    warning: true
                },
            ]
        );
    }

    #[test]
    fn every_gif_search_gets_an_answer() {
        let page = clusia_protocol::GifPage {
            items: Vec::new(),
            next_offset: None,
        };
        assert_eq!(gifs_tell(Reply::Gifs(page.clone())), Tell::Gifs(Ok(page)));
        assert_eq!(
            gifs_tell(Reply::Ack),
            Tell::Gifs(Err((
                ErrorCode::Internal,
                "Unexpected reply from clusiad".into()
            )))
        );
    }

    #[test]
    fn offline_gif_searches_fail_at_once() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let teller = Teller {
            tx,
            wake: Arc::new(|| {}),
        };
        let ask = Ask::SearchGifs {
            query: "turtle".into(),
            offset: 0,
        };
        assert!(offline(&ask, &teller));
        assert_eq!(
            rx.try_recv().ok(),
            Some(Tell::Gifs(Err((
                ErrorCode::Offline,
                "Not connected to clusiad".into()
            ))))
        );
    }

    #[tokio::test]
    async fn a_failed_open_is_no_longer_followed() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let teller = Teller {
            tx,
            wake: Arc::new(|| {}),
        };
        let pr = fixture::demo_pr();
        let open = OpenSet::default();
        open_set(&open).insert(pr.clone());
        let socket = PathBuf::from("/nonexistent/clusia-test/clusiad.sock");
        work(socket, teller, open.clone(), Ask::OpenReview(pr.clone())).await;
        assert!(matches!(
            rx.try_recv(),
            Ok(Tell::OpenFailed { pr: failed, cache: false, .. }) if failed == pr
        ));
        assert!(open_set(&open).is_empty(), "the failed PR is forgotten");
    }

    #[tokio::test]
    async fn a_media_fetch_without_a_daemon_says_why() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let teller = Teller {
            tx,
            wake: Arc::new(|| {}),
        };
        let url = "https://github.com/user-attachments/assets/1.png".to_string();
        let socket = PathBuf::from("/nonexistent/clusia-test/clusiad.sock");
        work(
            socket,
            teller,
            OpenSet::default(),
            Ask::FetchMedia(url.clone()),
        )
        .await;
        assert!(matches!(
            rx.try_recv(),
            Ok(Tell::Media { url: asked, file: Err(e) }) if asked == url && e.transient && e.message.contains("cannot reach clusiad")
        ));
    }

    #[test]
    fn only_offline_and_internal_daemon_errors_are_transient() {
        let class = |code| {
            media_error(ClientError::Server(clusia_protocol::ProtocolError {
                code,
                message: "m".into(),
            }))
            .transient
        };
        assert!(class(ErrorCode::Offline));
        assert!(class(ErrorCode::Internal));
        assert!(!class(ErrorCode::Refused));
        assert!(!class(ErrorCode::BadRequest));
        assert!(!class(ErrorCode::NotFound));
        assert!(!class(ErrorCode::NotConfigured));
        assert!(media_error(ClientError::Closed).transient);
    }

    #[test]
    fn only_a_missing_user_is_final_for_a_profile() {
        let class = |code| {
            profile_error(ClientError::Server(clusia_protocol::ProtocolError {
                code,
                message: "m".into(),
            }))
            .transient
        };
        for code in [
            ErrorCode::RateLimited,
            ErrorCode::Upstream,
            ErrorCode::Unauthorized,
            ErrorCode::Offline,
        ] {
            assert!(
                class(code),
                "{code:?} may pass, so the next hover asks again"
            );
        }
        assert!(!class(ErrorCode::NotFound));
        assert!(profile_error(ClientError::Closed).transient);
    }

    #[test]
    fn offline_media_asks_fail_at_once() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let teller = Teller {
            tx,
            wake: Arc::new(|| {}),
        };
        assert!(offline(
            &Ask::FetchMedia("https://github.com/a.png".into()),
            &teller
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(Tell::Media { file: Err(e), .. }) if e.transient && e.message == "Not connected to clusiad"
        ));
    }

    /// Runs `demo_answers` on `asks` and returns the tells it produced.
    fn demo_tells(asks: Vec<Ask>) -> Vec<Tell> {
        let (inbox, outbox) = local_link();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .insert_resource(Asks {
                recorded: asks,
                ..Asks::default()
            })
            .insert_resource(Model::default())
            .insert_resource(Toasts::default())
            .insert_resource(ReviewTabs::default())
            .init_resource::<Revalidations>()
            .init_resource::<Stories>()
            .init_resource::<Calls>()
            .init_resource::<TestsModels>()
            .init_resource::<TestMachine>()
            .insert_resource(Clock(Some(1_790_000_000)))
            .insert_resource(PermissionQueue::default())
            .insert_resource(outbox)
            .add_systems(Update, demo_answers);
        app.update();
        inbox.0.try_iter().collect()
    }

    #[test]
    fn the_demo_answers_the_hidden_comment_asks() {
        let pr = fixture::demo_pr();
        let tells = demo_tells(vec![
            Ask::GetHiddenComments(pr.clone()),
            Ask::SetCommentHidden {
                pr: pr.clone(),
                key: "thread:T1".into(),
                posts: Some(2),
            },
            Ask::ShowComments {
                pr: pr.clone(),
                keys: vec!["thread:T1".into()],
            },
        ]);
        assert_eq!(tells.len(), 3);
        assert!(
            tells
                .iter()
                .all(|t| matches!(t, Tell::Hidden { pr: p, .. } if *p == pr))
        );
    }

    #[test]
    fn choosing_codex_in_demo_mode_leaves_no_program() {
        use clusia_core::config::HarnessKind;
        let (_inbox, outbox) = local_link();
        let mut model = Model::default();
        model.snapshot.config.harness.program = Some("/opt/homebrew/bin/claude".into());
        let mut asks = Asks::default();
        crate::screens::config::harness::write_kind(&mut asks, &mut model, "codex");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .insert_resource(asks)
            .insert_resource(model)
            .insert_resource(Toasts::default())
            .insert_resource(ReviewTabs::default())
            .init_resource::<Revalidations>()
            .init_resource::<Stories>()
            .init_resource::<Calls>()
            .init_resource::<TestsModels>()
            .init_resource::<TestMachine>()
            .insert_resource(Clock(Some(1_790_000_000)))
            .insert_resource(PermissionQueue::default())
            .insert_resource(outbox)
            .add_systems(Update, demo_answers);
        app.update();
        let config = &app.world().resource::<Model>().snapshot.config;
        assert_eq!(config.harness.kind, HarnessKind::Codex);
        assert_eq!(config.harness.program, None);
    }

    /// The queue as the window has it after the demo request was told.
    fn demo_queue() -> PermissionQueue {
        let mut queue = PermissionQueue::default();
        let now_ms = 1_790_000_000_000;
        queue.push(fixture::demo_permission_request(now_ms), now_ms);
        queue
    }

    /// The demo's answers to `asks`, with the demo review open and ready in the tabs.
    fn demo_agent_tells(asks: Vec<Ask>) -> Vec<Tell> {
        use crate::review_state::{Phase, Ready, Tab, TabUi};
        let (inbox, outbox) = local_link();
        let mut tabs = ReviewTabs::default();
        tabs.0.insert(
            fixture::demo_pr(),
            Tab {
                phase: Phase::Ready(Box::new(Ready {
                    view: fixture::demo_review(1_790_000_000).0,
                    news: Vec::new(),
                    cached_at: None,
                })),
                ui: TabUi::default(),
            },
        );
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .insert_resource(Asks {
                recorded: asks,
                ..Asks::default()
            })
            .insert_resource(Model::default())
            .insert_resource(Toasts::default())
            .insert_resource(tabs)
            .init_resource::<Revalidations>()
            .init_resource::<Stories>()
            .init_resource::<Calls>()
            .init_resource::<TestsModels>()
            .init_resource::<TestMachine>()
            .insert_resource(Clock(Some(1_790_000_000)))
            .insert_resource(demo_queue())
            .insert_resource(outbox)
            .add_systems(Update, demo_answers);
        app.update();
        inbox.0.try_iter().collect()
    }

    #[test]
    fn demo_mode_reads_a_boxs_code_and_files_its_comment() {
        use crate::review_state::{Phase, Ready, Tab, TabUi};
        use crate::screens::review::diagrams::{Diagrams, DiagramsModel, modal};
        let pr = fixture::demo_pr();
        let (view, _) = fixture::demo_review(1_790_000_000);
        let version =
            modal::test_version(&view.review.head_sha, Some(("src/auth/refresh.rs", 38, 48)));
        let mut model = DiagramsModel::default();
        model.loaded.insert(2, version);
        let mut diagrams = Diagrams::default();
        diagrams.0.insert(pr.clone(), model);
        let mut tabs = ReviewTabs::default();
        tabs.0.insert(
            pr.clone(),
            Tab {
                phase: Phase::Ready(Box::new(Ready {
                    view,
                    news: Vec::new(),
                    cached_at: None,
                })),
                ui: TabUi::default(),
            },
        );
        let (inbox, outbox) = local_link();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .insert_resource(Asks {
                recorded: vec![
                    Ask::ReadDiagramCode {
                        pr: pr.clone(),
                        number: 2,
                        key: "flow".into(),
                        node: "refresh".into(),
                    },
                    Ask::AddDiagramComment {
                        pr: pr.clone(),
                        number: 2,
                        key: "flow".into(),
                        node: "refresh".into(),
                        body: "Can the lock end sooner?".into(),
                        ticket: 4,
                    },
                ],
                ..Asks::default()
            })
            .insert_resource(Model::default())
            .insert_resource(Toasts::default())
            .insert_resource(tabs)
            .insert_resource(diagrams)
            .init_resource::<Revalidations>()
            .init_resource::<Stories>()
            .init_resource::<Calls>()
            .init_resource::<TestsModels>()
            .init_resource::<TestMachine>()
            .insert_resource(Clock(Some(1_790_000_000)))
            .insert_resource(PermissionQueue::default())
            .insert_resource(outbox)
            .add_systems(Update, demo_answers);
        app.update();
        let tells: Vec<Tell> = inbox.0.try_iter().collect();
        assert_eq!(tells.len(), 3, "{tells:?}");
        let Tell::Diagrams(DiagramsTell::Code { number, code, .. }) = &tells[0] else {
            panic!("the code first: {:?}", tells[0])
        };
        assert_eq!(*number, 2);
        assert_eq!(code.lines.len(), 11);
        assert!(code.lines.iter().any(|l| l.number == 44 && l.changed));
        let Tell::ReviewFile(review) = &tells[1] else {
            panic!("the review next: {:?}", tells[1])
        };
        let item = review.draft.items.last().expect("an item");
        assert_eq!(item.origin, Origin::Diagram);
        assert_eq!(item.kind, DraftKind::LineComment);
        assert_eq!(item.body, "Can the lock end sooner?");
        assert_eq!(tells[2], Tell::Saved { pr, ticket: 4 });
    }

    #[test]
    fn demo_mode_replays_the_agent_log_and_the_probe() {
        let pr = fixture::demo_pr();
        let tells = demo_agent_tells(vec![Ask::AgentLog { pr: pr.clone() }, Ask::Probe]);
        assert_eq!(
            tells,
            [
                Tell::AgentLog {
                    pr: pr.clone(),
                    entries: fixture::demo_agent_log(1_790_000_000)
                },
                Tell::Agent(AgentTell::State {
                    pr,
                    state: SessionStateKind::Ready
                }),
                Tell::Probe(fixture::demo_probe()),
            ]
        );
        let other: PrRef = "rzorzal/other#1".parse().unwrap();
        assert!(demo_agent_tells(vec![Ask::AgentLog { pr: other }]).is_empty());
    }

    #[test]
    fn demo_mode_answers_a_question_with_a_streamed_demo_reply() {
        let pr = fixture::demo_pr();
        let tells = demo_agent_tells(vec![Ask::AgentSend {
            pr: pr.clone(),
            text: "Why a lock?".into(),
        }]);
        let agent: Vec<&AgentTell> = tells
            .iter()
            .filter_map(|t| match t {
                Tell::Agent(a) => Some(a),
                _ => None,
            })
            .collect();
        assert!(matches!(
            agent.first(),
            Some(AgentTell::State {
                state: SessionStateKind::Running,
                ..
            })
        ));
        assert!(matches!(
            agent.last(),
            Some(AgentTell::State {
                state: SessionStateKind::Ready,
                ..
            })
        ));
        let answer: String = agent
            .iter()
            .filter_map(|a| match a {
                AgentTell::Chunk { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            answer.contains("demo data") && answer.contains("Why a lock?"),
            "{answer}"
        );
        assert!(agent.iter().any(|a| matches!(a, AgentTell::ToolUse { .. })));
        assert!(agent.iter().any(|a| matches!(a, AgentTell::Done { .. })));
        assert!(agent.iter().all(|a| a.pr() == &pr));
    }

    #[test]
    fn demo_mode_accepts_a_suggestion_into_the_draft_as_the_agents() {
        use clusia_core::draft::Origin;
        let pr = fixture::demo_pr();
        let s = fixture::demo_suggestion();
        let before = fixture::demo_review(1_790_000_000)
            .0
            .review
            .draft
            .items
            .len();
        let tells = demo_agent_tells(vec![Ask::AcceptSuggestion {
            pr: pr.clone(),
            id: s.id.clone(),
            body: Some("Edited text.".into()),
        }]);
        let Some(Tell::ReviewFile(review)) = tells.first() else {
            panic!("{tells:?}")
        };
        assert_eq!(review.draft.items.len(), before + 1);
        let item = review.draft.items.last().unwrap();
        assert_eq!(item.origin, Origin::Agent);
        assert!(item.accepted);
        assert_eq!(item.body, "Edited text.");
        let anchor = item.anchor.as_ref().unwrap();
        assert_eq!(
            (anchor.path.as_str(), anchor.line),
            ("src/auth/refresh.rs", 44)
        );
        assert_eq!(
            tells.last(),
            Some(&Tell::Agent(AgentTell::Handled {
                pr: pr.clone(),
                id: s.id.clone(),
                accepted: true
            }))
        );
        // An unknown suggestion is a warning, not a draft item.
        let tells = demo_agent_tells(vec![Ask::AcceptSuggestion {
            pr,
            id: "sug-000000000000".into(),
            body: None,
        }]);
        assert!(matches!(&tells[..], [Tell::Notice { warning: true, .. }]));
    }

    #[test]
    fn demo_mode_dismisses_and_stops() {
        let pr = fixture::demo_pr();
        let tells = demo_agent_tells(vec![
            Ask::DismissSuggestion {
                pr: pr.clone(),
                id: "sug-5f0c1d2e3a4b".into(),
            },
            Ask::AgentCancel { pr: pr.clone() },
        ]);
        assert_eq!(
            tells,
            [
                Tell::Agent(AgentTell::Handled {
                    pr: pr.clone(),
                    id: "sug-5f0c1d2e3a4b".into(),
                    accepted: false
                }),
                Tell::Agent(AgentTell::State {
                    pr,
                    state: SessionStateKind::Ready
                }),
            ]
        );
    }

    const DEMO_COMMAND: &str = "cargo test -p clusia-auth refresh_race -- --nocapture";

    fn permission_tells(tells: &[Tell]) -> Vec<&PermissionTell> {
        tells
            .iter()
            .filter_map(|t| match t {
                Tell::Permission(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_demo_question_about_tests_raises_the_permission_request() {
        let pr = fixture::demo_pr();
        let tells = demo_agent_tells(vec![Ask::AgentSend {
            pr: pr.clone(),
            text: "Run the tests".into(),
        }]);
        assert!(matches!(
            tells.first(),
            Some(Tell::Agent(AgentTell::State {
                state: SessionStateKind::Running,
                ..
            }))
        ));
        assert_eq!(
            permission_tells(&tells),
            [&PermissionTell::Requested(
                fixture::demo_permission_request(1_790_000_000_000)
            )]
        );
        assert!(
            !tells
                .iter()
                .any(|t| matches!(t, Tell::Agent(AgentTell::Done { .. }))),
            "the turn waits for the answer"
        );
        let plain = demo_agent_tells(vec![Ask::AgentSend {
            pr,
            text: "Why a lock?".into(),
        }]);
        assert!(
            permission_tells(&plain).is_empty(),
            "other questions answer as before"
        );
    }

    #[test]
    fn a_granted_rule_covers_the_next_demo_question_about_tests() {
        use clusia_protocol::PermissionAnswerKind;
        let pr = fixture::demo_pr();
        let ask_tests = || Ask::AgentSend {
            pr: fixture::demo_pr(),
            text: "Run the tests".into(),
        };
        let covered = demo_agent_tells(vec![
            Ask::PermissionAnswer {
                id: fixture::demo_permission_request(0).id,
                answer: PermissionAnswerKind::Review,
            },
            ask_tests(),
        ]);
        let requested = |tells: &[Tell]| {
            permission_tells(tells)
                .iter()
                .any(|p| matches!(p, PermissionTell::Requested(_)))
        };
        assert!(!requested(&covered), "the rule already allows cargo test");
        let revoked = demo_agent_tells(vec![
            Ask::PermissionAnswer {
                id: fixture::demo_permission_request(0).id,
                answer: PermissionAnswerKind::Review,
            },
            Ask::RevokeRule {
                pr,
                rule: "Bash(cargo test:*)".into(),
            },
            ask_tests(),
        ]);
        assert!(requested(&revoked), "a revoked rule asks again");
    }

    #[test]
    fn answering_the_demo_request_finishes_the_turn() {
        use clusia_protocol::{PermissionAnswerKind, PermissionOutcome};
        let pr = fixture::demo_pr();
        let id = fixture::demo_permission_request(0).id;
        let answer = |answer| {
            demo_agent_tells(vec![Ask::PermissionAnswer {
                id: id.clone(),
                answer,
            }])
        };
        let once = answer(PermissionAnswerKind::Once);
        assert_eq!(
            permission_tells(&once),
            [&PermissionTell::Resolved {
                id: id.clone(),
                pr: pr.clone(),
                tool: "Bash".into(),
                summary: DEMO_COMMAND.into(),
                outcome: PermissionOutcome::Allowed,
                origin: clusia_protocol::TurnOrigin::Chat,
            }]
        );
        let said: String = once
            .iter()
            .filter_map(|t| match t {
                Tell::Agent(AgentTell::Chunk { text, .. }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(said.contains("demo data"), "{said}");
        assert!(matches!(
            once.last(),
            Some(Tell::Agent(AgentTell::State {
                state: SessionStateKind::Ready,
                ..
            }))
        ));

        let review = answer(PermissionAnswerKind::Review);
        assert_eq!(
            permission_tells(&review),
            [
                &PermissionTell::Resolved {
                    id: id.clone(),
                    pr: pr.clone(),
                    tool: "Bash".into(),
                    summary: DEMO_COMMAND.into(),
                    outcome: PermissionOutcome::AllowedForReview,
                    origin: clusia_protocol::TurnOrigin::Chat,
                },
                &PermissionTell::Rules {
                    pr: pr.clone(),
                    rules: fixture::demo_permission_rules()
                },
            ]
        );

        let deny = answer(PermissionAnswerKind::Deny);
        assert_eq!(
            permission_tells(&deny),
            [&PermissionTell::Resolved {
                id: id.clone(),
                pr,
                tool: "Bash".into(),
                summary: DEMO_COMMAND.into(),
                outcome: PermissionOutcome::Denied,
                origin: clusia_protocol::TurnOrigin::Chat,
            }]
        );
        let said: String = deny
            .iter()
            .filter_map(|t| match t {
                Tell::Agent(AgentTell::Chunk { text, .. }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(said.contains("denied"), "{said}");

        let unknown = demo_agent_tells(vec![Ask::PermissionAnswer {
            id: "nope".into(),
            answer: PermissionAnswerKind::Once,
        }]);
        assert!(unknown.is_empty(), "an answer for nothing changes nothing");
    }

    #[test]
    fn revoking_a_demo_rule_empties_the_list() {
        let pr = fixture::demo_pr();
        let tells = demo_agent_tells(vec![Ask::RevokeRule {
            pr: pr.clone(),
            rule: "Bash(cargo test:*)".into(),
        }]);
        assert_eq!(
            permission_tells(&tells),
            [&PermissionTell::Rules { pr, rules: vec![] }]
        );
    }

    #[test]
    fn demo_mode_applies_start_at_login_and_ignores_status_refreshes() {
        let (_inbox, outbox) = local_link();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .insert_resource(Asks {
                recorded: vec![Ask::SetStartAtLogin { on: false }, Ask::RefreshStatus],
                ..Asks::default()
            })
            .insert_resource(Model::default())
            .insert_resource(Toasts::default())
            .insert_resource(ReviewTabs::default())
            .init_resource::<Revalidations>()
            .init_resource::<Stories>()
            .init_resource::<Calls>()
            .init_resource::<TestsModels>()
            .init_resource::<TestMachine>()
            .insert_resource(Clock(Some(1_790_000_000)))
            .insert_resource(PermissionQueue::default())
            .insert_resource(outbox)
            .add_systems(Update, demo_answers);
        app.update();
        let model = app.world().resource::<Model>();
        assert!(!model.snapshot.config.general.start_at_login);
        assert!(model.rejected.is_empty());
        assert!(
            app.world().resource::<Toasts>().0.is_empty(),
            "a status refresh is not worth a toast"
        );
    }

    #[test]
    fn demo_mode_answers_gif_searches_and_media() {
        let url = "https://media.giphy.com/media/demo-party/giphy.gif".to_string();
        let tells = demo_tells(vec![
            Ask::SearchGifs {
                query: "turtle".into(),
                offset: 0,
            },
            Ask::FetchMedia(url.clone()),
            Ask::FetchMedia("https://example.org/a.png".into()),
        ]);
        let [
            Tell::Gifs(Ok(page)),
            Tell::Media { file: Ok(file), .. },
            Tell::Media { file: Err(why), .. },
        ] = &tells[..]
        else {
            panic!("unexpected answers: {tells:?}");
        };
        assert_eq!(
            page.items
                .iter()
                .map(|i| i.title.as_str())
                .collect::<Vec<_>>(),
            ["party turtle"]
        );
        assert_eq!(file.kind, clusia_core::media::MediaKind::Gif);
        let bytes = std::fs::read(&file.path).expect("the demo wrote its GIF");
        assert!(bytes.starts_with(b"GIF8"));
        assert_eq!(bytes.len() as u64, file.bytes);
        assert_eq!(why.message, "Demo mode has no copy of this picture");
        assert!(!why.transient);
    }

    #[test]
    fn a_resolved_request_keeps_who_asked() {
        use clusia_protocol::{PermissionOutcome, TurnOrigin};
        let resolved = |origin| Event::PermissionResolved {
            id: "perm-1".into(),
            pr: fixture::demo_pr(),
            tool: "Bash".into(),
            summary: "cargo test".into(),
            outcome: PermissionOutcome::Cancelled,
            origin,
        };
        for origin in [TurnOrigin::Chat, TurnOrigin::Security, TurnOrigin::Audit] {
            let Some(PermissionTell::Resolved { origin: told, .. }) =
                PermissionTell::from_event(&resolved(origin))
            else {
                panic!("a resolved tell");
            };
            assert_eq!(told, origin);
        }
    }

    #[test]
    fn permission_events_become_permission_tells() {
        use clusia_protocol::PermissionOutcome;
        let pr = fixture::demo_pr();
        let requested = Event::PermissionRequested {
            id: "perm-1".into(),
            pr: pr.clone(),
            turn: 3,
            tool: "Bash".into(),
            summary: "cargo test -p clusia-auth".into(),
            reason: Some("To check the race".into()),
            prefix: Some("cargo test".into()),
            sandbox: true,
            deadline: 1_790_000_112_000,
            detail: Some("old\n→\nnew".into()),
            origin: clusia_protocol::TurnOrigin::Chat,
        };
        assert_eq!(
            PermissionTell::from_event(&requested),
            Some(PermissionTell::Requested(PermissionRequest {
                id: "perm-1".into(),
                pr: pr.clone(),
                turn: 3,
                tool: "Bash".into(),
                summary: "cargo test -p clusia-auth".into(),
                reason: Some("To check the race".into()),
                prefix: Some("cargo test".into()),
                sandbox: true,
                deadline: 1_790_000_112_000,
                detail: Some("old\n→\nnew".into()),
                origin: clusia_protocol::TurnOrigin::Chat,
            }))
        );
        assert_eq!(
            PermissionTell::from_event(&Event::PermissionResolved {
                id: "perm-1".into(),
                pr: pr.clone(),
                tool: "Bash".into(),
                summary: "cargo test -p clusia-auth".into(),
                outcome: PermissionOutcome::Expired,
                origin: clusia_protocol::TurnOrigin::Chat,
            }),
            Some(PermissionTell::Resolved {
                id: "perm-1".into(),
                pr: pr.clone(),
                tool: "Bash".into(),
                summary: "cargo test -p clusia-auth".into(),
                outcome: PermissionOutcome::Expired,
                origin: clusia_protocol::TurnOrigin::Chat,
            })
        );
        assert_eq!(
            PermissionTell::from_event(&Event::RulesChanged {
                pr: pr.clone(),
                rules: vec!["Bash(cargo test:*)".into()],
            }),
            Some(PermissionTell::Rules {
                pr,
                rules: vec!["Bash(cargo test:*)".into()],
            })
        );
        assert_eq!(PermissionTell::from_event(&Event::Stopping), None);
        assert_eq!(
            AgentTell::from_event(&requested),
            None,
            "permission events are not chat events"
        );
    }

    #[test]
    fn demo_mode_writes_the_forced_theme_into_the_snapshot() {
        use clusia_core::config::Theme;
        for (forced, want) in [
            (Some(Theme::Light), Theme::Light),
            (Some(Theme::Dark), Theme::Dark),
            (None, Theme::System),
        ] {
            let home = tempfile::tempdir().unwrap();
            let mut app = App::new();
            app.add_plugins(MinimalPlugins)
                .add_message::<RequestRedraw>()
                .insert_resource(Clock(Some(1_790_000_000)))
                .add_message::<ReviewEvent>()
                .add_message::<AppExit>()
                .init_resource::<MediaCache>()
                .init_resource::<People>()
                .insert_resource(ReviewTabs::default())
                .init_resource::<Chats>()
                .init_resource::<Checks>()
                .init_resource::<Diagrams>()
                .init_resource::<Revalidations>()
                .init_resource::<Stories>()
                .init_resource::<Calls>()
                .init_resource::<TestsModels>()
                .init_resource::<TestMachine>()
                .init_resource::<TestsModels>()
                .init_resource::<TestMachine>()
                .init_resource::<crate::screens::review::agent::PermissionQueue>()
                .add_plugins(BridgePlugin {
                    mode: Mode::Demo {
                        theme: forced,
                        settings: vec![("appearance.text_size".into(), "large".into())],
                    },
                    paths: Paths::new(home.path()),
                    home: None,
                    thread: BridgeSlot::default(),
                });
            app.update();
            assert_eq!(
                app.world()
                    .resource::<Model>()
                    .snapshot
                    .config
                    .appearance
                    .theme,
                want
            );
            let appearance = &app.world().resource::<Model>().snapshot.config.appearance;
            assert_eq!(appearance.text_size, clusia_core::TextSize::Large);
        }
    }

    #[test]
    fn published_and_left_reviews_are_no_longer_followed() {
        let pr = fixture::demo_pr();
        let open = OpenSet::default();
        open_set(&open).insert(pr.clone());
        forget_finished(
            &open,
            &Tell::PublishFailed {
                pr: pr.clone(),
                code: ErrorCode::Upstream,
                message: "boom".into(),
            },
        );
        assert!(open_set(&open).contains(&pr), "a failed publish stays open");
        forget_finished(
            &open,
            &Tell::Published {
                pr: pr.clone(),
                result: PublishResult {
                    url: None,
                    closed: false,
                    unresolved: vec![],
                    close_error: None,
                },
            },
        );
        assert!(open_set(&open).is_empty(), "the published review is gone");
        open_set(&open).insert(pr.clone());
        forget_finished(&open, &Tell::Left(pr));
        assert!(open_set(&open).is_empty());
    }

    #[test]
    fn agent_events_become_agent_tells() {
        use clusia_protocol::{AgentErrorKind, SessionStateKind, Suggestion};
        let pr = fixture::demo_pr();
        let suggestion = Suggestion {
            id: "sug-0123456789ab".into(),
            file: "a.rs".into(),
            line: Some(3),
            start_line: None,
            end_line: None,
            body: "x".into(),
        };
        let cases = [
            (
                Event::AgentChunk {
                    pr: pr.clone(),
                    turn: 2,
                    text: "hi".into(),
                },
                AgentTell::Chunk {
                    pr: pr.clone(),
                    turn: 2,
                    text: "hi".into(),
                },
            ),
            (
                Event::AgentToolUse {
                    pr: pr.clone(),
                    turn: 2,
                    summary: "Read a.rs".into(),
                },
                AgentTell::ToolUse {
                    pr: pr.clone(),
                    turn: 2,
                    summary: "Read a.rs".into(),
                },
            ),
            (
                Event::AgentDenied {
                    pr: pr.clone(),
                    turn: 2,
                    tool: "Bash".into(),
                    detail: "ls".into(),
                },
                AgentTell::Denied {
                    pr: pr.clone(),
                    turn: 2,
                    tool: "Bash".into(),
                    detail: "ls".into(),
                },
            ),
            (
                Event::AgentSuggestion {
                    pr: pr.clone(),
                    turn: 2,
                    suggestion: suggestion.clone(),
                },
                AgentTell::Suggestion {
                    pr: pr.clone(),
                    turn: 2,
                    suggestion,
                },
            ),
            (
                Event::AgentDone {
                    pr: pr.clone(),
                    turn: 2,
                    duration_ms: 5,
                },
                AgentTell::Done {
                    pr: pr.clone(),
                    turn: 2,
                    duration_ms: 5,
                },
            ),
            (
                Event::AgentError {
                    pr: pr.clone(),
                    turn: 2,
                    kind: AgentErrorKind::Crashed,
                    message: "boom".into(),
                },
                AgentTell::Error {
                    pr: pr.clone(),
                    turn: 2,
                    kind: AgentErrorKind::Crashed,
                    message: "boom".into(),
                },
            ),
            (
                Event::SessionState {
                    pr: pr.clone(),
                    state: SessionStateKind::Queued,
                },
                AgentTell::State {
                    pr: pr.clone(),
                    state: SessionStateKind::Queued,
                },
            ),
        ];
        for (event, tell) in cases {
            assert_eq!(
                AgentTell::from_event(&event),
                Some(tell.clone()),
                "{event:?}"
            );
            assert_eq!(tell.pr(), &pr);
        }
        assert_eq!(AgentTell::from_event(&Event::Stopping), None);
    }

    #[test]
    fn refusals_read_like_the_chat() {
        assert_eq!(
            refusal_text(ErrorCode::Busy, "busy"),
            "The agent is busy: wait for the current turn"
        );
        assert_eq!(
            refusal_text(
                ErrorCode::InvalidState,
                "no review for acme/widgets#1; open it first"
            ),
            "no review for acme/widgets#1; open it first"
        );
    }

    fn demo_finding_ids() -> Vec<String> {
        fixture::demo_checks(1_790_000_000).0[0]
            .findings
            .iter()
            .map(|f| f.id.clone())
            .collect()
    }

    #[test]
    fn check_events_become_checks_tells() {
        use clusia_core::checks::CheckKind;
        use clusia_protocol::CheckState;
        let pr = fixture::demo_pr();
        let (results, _) = fixture::demo_checks(1_790_000_000);
        let finding = results[0].findings[0].clone();
        let events = [
            (
                Event::CheckState {
                    pr: pr.clone(),
                    kind: CheckKind::Audit,
                    state: CheckState::Waiting,
                },
                ChecksTell::State {
                    pr: pr.clone(),
                    kind: CheckKind::Audit,
                    state: CheckState::Waiting,
                },
            ),
            (
                Event::CheckFinding {
                    pr: pr.clone(),
                    kind: CheckKind::Security,
                    finding: finding.clone(),
                },
                ChecksTell::Finding {
                    pr: pr.clone(),
                    kind: CheckKind::Security,
                    finding,
                },
            ),
            (
                Event::CheckDone {
                    pr: pr.clone(),
                    kind: CheckKind::Security,
                    result: results[0].clone(),
                },
                ChecksTell::Done {
                    pr: pr.clone(),
                    kind: CheckKind::Security,
                    result: results[0].clone(),
                },
            ),
            (
                Event::FindingSettled {
                    pr: pr.clone(),
                    id: "f1".into(),
                    accepted: true,
                },
                ChecksTell::Settled {
                    pr: pr.clone(),
                    id: "f1".into(),
                    accepted: true,
                },
            ),
        ];
        for (event, tell) in events {
            let got = ChecksTell::from_event(&event);
            assert_eq!(got.as_ref(), Some(&tell));
            assert_eq!(got.unwrap().pr(), &pr);
        }
        assert_eq!(ChecksTell::from_event(&Event::Stopping), None);
        assert!(
            AgentTell::from_event(&Event::FindingSettled {
                pr,
                id: "f1".into(),
                accepted: false
            })
            .is_none()
        );
    }

    #[test]
    fn a_round_event_becomes_a_round_tell() {
        let pr = fixture::demo_pr();
        let event = Event::RevalidationChanged {
            pr: pr.clone(),
            revalidation: None,
        };
        let got = RevalidationTell::from_event(&event).expect("a round tell");
        assert_eq!(
            got,
            RevalidationTell::Round {
                pr: pr.clone(),
                round: None,
            }
        );
        assert_eq!(got.pr(), &pr);
        assert_eq!(RevalidationTell::from_event(&Event::Stopping), None);
        assert!(ChecksTell::from_event(&event).is_none());
        assert!(AgentTell::from_event(&event).is_none());
    }

    #[test]
    fn offline_decisions_are_refused_not_dropped() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let teller = Teller {
            tx,
            wake: Arc::new(|| {}),
        };
        let pr = fixture::demo_pr();
        assert!(offline(
            &Ask::DecideProposal {
                pr: pr.clone(),
                item: "i2".into(),
                accept: true
            },
            &teller
        ));
        assert!(offline(&Ask::AcceptAllProposals(pr.clone()), &teller));
        assert!(offline(&Ask::GetRevalidation(pr.clone()), &teller));
        assert!(offline(&Ask::StopRevalidation(pr.clone()), &teller));
        assert!(offline(&Ask::Revalidate(pr.clone()), &teller));
        let told: Vec<Tell> = rx.try_iter().collect();
        assert!(told.iter().any(|t| matches!(
            t,
            Tell::Revalidation(RevalidationTell::Refused { item: Some(i), .. }) if i == "i2"
        )));
        assert!(told.iter().any(|t| matches!(
            t,
            Tell::Revalidation(RevalidationTell::Refused { item: None, .. })
        )));
        assert_eq!(
            told.iter()
                .filter(|t| matches!(t, Tell::Notice { warning: true, .. }))
                .count(),
            4,
            "a read says nothing"
        );
    }

    #[test]
    fn offline_check_writes_are_refused_not_dropped() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let teller = Teller {
            tx,
            wake: Arc::new(|| {}),
        };
        let pr = fixture::demo_pr();
        assert!(offline(
            &Ask::AcceptFinding {
                pr: pr.clone(),
                id: "f1".into(),
                body: None
            },
            &teller
        ));
        let told: Vec<Tell> = rx.try_iter().collect();
        assert!(told.iter().any(|t| matches!(
            t,
            Tell::Checks(ChecksTell::Refused { id, .. }) if id == "f1"
        )));
        assert!(
            told.iter()
                .any(|t| matches!(t, Tell::Notice { warning: true, .. }))
        );
        assert!(offline(&Ask::GetChecks(pr), &teller));
        assert!(
            rx.try_iter().next().is_none(),
            "a read is not worth a toast"
        );
    }

    #[test]
    fn diagram_events_become_diagrams_tells() {
        let pr = fixture::demo_pr();
        let versions = fixture::demo_diagrams(1_790_000_000, &pr);
        let planned = versions[1].clone();
        let cases = [
            (
                Event::DiagramsChanged {
                    pr: pr.clone(),
                    state: DiagramsState::Planning,
                },
                DiagramsTell::State {
                    pr: pr.clone(),
                    state: DiagramsState::Planning,
                },
            ),
            (
                Event::DiagramsPlanned {
                    pr: pr.clone(),
                    version: Box::new(planned.clone()),
                },
                DiagramsTell::Planned {
                    pr: pr.clone(),
                    version: Box::new(planned),
                },
            ),
            (
                Event::DiagramEntryChanged {
                    pr: pr.clone(),
                    number: 2,
                    key: "flow".into(),
                    state: EntryState::Drawing,
                },
                DiagramsTell::Entry {
                    pr: pr.clone(),
                    number: 2,
                    key: "flow".into(),
                    state: EntryState::Drawing,
                },
            ),
        ];
        for (event, want) in cases {
            assert_eq!(DiagramsTell::from_event(&event), Some(want));
        }
        assert_eq!(DiagramsTell::from_event(&Event::Stopping), None);
    }

    #[test]
    fn offline_diagram_asks_are_answered_not_dropped() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let teller = Teller {
            tx,
            wake: Arc::new(|| {}),
        };
        let pr = fixture::demo_pr();
        assert!(offline(&Ask::GetDiagrams(pr.clone()), &teller));
        assert!(offline(
            &Ask::GetDiagramVersion {
                pr: pr.clone(),
                number: 1
            },
            &teller
        ));
        assert!(
            rx.try_iter().next().is_none(),
            "a read is not worth a toast"
        );
        assert!(offline(&Ask::DrawDiagrams(pr.clone()), &teller));
        assert!(offline(&Ask::StopDiagrams(pr.clone()), &teller));
        assert!(offline(
            &Ask::DrawDiagram {
                pr: pr.clone(),
                key: "flow".into()
            },
            &teller
        ));
        assert_eq!(
            rx.try_iter()
                .filter(|t| matches!(t, Tell::Notice { warning: true, .. }))
                .count(),
            3
        );
        assert!(offline(
            &Ask::AddDiagramComment {
                pr: pr.clone(),
                number: 2,
                key: "flow".into(),
                node: "refresh".into(),
                body: "Why?".into(),
                ticket: 4
            },
            &teller
        ));
        assert!(offline(
            &Ask::ReadDiagramCode {
                pr: pr.clone(),
                number: 2,
                key: "flow".into(),
                node: "refresh".into()
            },
            &teller
        ));
        let told: Vec<Tell> = rx.try_iter().collect();
        assert!(
            told.iter()
                .any(|t| matches!(t, Tell::Refused { ticket: 4, .. }))
        );
        assert!(told.iter().any(|t| matches!(
            t,
            Tell::Diagrams(DiagramsTell::Code { code, .. }) if code.unavailable.is_some()
        )));
    }

    #[test]
    fn the_demo_answers_the_keywords_entry() {
        let pr = fixture::demo_pr();
        let tells = demo_tells(vec![Ask::GetDiagrams(pr.clone())]);
        let [
            Tell::Diagrams(DiagramsTell::Loaded {
                versions, latest, ..
            }),
        ] = tells.as_slice()
        else {
            panic!("loaded: {tells:?}");
        };
        let last = latest
            .as_ref()
            .and_then(|v| v.entries.last())
            .expect("an entry");
        assert_eq!(last.plan.key, "keywords");
        assert_eq!(versions[1].entries.len(), 9);
        let tells = demo_tells(vec![Ask::DrawDiagram {
            pr: pr.clone(),
            key: "keywords".into(),
        }]);
        assert!(matches!(
            tells.as_slice(),
            [Tell::Diagrams(DiagramsTell::Entry {
                number: 2,
                key,
                state: EntryState::ReadyMap { .. },
                ..
            })] if key == "keywords"
        ));
    }

    #[test]
    fn the_demo_reads_the_code_of_a_place() {
        use crate::review_state::{Phase, Ready, Tab, TabUi};
        use crate::screens::review::diagrams::{Diagrams, DiagramsModel};
        use clusia_core::keywords::place_node;
        let pr = fixture::demo_pr();
        let versions = fixture::demo_diagrams(1_790_000_000, &pr);
        let latest = versions[1].clone();
        let (view, _) = fixture::demo_review(1_790_000_000);
        let mut model = DiagramsModel::default();
        model.loaded.insert(2, latest);
        let mut diagrams = Diagrams::default();
        diagrams.0.insert(pr.clone(), model);
        let mut tabs = ReviewTabs::default();
        tabs.0.insert(
            pr.clone(),
            Tab {
                phase: Phase::Ready(Box::new(Ready {
                    view,
                    news: Vec::new(),
                    cached_at: None,
                })),
                ui: TabUi::default(),
            },
        );
        let (inbox, outbox) = local_link();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .insert_resource(Asks {
                recorded: vec![Ask::ReadDiagramCode {
                    pr: pr.clone(),
                    number: 2,
                    key: "keywords".into(),
                    node: place_node(0),
                }],
                ..Asks::default()
            })
            .insert_resource(Model::default())
            .insert_resource(Toasts::default())
            .insert_resource(tabs)
            .insert_resource(diagrams)
            .init_resource::<Revalidations>()
            .init_resource::<Stories>()
            .init_resource::<Calls>()
            .init_resource::<TestsModels>()
            .init_resource::<TestMachine>()
            .insert_resource(Clock(Some(1_790_000_000)))
            .insert_resource(PermissionQueue::default())
            .insert_resource(outbox)
            .add_systems(Update, demo_answers);
        app.update();
        let tells: Vec<Tell> = inbox.0.try_iter().collect();
        let [Tell::Diagrams(DiagramsTell::Code { code, node, .. })] = tells.as_slice() else {
            panic!("the code: {tells:?}")
        };
        assert_eq!(node, "p0");
        assert_eq!(code.unavailable, None);
        assert!(!code.lines.is_empty());
        let map = fixture::keywords_map();
        let anchor = map.anchor(0).expect("place 0");
        assert_eq!(code.path, anchor.path);
        assert!(code.lines.iter().any(|l| l.number == anchor.start_line));
    }

    #[test]
    fn the_demo_answers_the_diagram_asks() {
        let pr = fixture::demo_pr();
        let tells = demo_tells(vec![Ask::GetDiagrams(pr.clone())]);
        let [
            Tell::Diagrams(DiagramsTell::Loaded {
                versions,
                latest,
                state,
                ..
            }),
        ] = tells.as_slice()
        else {
            panic!("loaded: {tells:?}");
        };
        assert_eq!(
            versions.iter().map(|v| v.number).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(latest.as_ref().map(|v| v.number), Some(2));
        assert_eq!(*state, DiagramsState::Idle);
        assert!(matches!(
            demo_tells(vec![Ask::GetDiagrams("acme/widgets#9".parse().unwrap())]).as_slice(),
            [Tell::Diagrams(DiagramsTell::Loaded { versions, .. })] if versions.is_empty()
        ));
        assert!(matches!(
            demo_tells(vec![Ask::GetDiagramVersion { pr: pr.clone(), number: 1 }]).as_slice(),
            [Tell::Diagrams(DiagramsTell::Version { version, .. })] if version.number == 1
        ));
        assert!(matches!(
            demo_tells(vec![Ask::DrawDiagrams(pr.clone())]).as_slice(),
            [Tell::Diagrams(DiagramsTell::Loaded { .. })]
        ));
        assert!(matches!(
            demo_tells(vec![Ask::DrawDiagram { pr: pr.clone(), key: "er".into() }]).as_slice(),
            [Tell::Diagrams(DiagramsTell::Entry { number: 2, key, state: EntryState::Ready { .. }, .. })]
                if key == "er"
        ));
        assert!(matches!(
            demo_tells(vec![Ask::StopDiagrams(pr.clone())]).as_slice(),
            [Tell::Diagrams(DiagramsTell::State {
                state: DiagramsState::Idle,
                ..
            })]
        ));
    }

    #[test]
    fn diagram_tells_reach_the_resource_through_pump() {
        let pr = fixture::demo_pr();
        let (inbox, outbox) = local_link();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<ReviewEvent>()
            .add_message::<ShowRequested>()
            .add_message::<TabsRestored>()
            .add_message::<AppExit>()
            .add_message::<GifsArrived>()
            .insert_resource(inbox)
            .init_resource::<Asks>()
            .init_resource::<Model>()
            .init_resource::<Toasts>()
            .init_resource::<ReviewTabs>()
            .init_resource::<MediaCache>()
            .init_resource::<People>()
            .init_resource::<Chats>()
            .init_resource::<PermissionQueue>()
            .init_resource::<Checks>()
            .init_resource::<Diagrams>()
            .init_resource::<Revalidations>()
            .init_resource::<Stories>()
            .init_resource::<Calls>()
            .init_resource::<TestsModels>()
            .init_resource::<TestMachine>()
            .init_resource::<People>()
            .insert_resource(Clock(Some(1_790_000_000)))
            .add_systems(Update, pump);
        let _ = outbox
            .0
            .send(fixture::demo_diagrams_tell(&pr, 1_790_000_000));
        app.update();
        let model = &app.world().resource::<Diagrams>().0[&pr];
        assert!(model.received && model.latest_number() == Some(2));
        let _ = outbox.0.send(Tell::Lost("gone".into()));
        app.update();
        assert!(!app.world().resource::<Diagrams>().0[&pr].asked);
    }

    #[test]
    fn the_demo_answers_the_checks_asks() {
        use clusia_core::checks::CheckKind;
        let pr = fixture::demo_pr();
        let tells = demo_agent_tells(vec![Ask::GetChecks(pr.clone())]);
        let Some(Tell::Checks(ChecksTell::Loaded {
            results,
            states,
            accepted,
            dismissed,
            ..
        })) = tells.first()
        else {
            panic!("loaded: {tells:?}");
        };
        assert_eq!((results.len(), states.len()), (2, 2));
        assert!(accepted.is_empty() && dismissed.is_empty());
        assert!(
            matches!(
                demo_tells(vec![Ask::GetChecks("acme/widgets#9".parse().unwrap())]).as_slice(),
                [Tell::Checks(ChecksTell::Loaded { results, .. })] if results.is_empty()
            ),
            "another review has no checks, and says so instead of loading"
        );
        assert!(
            demo_tells(vec![Ask::GetChecks(pr.clone())]).is_empty(),
            "no open review, nothing to mark as accepted"
        );
        let tells = demo_tells(vec![Ask::RunCheck {
            pr: pr.clone(),
            kind: CheckKind::Audit,
        }]);
        assert!(matches!(
            tells.as_slice(),
            [Tell::Checks(ChecksTell::Done {
                kind: CheckKind::Audit,
                ..
            })]
        ));
        let tells = demo_tells(vec![Ask::StopCheck {
            pr: pr.clone(),
            kind: CheckKind::Security,
        }]);
        assert!(matches!(
            tells.as_slice(),
            [Tell::Checks(ChecksTell::State {
                state: clusia_protocol::CheckState::NotRun,
                ..
            })]
        ));
        let id = demo_finding_ids()[0].clone();
        let tells = demo_tells(vec![Ask::DismissFinding { pr, id: id.clone() }]);
        assert!(matches!(
            tells.as_slice(),
            [Tell::Checks(ChecksTell::Settled { accepted: false, id: got, .. })] if *got == id
        ));
    }

    #[test]
    fn a_demo_accept_that_fails_frees_the_card() {
        let pr = fixture::demo_pr();
        let tells = demo_agent_tells(vec![Ask::AcceptFinding {
            pr: pr.clone(),
            id: "no-such-finding".into(),
            body: None,
        }]);
        assert!(
            tells
                .iter()
                .any(|t| matches!(t, Tell::Notice { warning: true, .. }))
        );
        assert!(tells.iter().any(|t| matches!(
            t,
            Tell::Checks(ChecksTell::Refused { id, .. }) if id == "no-such-finding"
        )));
        let tells = demo_tells(vec![Ask::AcceptFinding {
            pr,
            id: demo_finding_ids()[0].clone(),
            body: None,
        }]);
        assert!(
            tells
                .iter()
                .any(|t| matches!(t, Tell::Checks(ChecksTell::Refused { .. }))),
            "a review that is not open refuses too"
        );
    }

    #[test]
    fn the_demo_accepts_a_finding_into_the_draft_with_its_origin() {
        let pr = fixture::demo_pr();
        let id = demo_finding_ids()[0].clone();
        let tells = demo_agent_tells(vec![Ask::AcceptFinding {
            pr: pr.clone(),
            id: id.clone(),
            body: Some("Please drop the body from the log.".into()),
        }]);
        let Some(Tell::ReviewFile(review)) = tells.first() else {
            panic!("the draft changed: {tells:?}");
        };
        let item = review.draft.items.last().expect("an item");
        assert_eq!(item.origin, Origin::Security);
        assert!(item.accepted);
        assert_eq!(item.body, "Please drop the body from the log.");
        let anchor = item.anchor.as_ref().expect("anchored");
        assert_eq!(
            (anchor.path.as_str(), anchor.line),
            ("src/client/http.rs", 17)
        );
        assert!(matches!(
            tells.last(),
            Some(Tell::Checks(ChecksTell::Settled { accepted: true, .. }))
        ));
        let audit_id = fixture::demo_checks(1_790_000_000).0[1].findings[0]
            .id
            .clone();
        let tells = demo_agent_tells(vec![Ask::AcceptFinding {
            pr,
            id: audit_id,
            body: None,
        }]);
        let Some(Tell::ReviewFile(review)) = tells.first() else {
            panic!("the draft changed");
        };
        assert_eq!(review.draft.items.last().unwrap().origin, Origin::Audit);
    }

    #[test]
    fn offline_story_and_calls_asks_are_answered_not_dropped() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let teller = Teller {
            tx,
            wake: Arc::new(|| {}),
        };
        let pr = fixture::demo_pr();
        for read in [
            Ask::GetStory(pr.clone()),
            Ask::GetStoryVersion {
                pr: pr.clone(),
                number: 1,
            },
            Ask::GetCalls {
                pr: pr.clone(),
                center: None,
                depth: None,
                expand: vec![],
            },
            Ask::FindFunction {
                pr: pr.clone(),
                query: "refresh".into(),
            },
        ] {
            assert!(offline(&read, &teller));
        }
        assert!(
            rx.try_iter().next().is_none(),
            "a read is not worth a toast"
        );

        for run in [
            Ask::WriteStory(pr.clone()),
            Ask::RewriteStory(pr.clone()),
            Ask::StopStory(pr.clone()),
        ] {
            assert!(offline(&run, &teller));
        }
        assert_eq!(
            rx.try_iter()
                .filter(|t| matches!(t, Tell::Notice { warning: true, .. }))
                .count(),
            3
        );

        assert!(offline(
            &Ask::MarkStepRead {
                pr: pr.clone(),
                number: 2,
                step: "safer-saves".into(),
                read: true
            },
            &teller
        ));
        let told: Vec<Tell> = rx.try_iter().collect();
        assert!(
            told.iter()
                .any(|t| matches!(t, Tell::Notice { warning: true, .. }))
        );
        assert!(told.contains(&Tell::Story(StoryTell::ReadRefused {
            pr: pr.clone(),
            step: "safer-saves".into(),
            read: true
        })));

        assert!(offline(
            &Ask::AddStepComment {
                pr: pr.clone(),
                number: 2,
                step: "safer-saves".into(),
                body: "Why?".into(),
                ticket: 5
            },
            &teller
        ));
        assert!(offline(
            &Ask::AddCallsComment {
                pr: pr.clone(),
                function: "f".into(),
                body: "Why?".into(),
                ticket: 6
            },
            &teller
        ));
        assert!(offline(
            &Ask::ReadFunctionCode {
                pr: pr.clone(),
                function: "f".into()
            },
            &teller
        ));
        let told: Vec<Tell> = rx.try_iter().collect();
        assert!(
            told.iter()
                .any(|t| matches!(t, Tell::Refused { ticket: 5, .. }))
        );
        assert!(
            told.iter()
                .any(|t| matches!(t, Tell::Refused { ticket: 6, .. }))
        );
        assert!(told.iter().any(|t| matches!(
            t,
            Tell::Calls(CallsTell::Code { function, code, .. })
                if function == "f" && code.unavailable.is_some()
        )));
    }

    #[test]
    fn the_demo_answers_story_and_calls() {
        let pr = fixture::demo_pr();
        let tells = demo_tells(vec![
            Ask::GetStory(pr.clone()),
            Ask::GetStoryVersion {
                pr: pr.clone(),
                number: 1,
            },
            Ask::GetStoryVersion {
                pr: pr.clone(),
                number: 9,
            },
            Ask::WriteStory(pr.clone()),
            Ask::RewriteStory(pr.clone()),
            Ask::StopStory(pr.clone()),
            Ask::MarkStepRead {
                pr: pr.clone(),
                number: 2,
                step: "safer-saves".into(),
                read: true,
            },
        ]);
        let [
            Tell::Story(StoryTell::Loaded {
                versions, latest, ..
            }),
            Tell::Story(StoryTell::Version { version, .. }),
            Tell::Story(StoryTell::State { state, .. }),
        ] = tells.as_slice()
        else {
            panic!("loaded, version 1, then the stop: {tells:?}");
        };
        assert_eq!(versions.len(), 2);
        assert_eq!(latest.as_ref().map(|v| v.number), Some(2));
        assert_eq!(version.number, 1);
        assert_eq!(*state, StoryState::Idle);

        let other = PrRef::new("acme", "widgets", 7).expect("ref");
        let tells = demo_tells(vec![
            Ask::GetStory(other.clone()),
            Ask::GetCalls {
                pr: other.clone(),
                center: None,
                depth: None,
                expand: vec![],
            },
        ]);
        assert!(matches!(
            &tells[0],
            Tell::Story(StoryTell::Loaded { versions, latest: None, static_steps, .. })
                if versions.is_empty() && static_steps.is_empty()
        ));
        assert!(matches!(
            &tells[1],
            Tell::Calls(CallsTell::Graph {
                graph: None,
                index: CallsIndexState::Idle,
                ..
            })
        ));

        let tells = demo_tells(vec![
            Ask::GetCalls {
                pr: pr.clone(),
                center: None,
                depth: Some(2),
                expand: vec![-1],
            },
            Ask::FindFunction {
                pr: pr.clone(),
                query: "refresh".into(),
            },
            Ask::ReadFunctionCode {
                pr: pr.clone(),
                function: "src/auth/refresh.rs#TokenStore::refresh#38".into(),
            },
        ]);
        let [
            Tell::Calls(CallsTell::Graph { request, graph, .. }),
            Tell::Calls(CallsTell::Found { query, hits, .. }),
            Tell::Calls(CallsTell::Code { function, code, .. }),
        ] = tells.as_slice()
        else {
            panic!("graph, found, code: {tells:?}");
        };
        assert_eq!((request.depth, request.expand.clone()), (2, vec![-1]));
        assert!(
            graph
                .as_ref()
                .is_some_and(|g| g.nodes.iter().all(|n| n.level.abs() <= 2))
        );
        assert_eq!(query, "refresh");
        assert!(!hits.is_empty());
        assert_eq!(function, "src/auth/refresh.rs#TokenStore::refresh#38");
        assert_eq!(code.start_line, 38);
    }

    #[test]
    fn a_step_comment_for_a_review_that_is_not_open_is_refused_in_the_demo() {
        let tells = demo_tells(vec![Ask::AddStepComment {
            pr: fixture::demo_pr(),
            number: 1,
            step: "refresh-early".into(),
            body: "Why?".into(),
            ticket: 3,
        }]);
        assert!(matches!(&tells[..], [Tell::Refused { ticket: 3, .. }]));
    }

    #[test]
    fn story_and_calls_events_become_tells() {
        let pr = fixture::demo_pr();
        assert_eq!(
            StoryTell::from_event(&Event::StoryChanged {
                pr: pr.clone(),
                state: StoryState::Writing
            }),
            Some(StoryTell::State {
                pr: pr.clone(),
                state: StoryState::Writing
            })
        );
        assert_eq!(
            CallsTell::from_event(&Event::CallsIndexChanged {
                pr: pr.clone(),
                state: CallsIndexState::Building
            }),
            Some(CallsTell::Index {
                pr: pr.clone(),
                state: CallsIndexState::Building
            })
        );
        assert_eq!(StoryTell::from_event(&Event::Stopping), None);
        assert_eq!(CallsTell::from_event(&Event::Stopping), None);
        assert_eq!(
            CallsTell::Index {
                pr: pr.clone(),
                state: CallsIndexState::Idle
            }
            .pr(),
            &pr
        );
    }

    #[test]
    fn a_lost_connection_makes_the_story_and_calls_ask_again() {
        use crate::screens::review::calls::{Calls, CallsModel};
        use crate::screens::review::story::model::{Stories, StoryModel};
        let (inbox, outbox) = local_link();
        let pr = fixture::demo_pr();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .add_message::<ReviewEvent>()
            .add_message::<ShowRequested>()
            .add_message::<TabsRestored>()
            .add_message::<AppExit>()
            .add_message::<GifsArrived>()
            .insert_resource(inbox)
            .init_resource::<Asks>()
            .init_resource::<Model>()
            .init_resource::<Toasts>()
            .init_resource::<ReviewTabs>()
            .init_resource::<MediaCache>()
            .init_resource::<Chats>()
            .init_resource::<PermissionQueue>()
            .init_resource::<Checks>()
            .init_resource::<Diagrams>()
            .init_resource::<Revalidations>()
            .init_resource::<Stories>()
            .init_resource::<Calls>()
            .init_resource::<TestsModels>()
            .init_resource::<TestMachine>()
            .init_resource::<People>()
            .insert_resource(Clock(Some(1_790_000_000)))
            .add_systems(Update, pump);
        let _ = outbox.0.send(fixture::demo_story_tell(&pr, 1_790_000_000));
        let _ = outbox.0.send(fixture::demo_calls_graph(None, 3, &[]));
        app.update();
        assert!(app.world().resource::<Stories>().0[&pr].received);
        assert!(app.world().resource::<Calls>().0[&pr].graph.is_some());
        {
            let mut stories = app.world_mut().resource_mut::<Stories>();
            let m: &mut StoryModel = stories.0.get_mut(&pr).unwrap();
            m.write_sent = true;
            m.requested.insert(1);
            m.failed.insert(1);
        }
        {
            let mut calls = app.world_mut().resource_mut::<Calls>();
            let m: &mut CallsModel = calls.0.get_mut(&pr).unwrap();
            m.asking = Some(crate::screens::review::calls::CallsRequest {
                center: None,
                depth: 3,
                expand: vec![],
            });
            m.requested_code
                .insert("src/auth/refresh.rs#TokenStore::refresh#38".into());
        }
        let _ = outbox.0.send(Tell::Lost("gone".into()));
        app.update();
        let story = &app.world().resource::<Stories>().0[&pr];
        assert!(
            !story.asked
                && !story.write_sent
                && story.requested.is_empty()
                && story.failed.is_empty()
        );
        let calls = &app.world().resource::<Calls>().0[&pr];
        assert!(!calls.asked && calls.asking.is_none());
        assert!(
            calls.requested_code.is_empty(),
            "code asked before the loss is asked again"
        );
    }

    /// A one-connection daemon that opens a review but holds back the news until `release`.
    async fn daemon_holding_the_news(
        view: ReviewView,
        news: Vec<NewsItem>,
        release: tokio::sync::oneshot::Receiver<()>,
    ) -> (tempfile::TempDir, PathBuf) {
        use clusia_protocol::{
            ClientMessage, MessageReader, Outcome, PROTOCOL_VERSION, ProtocolError, ServerMessage,
            write_message,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut r = MessageReader::new(r);
            let _hello: ClientMessage = r.next().await.unwrap().unwrap();
            let welcome = ServerMessage::Welcome {
                protocol: PROTOCOL_VERSION,
                daemon: "9.9.9".into(),
            };
            write_message(&mut w, &welcome).await.unwrap();
            let mut release = Some(release);
            while let Ok(Some(ClientMessage::Request { id, cmd })) = r.next::<ClientMessage>().await
            {
                let result = match cmd {
                    Command::OpenReview { .. } => {
                        Outcome::Ok(Reply::Review(Box::new(view.clone())))
                    }
                    Command::GetWhatsNew { .. } => {
                        if let Some(release) = release.take() {
                            let _ = release.await;
                        }
                        Outcome::Ok(Reply::WhatsNew(news.clone()))
                    }
                    _ => Outcome::Err(ProtocolError::new(ErrorCode::NotFound, "none")),
                };
                write_message(&mut w, &ServerMessage::Response { id, result })
                    .await
                    .unwrap();
            }
        });
        (dir, path)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_copy_is_told_before_the_news_is_asked_for_and_the_news_after() {
        let (view, news) = crate::fixture::demo_review(crate::testing::NOW);
        let (release, held) = tokio::sync::oneshot::channel();
        let (_dir, socket) = daemon_holding_the_news(view, news.clone(), held).await;
        let (tx, rx) = crossbeam_channel::unbounded();
        let teller = Teller {
            tx,
            wake: Arc::new(|| {}),
        };
        let pr: PrRef = "rzorzal/clusia#123".parse().unwrap();
        let opening = tokio::spawn({
            let pr = pr.clone();
            async move { open_review(&socket, &teller, pr).await }
        });

        // The daemon answers the news only after this: a bridge that waited for it first
        // would never tell the copy.
        let first = tokio::task::spawn_blocking(move || {
            let first = rx.recv_timeout(Duration::from_secs(10));
            (first, rx)
        })
        .await
        .unwrap();
        assert!(
            matches!(&first.0, Ok(Tell::OpenedAwaitingNews { pr: got, .. }) if *got == pr),
            "{:?}",
            first.0.map(|_| ())
        );
        release.send(()).unwrap();

        assert_eq!(opening.await.unwrap(), Tell::News { pr, news });
        assert!(first.1.try_recv().is_err(), "nothing else was told");
    }

    fn pr7() -> PrRef {
        "rzorzal/clusia#7".parse().unwrap()
    }

    #[test]
    fn the_five_tests_events_become_tells_and_nothing_else_does() {
        use clusia_core::tests::{StepState, TestCaseResult, TestStatus, TestsView};
        let pr = pr7();
        let view = TestsView::new("abc");
        assert_eq!(
            TestsTell::from_event(&Event::TestEnv {
                pr: pr.clone(),
                view: Box::new(view.clone()),
            }),
            Some(TestsTell::Loaded {
                pr: pr.clone(),
                view: Box::new(view),
            })
        );
        assert_eq!(
            TestsTell::from_event(&Event::TestStep {
                pr: pr.clone(),
                step: "image".into(),
                state: StepState::Running,
                tail: Some("Step 2/5".into()),
            }),
            Some(TestsTell::Step {
                pr: pr.clone(),
                step: "image".into(),
                state: StepState::Running,
                tail: Some("Step 2/5".into()),
            })
        );
        assert_eq!(
            TestsTell::from_event(&Event::TestOutput {
                pr: pr.clone(),
                run: "r1".into(),
                tail: "running 3 tests".into(),
            }),
            Some(TestsTell::Output {
                pr: pr.clone(),
                run: "r1".into(),
                tail: "running 3 tests".into(),
            })
        );
        let result = TestCaseResult {
            name: "auth::refresh".into(),
            file: None,
            status: TestStatus::Passed,
            duration_ms: Some(4),
            message: None,
        };
        let told = TestsTell::from_event(&Event::TestResult {
            pr: pr.clone(),
            run: "r1".into(),
            result: result.clone(),
        })
        .expect("a result");
        assert_eq!(told.pr(), &pr);
        assert_eq!(
            told,
            TestsTell::Result {
                pr: pr.clone(),
                run: "r1".into(),
                result,
            }
        );
        assert_eq!(TestsTell::from_event(&Event::Stopping), None);
    }

    #[test]
    fn slow_test_asks_run_on_a_worker_and_stop_does_not() {
        let pr = pr7();
        for ask in [
            Ask::ListTests(pr.clone()),
            Ask::GetTestDisk,
            Ask::CleanUpTests { repo: None },
            Ask::CheckTestEngine { pr: None },
            Ask::Probe,
            Ask::OpenReview(pr.clone()),
        ] {
            assert!(on_worker(&ask), "{ask:?}");
        }
        for ask in [
            Ask::GetTests(pr.clone()),
            Ask::GenerateTests(pr.clone()),
            Ask::PrepareTests {
                pr: pr.clone(),
                recipe_hash: "h".into(),
                approve: clusia_core::tests::Approve::All,
            },
            Ask::RunTests {
                pr: pr.clone(),
                scope: clusia_core::tests::TestScope::All,
            },
            Ask::StopTests(pr.clone()),
            Ask::AcceptRecipe(pr.clone()),
        ] {
            assert!(
                !on_worker(&ask),
                "{ask:?} must never queue behind a slow ask"
            );
        }
    }

    #[test]
    fn demo_mode_frees_every_tests_button_it_does_not_serve() {
        use clusia_core::tests::{Approve, TestScope};
        let pr = fixture::demo_pr();
        let tells = demo_agent_tells(vec![
            Ask::GenerateTests(pr.clone()),
            Ask::PrepareTests {
                pr: pr.clone(),
                recipe_hash: "h".into(),
                approve: Approve::All,
            },
            Ask::RunTests {
                pr: pr.clone(),
                scope: TestScope::All,
            },
            Ask::StopTests(pr.clone()),
            Ask::ListTests(pr.clone()),
            Ask::AcceptRecipe(pr.clone()),
            Ask::CleanUpTests { repo: None },
        ]);
        let kinds: Vec<TestsAskKind> = tells
            .iter()
            .filter_map(|t| match t {
                Tell::Tests(TestsTell::Refused { ask, message, .. }) => {
                    assert_eq!(message, "Demo mode: nothing is sent to the daemon");
                    Some(*ask)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            [
                TestsAskKind::Generate,
                TestsAskKind::Prepare,
                TestsAskKind::Run,
                TestsAskKind::Stop,
                TestsAskKind::List,
                TestsAskKind::Accept,
            ]
        );
        assert!(tells.contains(&Tell::TestDisk(Err(
            "Demo mode: nothing is sent to the daemon".into()
        ))));
        // The model a click left pending is free again once the tell is applied.
        let mut model = crate::screens::review::tests_tab::TestsModel::default();
        model.sent(TestsAskKind::Generate);
        for tell in &tells {
            if let Tell::Tests(tell) = tell {
                model.apply(tell);
                break;
            }
        }
        assert_eq!(
            model.pending, None,
            "Generate tests comes back enabled after the click"
        );
    }

    #[test]
    fn demo_mode_answers_the_machine_asks_and_an_empty_view_for_the_tests_of_the_demo_review() {
        let tells = demo_agent_tells(vec![
            Ask::GetTestDisk,
            Ask::CheckTestEngine { pr: None },
            Ask::GetTests(fixture::demo_pr()),
        ]);
        assert_eq!(tells.len(), 3);
        assert_eq!(tells[0], Tell::TestDisk(Ok(fixture::demo_test_disk())));
        assert_eq!(tells[1], Tell::TestEngine(Ok(fixture::demo_test_engine())));
        let Tell::Tests(TestsTell::Loaded { pr, view }) = &tells[2] else {
            panic!("a view: {:?}", tells[2]);
        };
        assert_eq!(pr, &fixture::demo_pr());
        assert_eq!(view.phase, clusia_core::tests::TestsPhase::NotGenerated);
        assert_eq!(view.listed, None);
    }
}
