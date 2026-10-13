//! A review in progress and its lifecycle (spec §6).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::config::HarnessKind;
use crate::revalidation::Revalidation;
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

/// Where the pull request stands on GitHub, as the daemon last saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    #[default]
    Open,
    Closed,
    Merged,
}

impl PrState {
    pub fn of(closed: bool, merged: bool) -> Self {
        if merged {
            PrState::Merged
        } else if closed {
            PrState::Closed
        } else {
            PrState::Open
        }
    }

    pub fn is_open(&self) -> bool {
        *self == PrState::Open
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
    /// The harness that made `harness_session`; a session without one is Claude Code's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session_kind: Option<HarnessKind>,
    /// The program that made `harness_session` when it is a custom command, as configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session_program: Option<String>,
    /// What the pages called the harness that made `harness_session`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session_name: Option<String>,
    /// Whether the pull request is still open; a closed or merged one is not checked again
    /// in the background.
    #[serde(default, skip_serializing_if = "PrState::is_open")]
    pub pr_state: PrState,
    /// What the agent proposed for the draft after the pull request got new commits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revalidation: Option<Revalidation>,
    /// The draft items the reviewer removed through a revalidation. They stay in the draft as
    /// obsolete and do not stop a publish; re-anchoring or deleting an item takes it off.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub removed: BTreeSet<String>,
}

impl Review {
    /// The stored session when `kind` made it; a session of another harness is not `kind`'s to
    /// resume. A custom command's session also needs the same `program`.
    pub fn session_of(&self, kind: HarnessKind, program: Option<&str>) -> Option<&str> {
        let made_by = self.harness_session_kind.unwrap_or(HarnessKind::ClaudeCode);
        let same_program =
            kind != HarnessKind::Custom || self.harness_session_program.as_deref() == program;
        self.harness_session
            .as_deref()
            .filter(|_| made_by == kind && same_program)
    }

    /// Stores `id` as the chat's session, made by `kind`. The program is kept only for a custom
    /// command.
    pub fn remember_session(
        &mut self,
        id: &str,
        kind: HarnessKind,
        program: Option<&str>,
        name: &str,
    ) {
        self.harness_session = Some(id.to_string());
        self.harness_session_kind = Some(kind);
        self.harness_session_program = program
            .filter(|_| kind == HarnessKind::Custom)
            .map(str::to_owned);
        self.harness_session_name = Some(name.to_owned());
    }

    /// Drops the chat's session, so the next turn starts a new one.
    pub fn forget_session(&mut self) {
        self.harness_session = None;
        self.harness_session_kind = None;
        self.harness_session_program = None;
        self.harness_session_name = None;
    }

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
            harness_session_kind: None,
            harness_session_program: None,
            harness_session_name: None,
            pr_state: PrState::Open,
            revalidation: None,
            removed: BTreeSet::new(),
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
    fn pr_state_follows_closed_and_merged() {
        assert_eq!(PrState::of(false, false), PrState::Open);
        assert_eq!(PrState::of(true, false), PrState::Closed);
        assert_eq!(PrState::of(true, true), PrState::Merged);
        assert!(PrState::Open.is_open() && !PrState::Closed.is_open());
    }

    #[test]
    fn a_stored_review_without_pr_state_is_open_and_open_is_not_written() {
        let review = Review::new(
            "acme/widgets#7".parse().unwrap(),
            "t".into(),
            "b".into(),
            "h".into(),
            1,
        );
        let json = serde_json::to_value(&review).unwrap();
        assert!(json.get("pr_state").is_none());
        let back: Review = serde_json::from_value(json).unwrap();
        assert_eq!(back.pr_state, PrState::Open);
        let mut ended = review;
        ended.pr_state = PrState::Merged;
        let json = serde_json::to_value(&ended).unwrap();
        assert_eq!(json["pr_state"], "merged");
    }

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

    #[test]
    fn a_session_belongs_to_the_harness_that_made_it() {
        let pr: PrRef = "acme/widgets#7".parse().unwrap();
        let mut review = Review::new(pr, "t".into(), "b".into(), "h".into(), 1);
        assert_eq!(review.session_of(HarnessKind::ClaudeCode, None), None);
        review.harness_session = Some("uuid".into());
        assert_eq!(
            review.session_of(HarnessKind::ClaudeCode, None),
            Some("uuid"),
            "no kind recorded: Claude Code's"
        );
        assert_eq!(review.session_of(HarnessKind::Codex, None), None);
        review.harness_session_kind = Some(HarnessKind::Codex);
        assert_eq!(review.session_of(HarnessKind::Codex, None), Some("uuid"));
        assert_eq!(review.session_of(HarnessKind::ClaudeCode, None), None);
    }

    #[test]
    fn a_custom_session_belongs_to_its_program() {
        let pr: PrRef = "acme/widgets#7".parse().unwrap();
        let mut review = Review::new(pr, "t".into(), "b".into(), "h".into(), 1);
        review.remember_session("abc", HarnessKind::Custom, Some("/bin/echo-a"), "Echo");
        assert_eq!(
            review.session_of(HarnessKind::Custom, Some("/bin/echo-a")),
            Some("abc")
        );
        assert_eq!(
            review.session_of(HarnessKind::Custom, Some("/bin/echo-b")),
            None
        );
        assert_eq!(review.session_of(HarnessKind::Custom, None), None);
        assert_eq!(review.harness_session_name.as_deref(), Some("Echo"));
        review.remember_session("thr", HarnessKind::Codex, Some("/bin/ignored"), "Codex");
        assert_eq!(review.harness_session_program, None);
        assert_eq!(
            review.session_of(HarnessKind::Codex, Some("/x")),
            Some("thr")
        );
    }

    #[test]
    fn forgetting_the_session_clears_every_field() {
        let pr: PrRef = "acme/widgets#7".parse().unwrap();
        let mut review = Review::new(pr, "t".into(), "b".into(), "h".into(), 1);
        review.remember_session("abc", HarnessKind::Custom, Some("/bin/echo-a"), "Echo");
        review.forget_session();
        assert_eq!(review.harness_session, None);
        assert_eq!(review.harness_session_kind, None);
        assert_eq!(review.harness_session_program, None);
        assert_eq!(review.harness_session_name, None);
        let text = serde_json::to_string(&review).unwrap();
        assert!(!text.contains("harness_session_program"), "{text}");
        assert!(!text.contains("harness_session_name"), "{text}");
    }

    #[test]
    fn the_session_kind_is_stored_only_when_there_is_one() {
        let pr: PrRef = "acme/widgets#7".parse().unwrap();
        let mut review = Review::new(pr, "t".into(), "b".into(), "h".into(), 1);
        let plain = serde_json::to_string(&review).unwrap();
        assert!(!plain.contains("harness_session_kind"), "{plain}");
        let old: Review = serde_json::from_str(&plain).unwrap();
        assert_eq!(old.harness_session_kind, None);
        review.harness_session = Some("thr-1".into());
        review.harness_session_kind = Some(HarnessKind::Codex);
        let text = serde_json::to_string(&review).unwrap();
        assert!(text.contains(r#""harness_session_kind":"codex""#), "{text}");
        let back: Review = serde_json::from_str(&text).unwrap();
        assert_eq!(back, review);
    }
}
