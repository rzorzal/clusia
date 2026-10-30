//! The Checks on GitHub card: GitHub's check runs by name, as plain text.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use clusia_core::printable::printable;
use clusia_core::{CheckRun, PrRef};

use super::body::ChecksRegion;
use super::{CHECKS_SHOWN, TestsModels};
use crate::fonts::UiFonts;
use crate::review_state::ReviewTabs;
use crate::theme::Swatch;
use crate::ui::kit::{Type, card, text, wrapped_text};

/// One check name of the card. It is text only: nothing about it acts when clicked.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CheckName {
    pub pr: PrRef,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mark {
    Pass,
    Fail,
    Skip,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChecksView {
    pub passed: usize,
    pub total: usize,
    pub shown: Vec<(Mark, String)>,
    pub more: usize,
}

/// The view a `ChecksRegion` was drawn from.
#[derive(Component)]
pub(super) struct ChecksState(ChecksView);

/// A completed run with `success` passes; `failure`, `timed_out`, `action_required` and
/// `startup_failure` fail; any other conclusion of a completed run, or none, is skipped; a run
/// that is not completed is pending.
pub(super) fn mark_of(run: &CheckRun) -> Mark {
    if run.status != "completed" {
        return Mark::Pending;
    }
    match run.conclusion.as_deref() {
        Some("success") => Mark::Pass,
        Some("failure" | "timed_out" | "action_required" | "startup_failure") => Mark::Fail,
        _ => Mark::Skip,
    }
}

/// `shown` is the first `CHECKS_SHOWN` names, escaped as one line; `more` the rest.
pub(super) fn checks_view(runs: &[CheckRun]) -> ChecksView {
    ChecksView {
        passed: runs.iter().filter(|r| mark_of(r) == Mark::Pass).count(),
        total: runs.len(),
        shown: runs
            .iter()
            .take(CHECKS_SHOWN)
            .map(|r| (mark_of(r), printable(&r.name)))
            .collect(),
        more: runs.len().saturating_sub(CHECKS_SHOWN),
    }
}

fn glyph(mark: Mark) -> (&'static str, Swatch) {
    match mark {
        Mark::Pass => ("✓", Swatch::Green),
        Mark::Fail => ("✕", Swatch::Orange),
        Mark::Skip => ("–", Swatch::Muted),
        Mark::Pending => ("●", Swatch::Muted),
    }
}

pub(super) fn fill_checks(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    models: Res<TestsModels>,
    fonts: Res<UiFonts>,
    regions: Query<(Entity, &ChecksRegion, Option<&ChecksState>)>,
) {
    for (entity, region, state) in &regions {
        let pr = &region.0;
        if tabs.0.get(pr).and_then(|t| t.ready()).is_none() {
            continue;
        }
        let Some(view) = models.0.get(pr).and_then(|m| m.view.as_ref()) else {
            continue;
        };
        let checks = checks_view(&view.github_checks);
        if state.is_some_and(|s| s.0 == checks) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|p| {
            p.spawn(card(Node {
                width: percent(100),
                justify_content: JustifyContent::SpaceBetween,
                align_items: AlignItems::FlexStart,
                column_gap: px(16),
                padding: UiRect::axes(px(16), px(12)),
                ..default()
            }))
            .with_children(|row| {
                draw(row, &fonts, pr, &checks);
            });
        });
        commands.entity(entity).insert(ChecksState(checks));
    }
}

fn draw(row: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, checks: &ChecksView) {
    row.spawn(Node {
        flex_grow: 1.0,
        flex_basis: px(0),
        min_width: px(0),
        flex_wrap: FlexWrap::Wrap,
        align_items: AlignItems::Center,
        column_gap: px(14),
        row_gap: px(6),
        ..default()
    })
    .with_children(|names| {
        names.spawn(text(fonts, "Checks on GitHub", Type::STRONG));
        if checks.total == 0 {
            names.spawn(text(
                fonts,
                "No checks on GitHub for this commit.",
                Type::MUTED,
            ));
            return;
        }
        for (mark, name) in &checks.shown {
            let (mark, ink) = glyph(*mark);
            names
                .spawn((
                    Node {
                        column_gap: px(5),
                        ..default()
                    },
                    CheckName {
                        pr: pr.clone(),
                        name: name.clone(),
                    },
                ))
                .with_children(|one| {
                    one.spawn(text(fonts, mark, Type::BODY.ink(ink)));
                    one.spawn(wrapped_text(fonts, name.clone(), Type::BODY.ink(ink)));
                });
        }
        if checks.more > 0 {
            names.spawn(text(fonts, format!("+ {} more", checks.more), Type::MUTED));
        }
    });
    if checks.total > 0 {
        let ink = if checks.passed == checks.total {
            Swatch::Green
        } else {
            Swatch::Muted
        };
        row.spawn(text(
            fonts,
            format!("{} of {} passed", checks.passed, checks.total),
            Type::MUTED.ink(ink),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::review_state::ReviewSection;
    use crate::testing::{self, NOW};
    use crate::ui::markdown::{MdBody, MdLink};
    use clusia_core::tests::{TestsPhase, TestsView};

    fn check(name: &str, status: &str, conclusion: Option<&str>) -> CheckRun {
        CheckRun {
            name: name.into(),
            status: status.into(),
            conclusion: conclusion.map(Into::into),
            started_at: None,
            completed_at: None,
            details_url: Some(format!("https://github.com/rzorzal/clusia/runs/{name}")),
        }
    }

    fn show(checks: Vec<CheckRun>) -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        let mut view: TestsView = testing::tests_view(&app, &pr, TestsPhase::Ready);
        view.env = Some(testing::tests_env(true));
        view.recipe = Some(testing::tests_recipe());
        view.plan = Some(testing::tests_plan(Vec::new(), true));
        view.github_checks = checks;
        testing::open_tests(&mut app, &pr, view);
        testing::recorded(&mut app);
        (app, pr)
    }

    #[test]
    fn marks_follow_the_conclusion() {
        let m = |status, conclusion| mark_of(&check("x", status, conclusion));
        assert_eq!(m("completed", Some("success")), Mark::Pass);
        for c in ["failure", "timed_out", "action_required", "startup_failure"] {
            assert_eq!(m("completed", Some(c)), Mark::Fail, "{c}");
        }
        for c in ["cancelled", "skipped", "neutral", "stale"] {
            assert_eq!(m("completed", Some(c)), Mark::Skip, "{c}");
        }
        assert_eq!(m("in_progress", None), Mark::Pending);
        assert_eq!(m("queued", None), Mark::Pending);
        assert_eq!(m("completed", None), Mark::Skip);
    }

    #[test]
    fn the_card_counts_the_passed_checks() {
        let (mut app, _) = show(vec![
            check("build", "completed", Some("success")),
            check("clippy", "completed", Some("success")),
            check("test (macOS)", "completed", Some("failure")),
            check("deploy", "in_progress", None),
        ]);
        assert!(testing::shows(&mut app, "Checks on GitHub"));
        assert!(testing::shows(&mut app, "build"));
        assert!(testing::shows(&mut app, "test (macOS)"));
        assert!(testing::shows(&mut app, "2 of 4 passed"));
        assert!(testing::shows(&mut app, "✕"));
        assert!(testing::shows(&mut app, "●"));
    }

    #[test]
    fn more_than_twelve_names_show_a_count() {
        let many: Vec<CheckRun> = (0..15)
            .map(|i| check(&format!("job-{i:02}"), "completed", Some("success")))
            .collect();
        let (mut app, _) = show(many);
        assert!(testing::shows(&mut app, "job-11"));
        assert!(!testing::shows(&mut app, "job-12"));
        assert!(testing::shows(&mut app, "+ 3 more"));
        assert!(testing::shows(&mut app, "15 of 15 passed"));
        assert_eq!(
            testing::count::<CheckName>(&mut app),
            CHECKS_SHOWN,
            "only the names shown are nodes"
        );
    }

    #[test]
    fn no_checks_says_so() {
        let (mut app, _) = show(Vec::new());
        assert!(testing::shows(
            &mut app,
            "No checks on GitHub for this commit."
        ));
        assert!(!testing::shows(&mut app, "passed"));
    }

    #[test]
    fn check_names_never_link() {
        let nasty = "[docs](https://evil.example/x)\u{202e}\n**bold**";
        let (mut app, _pr) = show(vec![
            check(nasty, "completed", Some("success")),
            check("build", "completed", Some("failure")),
        ]);
        assert!(testing::shows(
            &mut app,
            "[docs](https://evil.example/x)\\u{202e}\\n**bold**"
        ));
        assert!(
            !testing::shows(&mut app, "github.com/rzorzal/clusia/runs"),
            "details_url is never shown"
        );
        assert_eq!(testing::count::<MdBody>(&mut app), 0);
        assert_eq!(testing::count::<MdLink>(&mut app), 0);
        for name in testing::entities::<CheckName>(&mut app) {
            testing::click(&mut app, name);
        }
        assert!(testing::recorded(&mut app).is_empty());
        assert!(
            app.world()
                .get_resource::<crate::platform_open::OpenUrls>()
                .is_none_or(|u| u.0.is_empty())
        );
    }
}
