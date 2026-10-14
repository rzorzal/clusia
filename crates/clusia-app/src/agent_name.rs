//! The agent's display name on the pages: the configured one, or, for an open review, the one
//! that review's session started with.

use clusia_core::Review;
use clusia_core::config::HarnessKind;

use crate::review_state::Tab;
use crate::snapshot::Snapshot;

/// The agent the settings name now.
pub fn active(snap: &Snapshot) -> String {
    snap.config.harness.display_name()
}

/// The `(kind, name)` the settings name now.
fn configured(snap: &Snapshot) -> (HarnessKind, String) {
    (snap.config.harness.kind, snap.config.harness.display_name())
}

/// The agent `review` is pinned to: its session's kind and name when it has them, Claude Code
/// for a session saved before kinds were stored, and `fallback` while there is no session. A
/// session saved with a kind but no name takes the kind's name. A custom command's session that
/// belongs to another program than the pinned one (`pinned_program`) does not name the review:
/// the pinned harness does.
fn pinned(
    review: &Review,
    fallback: (HarnessKind, String),
    pinned_program: Option<&str>,
) -> (HarnessKind, String) {
    match (
        review.harness_session_kind,
        review.harness_session.is_some(),
    ) {
        (Some(HarnessKind::Custom), _)
            if fallback.0 == HarnessKind::Custom
                && pinned_program.is_some()
                && review
                    .session_of(HarnessKind::Custom, pinned_program)
                    .is_none() =>
        {
            fallback
        }
        (Some(kind), _) => (
            kind,
            review
                .harness_session_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| kind.name().to_string()),
        ),
        (None, true) => (
            HarnessKind::ClaudeCode,
            HarnessKind::ClaudeCode.name().to_string(),
        ),
        (None, false) => fallback,
    }
}

/// The agent of `review`, falling back to the active one while it has no session.
pub fn of_review(snap: &Snapshot, review: &Review) -> String {
    pinned(
        review,
        configured(snap),
        snap.config.harness.program.as_deref(),
    )
    .1
}

/// The agent of the review in `tab`: the pinned one (the session's, else the config the tab
/// was opened with), so a later switch or rename in Config never changes an open review. A tab
/// with no review yet shows the agent it was opened with, else the active one.
pub fn of_tab(snap: &Snapshot, tab: &Tab) -> String {
    pinned_of_tab(snap, tab).1
}

/// The kind `of_tab` names.
pub fn kind_of_tab(snap: &Snapshot, tab: &Tab) -> HarnessKind {
    pinned_of_tab(snap, tab).0
}

fn pinned_of_tab(snap: &Snapshot, tab: &Tab) -> (HarnessKind, String) {
    let (opened, program) = match &tab.ui.opened_with {
        // A custom command opened with no program gives way to the configured harness while the
        // config is still Custom, as the daemon does: it could not run anything.
        Some((HarnessKind::Custom, _))
            if tab.ui.opened_program.is_none()
                && snap.config.harness.kind == HarnessKind::Custom =>
        {
            (configured(snap), snap.config.harness.program.clone())
        }
        Some(opened) => (opened.clone(), tab.ui.opened_program.clone()),
        None => (configured(snap), snap.config.harness.program.clone()),
    };
    match tab.ready() {
        Some(ready) => pinned(&ready.view.review, opened, program.as_deref()),
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
            opened_with: opened_with.map(|kind| (kind, kind.name().to_string())),
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

    #[test]
    fn the_active_name_of_a_custom_command_is_its_display_name() {
        let mut snap = Snapshot::default();
        snap.config.harness.kind = HarnessKind::Custom;
        assert_eq!(active(&snap), "Custom command");
        snap.config.harness.program = Some("/usr/local/bin/echo-harness".into());
        assert_eq!(active(&snap), "echo-harness");
        snap.config.harness.custom_name = "Echo".into();
        assert_eq!(active(&snap), "Echo");
    }

    #[test]
    fn a_saved_review_keeps_the_name_its_session_started_with() {
        let mut snap = Snapshot::default();
        snap.config.harness.kind = HarnessKind::ClaudeCode;
        let mut r = review();
        r.harness_session = Some("s-1".into());
        r.harness_session_kind = Some(HarnessKind::Custom);
        r.harness_session_name = Some("Echo".into());
        assert_eq!(of_review(&snap, &r), "Echo");
        r.harness_session_name = None;
        assert_eq!(
            of_review(&snap, &r),
            "Custom command",
            "a session saved without a name falls back to the kind's"
        );
    }

    #[test]
    fn a_session_of_another_program_does_not_name_the_review() {
        let mut snap = Snapshot::default();
        snap.config.harness.kind = HarnessKind::Custom;
        let mut tab = ready_tab(None, None);
        tab.ui.opened_with = Some((HarnessKind::Custom, "Echo".into()));
        tab.ui.opened_program = Some("/usr/local/bin/echo-harness".into());
        let Some(ready) = tab.ready_mut() else {
            panic!("ready")
        };
        ready.view.review.remember_session(
            "s-1",
            HarnessKind::Custom,
            Some("/usr/local/bin/other"),
            "Other",
        );
        assert_eq!(
            of_tab(&snap, &tab),
            "Echo",
            "the session belongs to another program: the pinned harness names the review"
        );
        let Some(ready) = tab.ready_mut() else {
            panic!("ready")
        };
        ready.view.review.remember_session(
            "s-2",
            HarnessKind::Custom,
            Some("/usr/local/bin/echo-harness"),
            "Echo 2",
        );
        assert_eq!(
            of_tab(&snap, &tab),
            "Echo 2",
            "the session of the pinned program names it"
        );
    }

    #[test]
    fn a_custom_tab_opened_without_a_program_follows_the_config() {
        let mut snap = Snapshot::default();
        snap.config.harness.kind = HarnessKind::Custom;
        let mut tab = ready_tab(None, None);
        tab.ui.opened_with = Some((HarnessKind::Custom, "Custom command".into()));
        tab.ui.opened_program = None;
        snap.config.harness.program = Some("/usr/local/bin/echo-harness".into());
        assert_eq!(
            of_tab(&snap, &tab),
            "echo-harness",
            "the program set later names the review, as it does in the daemon"
        );
        snap.config.harness.kind = HarnessKind::ClaudeCode;
        assert_eq!(
            of_tab(&snap, &tab),
            "Custom command",
            "the config left Custom: the tab keeps the name it was opened with"
        );
    }

    #[test]
    fn a_tab_keeps_the_custom_name_it_was_opened_with() {
        let mut snap = Snapshot::default();
        snap.config.harness.kind = HarnessKind::Custom;
        snap.config.harness.custom_name = "Echo".into();
        let mut tab = ready_tab(None, None);
        tab.ui.opened_with = Some((HarnessKind::Custom, "Echo".into()));
        tab.ui.opened_program = Some("/usr/local/bin/echo-harness".into());
        snap.config.harness.custom_name = "Renamed".into();
        assert_eq!(
            of_tab(&snap, &tab),
            "Echo",
            "renaming the harness later leaves the open review alone"
        );
        assert_eq!(kind_of_tab(&snap, &tab), HarnessKind::Custom);
        tab.ui.opened_with = None;
        assert_eq!(of_tab(&snap, &tab), "Renamed", "no pin: the active name");
    }
}
