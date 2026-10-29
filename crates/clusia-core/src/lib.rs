//! Clúsia domain types and pure logic. Besides reading environment variables, the only I/O is
//! the log files of `logging`.

pub mod activity;
pub mod agent;
pub mod checks;
pub mod config;
pub mod diagram;
pub mod diffmap;
pub mod draft;
pub mod editor;
pub mod hidden;
pub mod hunks;
pub mod keywords;
pub mod launch_agent;
pub mod logging;
pub mod media;
pub mod notify;
pub mod paths;
pub mod people;
pub mod permissions;
pub mod pr;
pub mod prdata;
pub mod printable;
pub mod publish;
pub mod revalidation;
pub mod review;
pub mod story;
pub mod tabs;
#[cfg(any(test, feature = "test-kit"))]
pub mod testkit;
pub mod tests;
pub mod time;

pub use activity::{Activity, ActivityKind, ActivitySummary, DayCount};
pub use agent::{AgentState, review_md};
pub use checks::{
    AreaStatus, AuditArea, CheckKind, CheckResult, Finding, Pass, PromptContext, Severity,
    area_status, default_areas,
};
pub use config::{
    CODE_SIZES, Config, Density, Diff, DiffView, Dnd, EventKind, General, HourMinute, ListSort,
    Lists, Media, Notifications, Route, SoundId, TextSize, Weekday,
};
pub use diffmap::{
    DiffMap, FileChange, Hunk, LineMap, Relocation, added_lines, can_comment, commentable_lines,
    fits_diff, relocate,
};
pub use draft::{
    Anchor, Draft, DraftError, DraftItem, DraftKind, ItemStatus, Origin, Side, ThreadRef,
};
pub use editor::{EditorError, editor_argv};
pub use hidden::{HiddenComments, HiddenState};
pub use media::{MAX_MEDIA_BYTES, MediaKind};
pub use notify::OpenTarget;
pub use paths::{LAUNCH_AGENT_FILE, Paths, PathsError};
pub use people::Profile;
pub use pr::{PrDetail, PrFilter, PrRef, PrRefError, PrSummary};
pub use prdata::{
    CheckRun, ChecksSummary, CommitInfo, FileDiff, IssueComment, PrConversation, ReviewCache,
    ReviewInfo, ReviewThread, ThreadComment, ThreadPost,
};
pub use publish::{
    DEFAULT_BODY, PublishError, PublishPlan, ReplyPayload, ReviewComment, ReviewPayload,
    plan_publish,
};
pub use revalidation::{
    AskedItem, CommitLine, DecideError, Decision, DiagramDigest, DiagramsStale, MAX_BLOCKS,
    Proposal, QUIET_DEFAULT_SECS, QUIET_RANGE, Revalidation, RevalidationContext, RoundState,
    RowKind, accept_all, decide, diagrams_stale_section, fingerprint, open_round,
    revalidation_prompt, row_kind,
};
pub use review::{InvalidTransition, PrState, Review, ReviewEvent, ReviewState, Role, Verdict};
pub use tabs::{OpenTab, OpenTabs, TAB_IDLE_DAYS};
