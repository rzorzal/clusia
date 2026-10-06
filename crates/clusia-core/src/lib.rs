//! Clúsia domain types and pure logic. No I/O beyond reading environment variables.

pub mod activity;
pub mod config;
pub mod diffmap;
pub mod draft;
pub mod editor;
pub mod paths;
pub mod pr;
pub mod prdata;
pub mod publish;
pub mod review;
pub mod time;

pub use activity::{Activity, ActivityKind, ActivitySummary, DayCount};
pub use config::{CODE_SIZES, Config, Density, DiffView, ListSort, Lists};
pub use diffmap::{
    DiffMap, FileChange, Hunk, LineMap, Relocation, can_comment, commentable_lines, relocate,
};
pub use draft::{
    Anchor, Draft, DraftError, DraftItem, DraftKind, ItemStatus, Origin, Side, ThreadRef,
};
pub use editor::{EditorError, editor_argv};
pub use paths::{Paths, PathsError};
pub use pr::{PrDetail, PrFilter, PrRef, PrRefError, PrSummary};
pub use prdata::{
    ChecksSummary, CommitInfo, FileDiff, IssueComment, PrConversation, ReviewCache, ReviewInfo,
    ReviewThread, ThreadComment, ThreadPost,
};
pub use publish::{
    DEFAULT_BODY, PublishError, PublishPlan, ReplyPayload, ReviewComment, ReviewPayload,
    plan_publish,
};
pub use review::{InvalidTransition, Review, ReviewEvent, ReviewState, Role, Verdict};
