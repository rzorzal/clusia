//! The agent's display name on the pages: the configured one, or, for an open review, the one
//! that review's session started with.

use clusia_core::Review;
use clusia_core::config::HarnessKind;

use crate::review_state::Tab;
use crate::snapshot::Snapshot;

/// The agent the settings name now.
pub fn active(snap: &Snapshot) -> &'static str {
    snap.config.harness.kind.name()
}

/// The kind `review` is pinned to: its session's kind when it has one, Claude Code for a
/// session saved before kinds were stored, and `fallback` while there is no session.
fn kind_of(review: &Review, fallback: HarnessKind) -> HarnessKind {
    match (
        review.harness_session_kind,
        review.harness_session.is_some(),
    ) {
        (Some(kind), _) => kind,
        (None, true) => HarnessKind::ClaudeCode,
        (None, false) => fallback,
    }
}

/// The agent of `review`, falling back to the active one while it has no session.
pub fn of_review(snap: &Snapshot, review: &Review) -> &'static str {
    kind_of(review, snap.config.harness.kind).name()
}

/// The agent of the review in `tab`: the pinned kind (the session's, else the config kind the
/// tab was opened with), so a later switch in Config never renames an open review. A tab with no
/// review yet shows the kind it was opened with, else the active one.
pub fn of_tab(snap: &Snapshot, tab: &Tab) -> &'static str {
    kind_of_tab(snap, tab).name()
}

/// The kind `of_tab` names.
pub fn kind_of_tab(snap: &Snapshot, tab: &Tab) -> HarnessKind {
    let opened = tab.ui.opened_with.unwrap_or(snap.config.harness.kind);
    match tab.ready() {
        Some(ready) => kind_of(&ready.view.review, opened),
        None => opened,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::review_state::Tab;
    use crate::testing::NOW;
    use clusia_core::config::HarnessKind;

    fn review() -> Review {
        fixture::demo_review(NOW).0.review
    }

    #[test]
    fn the_active_name_follows_the_configured_kind() {
        let mut snap = Snapshot::default();
        assert_eq!(active(&snap), "Claude Code");
        snap.config.harness.kind = HarnessKind::Codex;
        assert_eq!(active(&snap), "Codex");
    }

    #[test]
    fn an_open_review_keeps_the_agent_it_started_with() {
        let mut snap = Snapshot::default();
        snap.config.harness.kind = HarnessKind::Codex;
        let mut r = review();
        assert_eq!(
            of_review(&snap, &r),
            "Codex",
            "no session yet: the active agent"
        );

        r.harness_session = Some("s-1".into());
        assert_eq!(
            of_review(&snap, &r),
            "Claude Code",
            "a session saved before kinds were stored is Claude Code's"
        );

        r.harness_session_kind = Some(HarnessKind::Codex);
        snap.config.harness.kind = HarnessKind::ClaudeCode;
        assert_eq!(
            of_review(&snap, &r),
            "Codex",
            "the config changed after the review began"
        );
    }

    fn ready_tab(opened_with: Option<HarnessKind>, session: Option<HarnessKind>) -> Tab {
        let (mut view, news) = fixture::demo_review(NOW);
        view.review.harness_session_kind = session;
        view.review.harness_session = session.map(|_| "s-1".to_string());
        let ui = crate::review_state::TabUi {
            opened_with,
            ..Default::default()
        };
        Tab {
            phase: crate::review_state::Phase::Ready(Box::new(crate::review_state::Ready {
                view,
                news,
                cached_at: None,
            })),
            ui,
        }
    }

    #[test]
    fn a_tab_keeps_the_config_kind_it_was_opened_with() {
        let mut snap = Snapshot::default();
        snap.config.harness.kind = HarnessKind::Codex;
        let tab = ready_tab(Some(HarnessKind::ClaudeCode), None);
        assert_eq!(
            of_tab(&snap, &tab),
            "Claude Code",
            "opened on Claude Code, then the user switched to Codex"
        );
        let tab = ready_tab(None, None);
        assert_eq!(
            of_tab(&snap, &tab),
            "Codex",
            "no pin recorded: the active one"
        );
        let tab = ready_tab(Some(HarnessKind::ClaudeCode), Some(HarnessKind::Codex));
        assert_eq!(
            of_tab(&snap, &tab),
            "Codex",
            "the session's own kind wins over the config kind at open"
        );
    }

    #[test]
    fn a_tab_that_is_not_ready_uses_the_active_agent() {
        let mut snap = Snapshot::default();
        snap.config.harness.kind = HarnessKind::Codex;
        let tab = crate::review_state::Tab {
            phase: crate::review_state::Phase::Loading { steps: Vec::new() },
            ui: Default::default(),
        };
        assert_eq!(of_tab(&snap, &tab), "Codex");
    }
}
