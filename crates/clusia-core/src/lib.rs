//! Clúsia domain types and pure logic. Besides reading environment variables, the only I/O is
//! the log files of `logging`.

pub mod activity;
pub mod agent;
pub mod config;
pub mod diffmap;
pub mod draft;
pub mod editor;
pub mod launch_agent;
pub mod logging;
pub mod media;
pub mod notify;
pub mod paths;
pub mod permissions;
pub mod pr;
pub mod prdata;
pub mod publish;
pub mod review;
pub mod time;

pub use activity::{Activity, ActivityKind, ActivitySummary, DayCount};
pub use agent::{AgentState, review_md};
pub use config::{
    CODE_SIZES, Config, Density, DiffView, Dnd, EventKind, General, HourMinute, ListSort, Lists,
    Media, Notifications, Route, SoundId, Weekday,
};
pub use diffmap::{
    DiffMap, FileChange, Hunk, LineMap, Relocation, can_comment, commentable_lines, relocate,
};
pub use draft::{
    Anchor, Draft, DraftError, DraftItem, DraftKind, ItemStatus, Origin, Side, ThreadRef,
};
pub use editor::{EditorError, editor_argv};
pub use media::{MAX_MEDIA_BYTES, MediaKind};
pub use notify::OpenTarget;
pub use paths::{LAUNCH_AGENT_FILE, Paths, PathsError};
pub use pr::{PrDetail, PrFilter, PrRef, PrRefError, PrSummary};
pub use prdata::{
    ChecksSummary, CommitInfo, FileDiff, IssueComment, PrConversation, ReviewCache, ReviewInfo,
    ReviewThread, ThreadComment, ThreadPost,
};
pub use publish::{
    DEFAULT_BODY, PublishError, PublishPlan, ReplyPayload, ReviewComment, ReviewPayload,
    plan_publish,
};
pub use review::{InvalidTransition, PrState, Review, ReviewEvent, ReviewState, Role, Verdict};
