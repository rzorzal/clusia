//! The result column: what the open case returned, the state it left against what was expected,
//! the error and the output, and where it ran. Everything here comes from the pull request's
//! agent or from running its code, so it is drawn as plain text after `printable`, never as
//! markdown, a link or a path.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::functions::{CaseResult, CaseStatus};
use clusia_core::printable::{printable, printable_lines};

use super::driver::{CommentOnFunction, ShowDriver, on_comment, on_show};
use super::{FunctionsModel, FunctionsModels, FunctionsResult};
use crate::fonts::UiFonts;
use crate::review_state::ReviewTabs;
use crate::screens::review::tests_tab::detail::cap_tail;
use crate::screens::review::tests_tab::{row_duration, runner_label};
use crate::theme::Swatch;
use crate::ui::kit::{Fill, Tone, Type, Variant, badge, button, card, text, wrapped_text};

/// The line of a column with nothing to show yet.
const RUN_HINT: &str = "Run this case to see what it returns.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Head {
    Plain,
    Pill(CaseStatus),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Body {
    Hint(&'static str),
    Result(Box<ShownResult>),
}

/// A result as the column draws it, every text already escaped. A `bool` beside a text says the
/// text did not meet what the case expected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShownResult {
    /// Why this is not the case's result, when it is not.
    pub note: Option<&'static str>,
    pub message: Option<String>,
    pub nothing_expected: bool,
    pub returned: Option<(String, bool)>,
    pub state_label: String,
    pub state: Vec<(String, bool)>,
    pub error: Option<String>,
    pub output: String,
    pub meta: String,
    pub function: String,
    pub driver: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResultView {
    pub head: Head,
    pub body: Body,
}

/// The `ResultView` the region it sits on was drawn from.
#[derive(Component)]
pub struct ResultBuilt(ResultView);

fn shown(model: &FunctionsModel, result: &CaseResult, note: Option<&'static str>) -> ShownResult {
    let function = model.selected_view();
    let case = model.open_view();
    let line = |text: &str| printable(text);
    let returned = result.returned.as_ref().map(|got| {
        match result
            .returned_expected
            .as_ref()
            .filter(|want| want.trim() != got.trim())
        {
            Some(want) => (format!("{} ← expected {}", line(got), line(want)), true),
            None => (line(got), false),
        }
    });
    let state = result
        .state
        .iter()
        .map(|check| {
            let got = check.got.as_deref().map_or("missing".to_string(), line);
            let unmet = check
                .expected
                .as_ref()
                .filter(|want| Some(*want) != check.got.as_ref());
            match unmet {
                Some(want) => (
                    format!("{} = {got} ← expected {}", line(&check.name), line(want)),
                    true,
                ),
                None => (format!("{} = {got}", line(&check.name)), false),
            }
        })
        .collect();
    let output = cap_tail(&result.output);
    let nothing_expected = case
        .is_some_and(|c| c.expect.returned.is_none() && c.expect.state.is_empty())
        && result.state.is_empty();
    ShownResult {
        note,
        message: result.message.as_deref().map(printable_lines),
        nothing_expected,
        returned,
        state_label: function
            .and_then(|f| f.state_label.as_deref())
            .map_or("State".to_string(), line),
        state,
        error: result.error.as_deref().map(printable_lines),
        output: printable_lines(output),
        meta: format!(
            "{} · {} · no network",
            row_duration(result.took_ms),
            runner_label(result.runner)
        ),
        function: function.map(|f| f.slug.clone()).unwrap_or_default(),
        driver: function.is_some_and(|f| f.driver.is_some()),
    }
}

/// What the column shows; `None` with no view or no selected function.
pub(super) fn result_view(model: &FunctionsModel) -> Option<ResultView> {
    let view = model.view.as_ref()?;
    model.selected_view()?;
    let hint = ResultView {
        head: Head::Plain,
        body: Body::Hint(RUN_HINT),
    };
    let Some(case) = model.open_view() else {
        return Some(hint);
    };
    let Some(result) = case.result.as_ref() else {
        return Some(hint);
    };
    if model.current_result(case).is_none() {
        let note = if result.hash != case.hash {
            "These values changed since this ran."
        } else {
            debug_assert!(result.head != view.head);
            "From an older commit."
        };
        return Some(ResultView {
            head: Head::Plain,
            body: Body::Result(Box::new(shown(model, result, Some(note)))),
        });
    }
    Some(ResultView {
        head: Head::Pill(result.status),
        body: Body::Result(Box::new(shown(model, result, None))),
    })
}

fn pill(status: CaseStatus) -> (&'static str, Tone) {
    match status {
        CaseStatus::Passed => ("passed", Tone::Green),
        CaseStatus::Failed => ("failed", Tone::Orange),
        CaseStatus::Error => ("error", Tone::Orange),
        CaseStatus::TimedOut => ("timed out", Tone::Orange),
        CaseStatus::Stopped => ("stopped", Tone::Orange),
    }
}

/// Redraws the column when what it shows changed: a result for a case that is not open changes
/// nothing here.
pub fn fill_result(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    models: Res<FunctionsModels>,
    fonts: Res<UiFonts>,
    regions: Query<(Entity, &FunctionsResult, Option<&ResultBuilt>)>,
) {
    for (entity, region, built) in &regions {
        let pr = &region.0;
        if tabs.0.get(pr).is_none_or(|t| t.ready().is_none()) {
            continue;
        }
        let Some(model) = models.0.get(pr) else {
            continue;
        };
        let Some(view) = result_view(model) else {
            if built.is_some() {
                commands.entity(entity).despawn_related::<Children>();
                commands.entity(entity).remove::<ResultBuilt>();
            }
            continue;
        };
        if built.is_some_and(|b| b.0 == view) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|p| {
            p.spawn(card(Node {
                flex_grow: 1.0,
                flex_direction: FlexDirection::Column,
                row_gap: px(10),
                padding: UiRect::all(px(16)),
                ..default()
            }))
            .with_children(|c| draw(c, &fonts, pr, &view));
        });
        commands.entity(entity).insert(ResultBuilt(view));
    }
}

type Spawner<'a> = ChildSpawnerCommands<'a>;

fn draw(c: &mut Spawner, fonts: &UiFonts, pr: &PrRef, view: &ResultView) {
    c.spawn(Node {
        align_items: AlignItems::Center,
        column_gap: px(8),
        ..default()
    })
    .with_children(|head| {
        head.spawn(text(fonts, "Result", Type::STRONG));
        if let Head::Pill(status) = &view.head {
            let (label, tone) = pill(*status);
            head.spawn(badge(fonts, label, tone));
        }
    });
    match &view.body {
        Body::Hint(line) => {
            c.spawn(wrapped_text(fonts, *line, Type::MUTED));
        }
        Body::Result(shown) => draw_result(c, fonts, pr, shown),
    }
}

fn draw_result(c: &mut Spawner, fonts: &UiFonts, pr: &PrRef, r: &ShownResult) {
    if let Some(note) = r.note {
        c.spawn(wrapped_text(fonts, note, Type::MUTED));
    }
    if let Some(message) = &r.message {
        c.spawn(wrapped_text(fonts, message.clone(), Type::MUTED));
    }
    if r.nothing_expected {
        c.spawn(wrapped_text(
            fonts,
            "Nothing was expected; this shows what the call did.",
            Type::MUTED,
        ));
    }
    if let Some((returned, unmet)) = &r.returned {
        c.spawn(text(fonts, "Returned", Type::META));
        block(c, fonts, returned.clone(), *unmet, false);
    }
    if !r.state.is_empty() {
        c.spawn(text(
            fonts,
            format!("{} after the call", r.state_label),
            Type::META,
        ));
        let any_unmet = r.state.iter().any(|(_, unmet)| *unmet);
        c.spawn((
            Node {
                width: percent(100),
                flex_direction: FlexDirection::Column,
                row_gap: px(2),
                padding: UiRect::all(px(12)),
                border_radius: BorderRadius::all(px(8)),
                ..default()
            },
            BackgroundColor::default(),
            Fill(if any_unmet {
                Swatch::OrangeSoft
            } else {
                Swatch::Chrome
            }),
        ))
        .with_children(|lines| {
            for (line, unmet) in &r.state {
                let ink = if *unmet { Swatch::Orange } else { Swatch::Fg };
                lines.spawn(wrapped_text(fonts, line.clone(), Type::MONO.ink(ink)));
            }
        });
    }
    if let Some(error) = &r.error {
        c.spawn(text(fonts, "Error", Type::META));
        block(c, fonts, error.clone(), true, false);
    }
    c.spawn(text(fonts, "Output", Type::META));
    if r.output.is_empty() {
        c.spawn(text(fonts, "No output", Type::MUTED));
    } else {
        block(c, fonts, r.output.clone(), false, true);
    }
    c.spawn(wrapped_text(fonts, r.meta.clone(), Type::MUTED));
    c.spawn(Node {
        flex_wrap: FlexWrap::Wrap,
        column_gap: px(14),
        ..default()
    })
    .with_children(|links| {
        if r.driver {
            links.spawn((
                button(fonts, "Show driver code", Variant::Ghost),
                ShowDriver {
                    pr: pr.clone(),
                    function: r.function.clone(),
                },
                observe(on_show),
            ));
        }
        links.spawn((
            button(fonts, "Comment on the function", Variant::Ghost),
            CommentOnFunction {
                pr: pr.clone(),
                function: r.function.clone(),
            },
            observe(on_comment),
        ));
    });
}

/// A mono block: orange when `unmet`, scrolling past its cap when `capped`.
fn block(c: &mut Spawner, fonts: &UiFonts, content: String, unmet: bool, capped: bool) {
    let (fill, ink) = if unmet {
        (Swatch::OrangeSoft, Swatch::Orange)
    } else {
        (Swatch::Chrome, Swatch::Fg)
    };
    let mut block = c.spawn((
        Node {
            width: percent(100),
            padding: UiRect::all(px(12)),
            border_radius: BorderRadius::all(px(8)),
            ..default()
        },
        BackgroundColor::default(),
        Fill(fill),
    ));
    if capped {
        block.insert((
            Node {
                width: percent(100),
                flex_grow: 1.0,
                flex_basis: px(0),
                min_height: px(60),
                max_height: px(260),
                padding: UiRect::all(px(12)),
                border_radius: BorderRadius::all(px(8)),
                overflow: Overflow::scroll_y(),
                ..default()
            },
            ScrollArea,
            ScrollPosition::default(),
        ));
    }
    block.with_child(wrapped_text(fonts, content, Type::MONO.ink(ink)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{FunctionsTell, Tell};
    use crate::fixture::{self, DemoFunctions};
    use crate::testing::{self, NOW};
    use clusia_core::functions::{CaseStatus, FunctionsView};

    fn app_with(view: FunctionsView) -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, crate::review_state::ReviewSection::Tests);
        testing::open_functions(&mut app, &pr, view);
        testing::recorded(&mut app);
        (app, pr)
    }

    fn mockup() -> FunctionsView {
        fixture::demo_functions(NOW, DemoFunctions::Mockup)
    }

    fn tell(app: &mut App, pr: &PrRef, view: FunctionsView) {
        testing::tell(
            app,
            Tell::Functions(FunctionsTell::Loaded {
                pr: pr.clone(),
                view: Box::new(view),
            }),
        );
    }

    #[test]
    fn the_failed_case_reads_as_the_mockup() {
        let (mut app, _pr) = app_with(mockup());
        for line in [
            "Result",
            "failed",
            "Returned",
            "Err(RefreshError::Http(500))",
            "Store after the call",
            "token = \"\" ← expected \"old\"",
            "Output",
            "WARN refresh failed: 500",
            "store cleared before retry",
            "0.21 s · container · no network",
            "Show driver code",
            "Comment on the function",
        ] {
            assert!(testing::shows(&mut app, line), "missing {line:?}");
        }
    }

    #[test]
    fn a_case_that_never_ran_says_how_to_see_a_result() {
        let mut view = mockup();
        view.functions[0].cases[0].result = None;
        let (mut app, _pr) = app_with(view);
        assert!(testing::shows(
            &mut app,
            "Run this case to see what it returns."
        ));
        assert!(!testing::shows(&mut app, "Returned"));
    }

    #[test]
    fn a_result_of_other_values_shows_not_run() {
        let mut view = mockup();
        view.functions[0].cases[0].hash = "other".into();
        let (mut app, pr) = app_with(view);
        assert!(testing::shows(
            &mut app,
            "These values changed since this ran."
        ));
        let model = &app.world().resource::<FunctionsModels>().0[&pr];
        let case = model.open_view().expect("an open case");
        assert!(
            model.current_result(case).is_none(),
            "the case has no current result, so the list counts none of it"
        );
        let (passed, failed) = model.counts(model.selected_view().unwrap());
        assert_eq!((passed, failed), (3, 0));
    }

    #[test]
    fn a_result_from_an_older_commit_is_marked() {
        let mut view = mockup();
        view.functions[0].cases[0].result.as_mut().unwrap().head = "oldhead".into();
        let (mut app, pr) = app_with(view);
        assert!(testing::shows(&mut app, "From an older commit."));
        let model = &app.world().resource::<FunctionsModels>().0[&pr];
        let case = model.open_view().unwrap();
        assert!(model.current_result(case).is_none());
    }

    #[test]
    fn each_status_has_its_pill_and_a_driver_that_did_not_report_says_why() {
        for (status, pill) in [
            (CaseStatus::Passed, "passed"),
            (CaseStatus::Failed, "failed"),
            (CaseStatus::Error, "error"),
            (CaseStatus::TimedOut, "timed out"),
            (CaseStatus::Stopped, "stopped"),
        ] {
            let mut view = mockup();
            let result = view.functions[0].cases[0].result.as_mut().unwrap();
            result.status = status;
            result.message = Some("The driver ended without a result (exit 101)".into());
            let (mut app, _pr) = app_with(view);
            assert!(testing::shows(&mut app, pill), "{pill}");
            assert!(testing::shows(
                &mut app,
                "The driver ended without a result (exit 101)"
            ));
        }
    }

    #[test]
    fn an_unmet_returned_value_and_a_missing_state_are_drawn_as_such() {
        let mut view = mockup();
        let result = view.functions[0].cases[0].result.as_mut().unwrap();
        result.returned = Some("Ok(Token(\"new\"))".into());
        result.returned_expected = Some("Err(RefreshError::Http(500))".into());
        result.state = vec![clusia_core::functions::StateCheck {
            name: "token".into(),
            got: None,
            expected: Some("\"old\"".into()),
        }];
        let (mut app, _pr) = app_with(view);
        assert!(testing::shows(
            &mut app,
            "Ok(Token(\"new\")) ← expected Err(RefreshError::Http(500))"
        ));
        assert!(testing::shows(
            &mut app,
            "token = missing ← expected \"old\""
        ));
    }

    #[test]
    fn a_case_with_no_expectations_says_what_the_result_is() {
        let mut view = mockup();
        let case = &mut view.functions[0].cases[0];
        case.expect = Default::default();
        let result = case.result.as_mut().unwrap();
        result.status = CaseStatus::Passed;
        result.returned_expected = None;
        result.state.clear();
        let (mut app, _pr) = app_with(view);
        assert!(testing::shows(
            &mut app,
            "Nothing was expected; this shows what the call did."
        ));
    }

    #[test]
    fn empty_output_says_so() {
        let mut view = mockup();
        view.functions[0].cases[0].result.as_mut().unwrap().output = String::new();
        let (mut app, _pr) = app_with(view);
        assert!(testing::shows(&mut app, "No output"));
    }

    #[test]
    fn case_text_output_and_driver_are_plain_text() {
        let mut view = mockup();
        let result = view.functions[0].cases[0].result.as_mut().unwrap();
        result.returned = Some("[x](https://example.test) **bold**\u{202e}".into());
        result.error = Some("\u{1b}[31mboom\u{1b}[0m [e](https://example.test)".into());
        result.output = "line one\n# not a heading\n[o](https://example.test)\u{1b}[1m".into();
        result.state[0].got = Some("`code` \u{202e}".into());
        let (mut app, pr) = app_with(view);
        assert!(testing::shows(
            &mut app,
            "[x](https://example.test) **bold**"
        ));
        assert!(testing::shows(&mut app, "# not a heading"));
        assert_eq!(
            testing::count::<crate::ui::markdown::MdBody>(&mut app),
            0,
            "none of it is parsed as markdown"
        );
        assert_eq!(testing::count::<crate::ui::markdown::MdLink>(&mut app), 0);
        // Clicking drawn text does nothing: no ask leaves the window.
        assert!(testing::recorded(&mut app).is_empty());
        let _ = pr;
    }

    #[test]
    fn huge_output_is_drawn_from_its_end_and_capped() {
        let mut view = mockup();
        let big = format!("{}END", "x".repeat(clusia_core::tests::TAIL_MAX * 3));
        view.functions[0].cases[0].result.as_mut().unwrap().output = big;
        let (mut app, _pr) = app_with(view);
        assert!(testing::shows(&mut app, "END"));
        let longest = testing::shown_text(&mut app)
            .into_iter()
            .map(|t| t.len())
            .max()
            .unwrap_or(0);
        assert!(longest <= clusia_core::tests::TAIL_MAX + 8, "{longest}");
    }

    #[test]
    fn a_case_done_for_another_case_does_not_rebuild_the_column() {
        let (mut app, pr) = app_with(mockup());
        let region = testing::find::<FunctionsResult>(&mut app, |r| r.0 == pr);
        let before = testing::tree_signature(&app, region);
        let mut view = mockup();
        view.functions[0].cases[2].result.as_mut().unwrap().took_ms = 999;
        tell(&mut app, &pr, view);
        assert_eq!(testing::tree_signature(&app, region), before);
    }
}
