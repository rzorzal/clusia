//! Clúsia domain types and pure logic. No I/O beyond reading environment variables.

pub mod activity;
pub mod config;
pub mod diffmap;
pub mod draft;
pub mod paths;
pub mod pr;
pub mod publish;
pub mod review;
pub mod time;

pub use activity::{Activity, ActivityKind, ActivitySummary, DayCount};
pub use config::Config;
pub use diffmap::{
    DiffMap, FileChange, Hunk, LineMap, Relocation, can_comment, commentable_lines, relocate,
};
pub use draft::{Anchor, Draft, DraftError, DraftItem, DraftKind, ItemStatus, Origin, Side};
pub use paths::{Paths, PathsError};
pub use pr::{PrDetail, PrFilter, PrRef, PrRefError, PrSummary};
pub use publish::{
    DEFAULT_BODY, PublishError, PublishPlan, ReviewComment, ReviewPayload, plan_publish,
};
pub use review::{InvalidTransition, Review, ReviewEvent, ReviewState, Role, Verdict};
