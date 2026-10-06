//! What GitHub knows about a pull request beyond its summary: files, conversation, commits, checks.

use serde::{Deserialize, Serialize};

use crate::draft::Side;
use crate::pr::PrDetail;
use crate::review::Role;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub previous_path: Option<String>,
    /// `added`, `removed`, `modified`, `renamed`, `copied`, `changed`, `unchanged`.
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    /// Unified diff hunks; absent for binary or very large files.
    pub patch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadComment {
    pub id: u64,
    pub in_reply_to: Option<u64>,
    pub path: String,
    /// `None` when the comment is outdated (its line no longer exists in the diff).
    pub line: Option<u32>,
    pub side: Option<Side>,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueComment {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewInfo {
    pub id: u64,
    pub author: String,
    /// `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED`.
    pub state: String,
    pub body: String,
    pub submitted_at: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub author: String,
    /// First line of the commit message.
    pub message: String,
    pub date: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChecksSummary {
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub pending: u32,
}

impl ChecksSummary {
    pub fn label(&self) -> &'static str {
        if self.total == 0 {
            "none"
        } else if self.pending > 0 {
            "pending"
        } else if self.failed > 0 {
            "failed"
        } else {
            "passed"
        }
    }
}

/// One comment of a review thread (GraphQL).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadPost {
    pub database_id: Option<u64>,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub url: String,
}

/// A review thread as GitHub's GraphQL API reports it: what replies and resolves point at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewThread {
    /// GraphQL node id (`PRRT_…`).
    pub id: String,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub path: String,
    /// `None` when the thread is outdated.
    pub line: Option<u32>,
    /// First line of a multi-line thread; `None` for single-line threads.
    pub start_line: Option<u32>,
    pub side: Side,
    pub viewer_can_reply: bool,
    pub viewer_can_resolve: bool,
    pub comments: Vec<ThreadPost>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PrConversation {
    /// Review comments as REST lists them (kept for the CLI).
    pub threads: Vec<ThreadComment>,
    pub comments: Vec<IssueComment>,
    pub reviews: Vec<ReviewInfo>,
    /// The same review comments grouped in threads, with node ids and resolved state.
    #[serde(default)]
    pub review_threads: Vec<ReviewThread>,
}

/// What a successful open fetched, kept on disk for "Open from cache".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCache {
    pub pr: PrDetail,
    pub files: Vec<FileDiff>,
    pub conversation: PrConversation,
    pub checks: Option<ChecksSummary>,
    pub role: Role,
    pub viewer: Option<String>,
    pub worktree: Option<String>,
    pub fetched_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_label() {
        let c = |total, passed, failed, pending| ChecksSummary {
            total,
            passed,
            failed,
            pending,
        };
        assert_eq!(c(0, 0, 0, 0).label(), "none");
        assert_eq!(c(3, 2, 0, 1).label(), "pending");
        assert_eq!(c(3, 2, 1, 0).label(), "failed");
        assert_eq!(c(2, 2, 0, 0).label(), "passed");
    }

    #[test]
    fn conversations_without_review_threads_still_load() {
        let json = r#"{"threads":[],"comments":[],"reviews":[]}"#;
        let c: PrConversation = serde_json::from_str(json).unwrap();
        assert!(c.review_threads.is_empty());
    }

    #[test]
    fn review_thread_wire_shape() {
        let t = ReviewThread {
            id: "PRRT_1".into(),
            is_resolved: false,
            is_outdated: false,
            path: "src/auth/refresh.rs".into(),
            line: Some(41),
            start_line: None,
            side: Side::Right,
            viewer_can_reply: true,
            viewer_can_resolve: true,
            comments: vec![ThreadPost {
                database_id: Some(7),
                author: "mona".into(),
                body: "Why one minute?".into(),
                created_at: "2026-10-01T10:00:00Z".into(),
                url: "https://github.com/rzorzal/clusia/pull/123#discussion_r7".into(),
            }],
        };
        assert_eq!(
            serde_json::to_string(&t).unwrap(),
            r#"{"id":"PRRT_1","is_resolved":false,"is_outdated":false,"path":"src/auth/refresh.rs","line":41,"start_line":null,"side":"right","viewer_can_reply":true,"viewer_can_resolve":true,"comments":[{"database_id":7,"author":"mona","body":"Why one minute?","created_at":"2026-10-01T10:00:00Z","url":"https://github.com/rzorzal/clusia/pull/123#discussion_r7"}]}"#
        );
    }

    #[test]
    fn review_cache_round_trips() {
        let pr: crate::PrRef = "acme/widgets#7".parse().unwrap();
        let cache = ReviewCache {
            pr: PrDetail {
                summary: crate::PrSummary {
                    pr: pr.clone(),
                    title: "Fix cache".into(),
                    author: "maria".into(),
                    url: "https://github.com/acme/widgets/pull/7".into(),
                    draft: false,
                    updated_at: "2026-10-01T12:00:00Z".into(),
                    comments: 1,
                },
                base_ref: "main".into(),
                head_ref: "fix".into(),
                base_sha: "b".repeat(40),
                head_sha: "h".repeat(40),
                additions: 1,
                deletions: 0,
                changed_files: 1,
                clone_url: "https://github.com/acme/widgets.git".into(),
                closed: true,
                merged: true,
            },
            files: vec![FileDiff {
                path: "src/a.rs".into(),
                previous_path: Some("src/old.rs".into()),
                status: "renamed".into(),
                additions: 1,
                deletions: 0,
                patch: None,
            }],
            conversation: PrConversation {
                threads: vec![],
                comments: vec![IssueComment {
                    id: 3,
                    author: "mona".into(),
                    body: "Looks good".into(),
                    created_at: "2026-10-01T10:00:00Z".into(),
                    url: "https://github.com/acme/widgets/pull/7#issuecomment-3".into(),
                }],
                reviews: vec![],
                review_threads: vec![ReviewThread {
                    id: "PRRT_1".into(),
                    is_resolved: true,
                    is_outdated: false,
                    path: "src/a.rs".into(),
                    line: Some(1),
                    start_line: None,
                    side: Side::Right,
                    viewer_can_reply: true,
                    viewer_can_resolve: false,
                    comments: vec![],
                }],
            },
            checks: Some(ChecksSummary {
                total: 2,
                passed: 1,
                failed: 1,
                pending: 0,
            }),
            role: Role::Author,
            viewer: Some("maria".into()),
            worktree: None,
            fetched_at: 1_700_000_000,
        };
        let json = serde_json::to_string(&cache).unwrap();
        assert_eq!(serde_json::from_str::<ReviewCache>(&json).unwrap(), cache);
    }
}
