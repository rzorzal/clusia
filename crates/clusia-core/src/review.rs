//! A review in progress and its lifecycle (spec §6).

use serde::{Deserialize, Serialize};

use crate::{Draft, PrRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Active,
    Saved,
    Outdated,
    Revalidated,
    Publishing,
    Published,
    Discarded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewEvent {
    Leave,
    Reopen,
    NewHead,
    Revalidated,
    Finalize,
    PublishOk,
    PublishFailed,
    Discard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cannot apply {event:?} to a review that is {from:?}")]
pub struct InvalidTransition {
    pub from: ReviewState,
    pub event: ReviewEvent,
}

impl ReviewState {
    pub fn apply(self, event: ReviewEvent) -> Result<ReviewState, InvalidTransition> {
        use ReviewEvent as E;
        use ReviewState as S;
        let next = match (self, event) {
            (S::Active, E::Leave) => S::Saved,
            (S::Active | S::Saved | S::Outdated | S::Revalidated, E::Reopen) => S::Active,
            (S::Active, E::NewHead) => S::Active,
            (S::Saved | S::Outdated | S::Revalidated, E::NewHead) => S::Outdated,
            (S::Outdated, E::Revalidated) => S::Revalidated,
            (S::Active | S::Saved | S::Revalidated, E::Finalize) => S::Publishing,
            (S::Publishing, E::PublishOk) => S::Published,
            (S::Publishing, E::PublishFailed) => S::Saved,
            (S::Active | S::Saved | S::Outdated | S::Revalidated, E::Discard) => S::Discarded,
            (from, event) => return Err(InvalidTransition { from, event }),
        };
        Ok(next)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, ReviewState::Published | ReviewState::Discarded)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub pr: PrRef,
    pub title: String,
    pub state: ReviewState,
    pub base_sha: String,
    pub head_sha: String,
    pub draft: Draft,
    pub created_at: i64,
    pub updated_at: i64,
    /// When the user last saw this review's "what's new"; `None` until the first `MarkSeen`.
    #[serde(default)]
    pub last_seen_at: Option<i64>,
    /// CI label (`passed`, `failed`, `pending`, `none`) at the last `MarkSeen`.
    #[serde(default)]
    pub last_checks: Option<String>,
    /// Harness session for running agents in a step (when available).
    #[serde(default)]
    pub harness_session: Option<String>,
}

impl Review {
    pub fn new(pr: PrRef, title: String, base_sha: String, head_sha: String, now: i64) -> Self {
        Self {
            pr,
            title,
            state: ReviewState::Active,
            base_sha,
            head_sha,
            draft: Draft::default(),
            created_at: now,
            updated_at: now,
            last_seen_at: None,
            last_checks: None,
            harness_session: None,
        }
    }

    /// Applies a lifecycle event. On error nothing changes.
    pub fn apply(&mut self, event: ReviewEvent, now: i64) -> Result<(), InvalidTransition> {
        self.state = self.state.apply(event)?;
        self.updated_at = now;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    RequestChanges,
    Comment,
    /// Owner only: a comment review (if there is content), then close the PR.
    ClosePr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Author,
    Reviewer,
}

impl Verdict {
    pub fn allowed_for(role: Role) -> &'static [Verdict] {
        match role {
            Role::Author => &[Verdict::Comment, Verdict::ClosePr],
            Role::Reviewer => &[Verdict::Approve, Verdict::RequestChanges, Verdict::Comment],
        }
    }

    pub fn is_allowed_for(self, role: Role) -> bool {
        Verdict::allowed_for(role).contains(&self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ReviewEvent as E;
    use ReviewState as S;

    #[test]
    fn transition_table() {
        let ok = [
            (S::Active, E::Leave, S::Saved),
            (S::Saved, E::Reopen, S::Active),
            (S::Outdated, E::Reopen, S::Active),
            (S::Revalidated, E::Reopen, S::Active),
            (S::Active, E::Reopen, S::Active),
            (S::Active, E::NewHead, S::Active),
            (S::Saved, E::NewHead, S::Outdated),
            (S::Outdated, E::NewHead, S::Outdated),
            (S::Revalidated, E::NewHead, S::Outdated),
            (S::Outdated, E::Revalidated, S::Revalidated),
            (S::Active, E::Finalize, S::Publishing),
            (S::Saved, E::Finalize, S::Publishing),
            (S::Revalidated, E::Finalize, S::Publishing),
            (S::Publishing, E::PublishOk, S::Published),
            (S::Publishing, E::PublishFailed, S::Saved),
            (S::Outdated, E::Discard, S::Discarded),
        ];
        for (from, event, to) in ok {
            assert_eq!(from.apply(event), Ok(to), "{from:?} + {event:?}");
        }
        for (from, event) in [
            (S::Outdated, E::Finalize),
            (S::Published, E::Reopen),
            (S::Discarded, E::Leave),
            (S::Publishing, E::Discard),
            (S::Saved, E::Leave),
        ] {
            assert_eq!(from.apply(event), Err(InvalidTransition { from, event }));
        }
        assert!(
            S::Published.is_terminal() && S::Discarded.is_terminal() && !S::Saved.is_terminal()
        );
    }

    #[test]
    fn review_new_and_apply() {
        let pr: PrRef = "acme/widgets#7".parse().unwrap();
        let mut r = Review::new(pr, "Fix".into(), "b1".into(), "h1".into(), 100);
        assert_eq!((r.state, r.created_at, r.updated_at), (S::Active, 100, 100));
        r.apply(E::Leave, 150).unwrap();
        assert_eq!((r.state, r.updated_at), (S::Saved, 150));
        assert!(r.apply(E::Leave, 160).is_err());
        assert_eq!(r.updated_at, 150, "a rejected transition changes nothing");
    }

    #[test]
    fn review_file_tolerates_missing_optional_fields() {
        let json = r#"{"pr":"acme/widgets#7","title":"t","state":"saved","base_sha":"b","head_sha":"h",
            "draft":{"items":[]},"created_at":1,"updated_at":2}"#;
        let r: Review = serde_json::from_str(json).unwrap();
        assert_eq!((r.last_seen_at, r.draft.next_id), (None, 0));
    }

    #[test]
    fn verdicts_by_role() {
        assert!(!Verdict::Approve.is_allowed_for(Role::Author));
        assert!(!Verdict::RequestChanges.is_allowed_for(Role::Author));
        assert!(Verdict::ClosePr.is_allowed_for(Role::Author));
        assert!(Verdict::Approve.is_allowed_for(Role::Reviewer));
        assert!(!Verdict::ClosePr.is_allowed_for(Role::Reviewer));
        assert_eq!(
            serde_json::to_string(&Verdict::RequestChanges).unwrap(),
            r#""request_changes""#
        );
    }
}
