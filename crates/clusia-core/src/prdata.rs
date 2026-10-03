//! What GitHub knows about a pull request beyond its summary: files, conversation, commits, checks.

use serde::{Deserialize, Serialize};

use crate::draft::Side;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PrConversation {
    pub threads: Vec<ThreadComment>,
    pub comments: Vec<IssueComment>,
    pub reviews: Vec<ReviewInfo>,
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
}
