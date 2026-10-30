//! The failure detail, the live log and the setup-step tail of the Auto Tests layout.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, ScrollArea, observe};
use clusia_core::printable::{plain_relative, printable, printable_lines};
use clusia_core::tests::{RunnerKind, TAIL_MAX, TestRun, TestStatus};
use clusia_core::{PrRef, Side, can_comment};
use clusia_view::diff::{parse_patch, row_of};

use super::body::DetailRegion;
use super::list::{can_run, selected_name};
use super::{
    TabState, TestCaseResult, TestsModel, TestsModels, comment_prefill, failure_place,
    runner_label, tab_state,
};
use crate::bridge::{Ask, Asks, Model, Toasts};
use crate::fonts::UiFonts;
use crate::review_state::{EditTarget, Editor, ReviewTabs, SHOW_STEP};
use crate::screens::review::agent::suggestion::show_file;
use crate::screens::review::diff::unsent;
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::theme::Swatch;
use crate::ui::composer::ComposerMode;
use crate::ui::kit::{Fill, Type, Variant, button, card, text, wrapped_text};

/// The longest path of a test's file the detail still opens in the editor.
const FILE_MAX: usize = 512;

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LiveLog(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CommentOnLine(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FullLog(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct OpenTestInEditor(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RunAgain(pub PrRef);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Glyph {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TestDetail {
    pub glyph: Glyph,
    pub name: String,
    pub meta: String,
    pub block: String,
    pub can_run: bool,
    pub open_in_editor: bool,
    pub full_log: bool,
    pub comment: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OutputDetail {
    pub meta: String,
    pub block: String,
    pub full_log: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DetailView {
    Select,
    Listing,
    Live,
    /// The title of the step being set up, empty when the daemon names none.
    SettingUp {
        title: String,
    },
    Test(Box<TestDetail>),
    Output(Box<OutputDetail>),
}

/// The view a `DetailRegion` was drawn from.
#[derive(Component)]
pub(super) struct DetailState(DetailView);

/// `file:line · exit <code> · <container|macOS sandbox>`, a missing part left out with its
/// separator; every part escaped as one line.
pub(super) fn meta_line(
    place: Option<(String, Option<u32>)>,
    exit: Option<i32>,
    runner: RunnerKind,
) -> String {
    let mut parts = Vec::new();
    match place {
        Some((file, Some(line))) => parts.push(format!("{}:{line}", printable(&file))),
        Some((file, None)) => parts.push(printable(&file)),
        None => {}
    }
    if let Some(code) = exit {
        parts.push(format!("exit {code}"));
    }
    parts.push(runner_label(runner).to_string());
    parts.join(" · ")
}

/// The end of `tail`, at most `TAIL_MAX` bytes, starting on a character boundary.
pub(super) fn cap_tail(tail: &str) -> &str {
    if tail.len() <= TAIL_MAX {
        return tail;
    }
    let mut start = tail.len() - TAIL_MAX;
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    &tail[start..]
}

/// What the live log shows: the live run's tail, or the tail of the step being set up, as
/// `printable_lines` and capped. The escapes make the text longer, so it is capped again.
pub(super) fn live_text(model: &TestsModel) -> String {
    let tail = match model.state() {
        Some(TabState::Preparing { step: Some(id) }) => {
            model.steps.get(&id).and_then(|(_, tail)| tail.as_deref())
        }
        _ => model.live.as_ref().map(|live| live.tail.as_str()),
    };
    let escaped = printable_lines(cap_tail(tail.unwrap_or_default()));
    cap_tail(&escaped).to_string()
}

/// The test the detail is about and the run it ran in: the selected one, by default the first
/// failure of the listed run.
fn selected_result(model: &TestsModel) -> Option<(&TestRun, &TestCaseResult)> {
    let name = selected_name(model)?;
    let run = model.listed_run()?;
    let result = run.results.iter().find(|r| r.name == name)?;
    Some((run, result))
}

fn command_line(run: &TestRun) -> String {
    format!("$ {}", printable(&run.command))
}

fn glyph_of(status: TestStatus) -> Glyph {
    match status {
        TestStatus::Passed => Glyph::Pass,
        TestStatus::Failed => Glyph::Fail,
        TestStatus::Skipped => Glyph::Skip,
    }
}

/// The path of `file` inside `worktree`, when `file` is a plain relative path.
fn in_worktree(worktree: Option<&str>, file: &str) -> Option<String> {
    let worktree = worktree?;
    plain_relative(file, FILE_MAX).then(|| format!("{worktree}/{file}"))
}

/// What the region shows; the first match wins.
pub(super) fn detail_view(
    model: &TestsModel,
    worktree: Option<&str>,
    can_run: bool,
) -> Option<DetailView> {
    let view = model.view.as_ref()?;
    match tab_state(view, model.live.as_ref()) {
        TabState::Listing => return Some(DetailView::Listing),
        TabState::Running if model.live.is_some() => return Some(DetailView::Live),
        TabState::Preparing { step } => {
            let title = step
                .map(|id| {
                    let step = view
                        .plan
                        .as_ref()
                        .and_then(|plan| plan.steps.iter().find(|s| s.id == id));
                    printable(step.map_or(id.as_str(), |s| s.title.as_str()))
                })
                .unwrap_or_default();
            return Some(DetailView::SettingUp { title });
        }
        _ => {}
    }
    let Some(run) = model.listed_run() else {
        return Some(DetailView::Select);
    };
    if let Some((_, result)) = selected_result(model) {
        let place = failure_place(result);
        let mut block = command_line(run);
        if let Some(message) = &result.message {
            block.push('\n');
            block.push_str(&printable_lines(message));
        }
        return Some(DetailView::Test(Box::new(TestDetail {
            glyph: glyph_of(result.status),
            name: printable(&result.name),
            meta: meta_line(place.clone(), run.exit_code, run.runner),
            block,
            can_run,
            open_in_editor: place
                .as_ref()
                .is_some_and(|(file, _)| in_worktree(worktree, file).is_some()),
            full_log: run.log.is_some(),
            comment: if place.is_some_and(|(_, line)| line.is_some()) {
                "Comment on this line"
            } else {
                "Comment on this test"
            },
        })));
    }
    if run.results.is_empty() {
        let mut block = command_line(run);
        if !run.tail.is_empty() {
            block.push('\n');
            block.push_str(&printable_lines(cap_tail(&run.tail)));
        }
        return Some(DetailView::Output(Box::new(OutputDetail {
            meta: meta_line(None, run.exit_code, run.runner),
            block,
            full_log: run.log.is_some(),
        })));
    }
    Some(DetailView::Select)
}

pub(super) fn fill_detail(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    models: Res<TestsModels>,
    fonts: Res<UiFonts>,
    regions: Query<(Entity, &DetailRegion, Option<&DetailState>)>,
) {
    for (entity, region, state) in &regions {
        let pr = &region.0;
        let Some(ready) = tabs.0.get(pr).and_then(|t| t.ready()) else {
            continue;
        };
        let Some(model) = models.0.get(pr) else {
            continue;
        };
        let Some(view) = detail_view(model, ready.view.worktree.as_deref(), can_run(model)) else {
            continue;
        };
        if state.is_some_and(|s| s.0 == view) {
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
            .with_children(|col| draw(col, &fonts, pr, &view, live_text(model)));
        });
        commands.entity(entity).insert(DetailState(view));
    }
}

type Spawner<'a> = ChildSpawnerCommands<'a>;

fn draw(col: &mut Spawner, fonts: &UiFonts, pr: &PrRef, view: &DetailView, live: String) {
    match view {
        DetailView::Select => {
            col.spawn(text(fonts, "Select a test to see its output.", Type::MUTED));
        }
        DetailView::Listing => {
            col.spawn(text(fonts, "Listing the tests…", Type::MUTED));
        }
        DetailView::Live => {
            col.spawn(text(fonts, "Running", Type::STRONG));
            block(col, fonts, pr, live, true);
        }
        DetailView::SettingUp { title } => {
            let header = if title.is_empty() {
                "Setting up".to_string()
            } else {
                format!("Setting up · {title}")
            };
            col.spawn(wrapped_text(fonts, header, Type::STRONG));
            block(col, fonts, pr, live, true);
        }
        DetailView::Test(t) => draw_test(col, fonts, pr, t),
        DetailView::Output(o) => {
            col.spawn(text(fonts, "Run output", Type::STRONG));
            col.spawn(text(fonts, o.meta.clone(), Type::MUTED));
            block(col, fonts, pr, o.block.clone(), false);
            if o.full_log {
                links(col, |row| {
                    row.spawn((
                        button(fonts, "Full log", Variant::Link),
                        FullLog(pr.clone()),
                        observe(on_full_log),
                    ));
                });
            }
        }
    }
}

fn draw_test(col: &mut Spawner, fonts: &UiFonts, pr: &PrRef, t: &TestDetail) {
    col.spawn(Node {
        justify_content: JustifyContent::SpaceBetween,
        align_items: AlignItems::FlexStart,
        column_gap: px(12),
        ..default()
    })
    .with_children(|head| {
        let (glyph, ink) = match t.glyph {
            Glyph::Pass => ("✓", Swatch::Green),
            Glyph::Fail => ("✕", Swatch::Orange),
            Glyph::Skip => ("–", Swatch::Muted),
        };
        head.spawn(Node {
            flex_grow: 1.0,
            flex_basis: px(0),
            min_width: px(0),
            column_gap: px(8),
            ..default()
        })
        .with_children(|name| {
            name.spawn(text(fonts, glyph, Type::STRONG.ink(ink)));
            name.spawn(wrapped_text(fonts, t.name.clone(), Type::STRONG));
        });
        if t.can_run {
            head.spawn((
                button(fonts, "▶ Run again", Variant::Secondary),
                RunAgain(pr.clone()),
                observe(on_run_again),
            ));
        }
    });
    col.spawn(wrapped_text(fonts, t.meta.clone(), Type::MUTED));
    block(col, fonts, pr, t.block.clone(), false);
    links(col, |row| {
        if t.open_in_editor {
            row.spawn((
                button(fonts, "Open in editor", Variant::Link),
                OpenTestInEditor(pr.clone()),
                observe(on_open_in_editor),
            ));
        }
        if t.full_log {
            row.spawn((
                button(fonts, "Full log", Variant::Link),
                FullLog(pr.clone()),
                observe(on_full_log),
            ));
        }
        row.spawn((
            button(fonts, t.comment, Variant::Link),
            CommentOnLine(pr.clone()),
            observe(on_comment),
        ));
    });
}

fn links(col: &mut Spawner, fill: impl FnOnce(&mut Spawner)) {
    col.spawn(Node {
        flex_wrap: FlexWrap::Wrap,
        column_gap: px(14),
        ..default()
    })
    .with_children(fill);
}

/// The mono block of output: one text, in a box that scrolls when it is taller than its cap.
/// A live block's text carries `LiveLog` and is replaced in place afterwards.
fn block(col: &mut Spawner, fonts: &UiFonts, pr: &PrRef, content: String, live: bool) {
    col.spawn((
        Node {
            width: percent(100),
            max_height: px(260),
            padding: UiRect::all(px(12)),
            border_radius: BorderRadius::all(px(8)),
            overflow: Overflow::scroll_y(),
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Chrome),
        ScrollArea,
        ScrollPosition::default(),
    ))
    .with_children(|b| {
        let mut log = b.spawn(wrapped_text(fonts, content, Type::MONO.ink(Swatch::Fg)));
        if live {
            log.insert(LiveLog(pr.clone()));
        }
    });
}

/// Writes the live log's text in place, only when it differs, so an unchanged tail never marks
/// the `Text` changed.
pub(super) fn update_live_log(models: Res<TestsModels>, mut logs: Query<(&LiveLog, &mut Text)>) {
    for (log, mut text) in &mut logs {
        let want = models.0.get(&log.0).map(live_text).unwrap_or_default();
        if text.0 != want {
            text.0 = want;
        }
    }
}

fn on_run_again(
    activate: On<Activate>,
    buttons: Query<&RunAgain>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut models: ResMut<TestsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(RunAgain(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let Some(name) = models
        .0
        .get(pr)
        .and_then(selected_result)
        .map(|(_, r)| r.name.clone())
    else {
        return;
    };
    super::list::run_one(
        pr,
        &name,
        &tabs,
        &model,
        &mut models,
        &mut asks,
        &mut toasts,
        &time,
    );
}

fn on_open_in_editor(
    activate: On<Activate>,
    buttons: Query<&OpenTestInEditor>,
    tabs: Res<ReviewTabs>,
    models: Res<TestsModels>,
    mut asks: ResMut<Asks>,
) {
    let Ok(OpenTestInEditor(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let worktree = tabs
        .0
        .get(pr)
        .and_then(|t| t.ready())
        .and_then(|r| r.view.worktree.as_deref());
    let Some((file, line)) = models
        .0
        .get(pr)
        .and_then(selected_result)
        .and_then(|(_, result)| failure_place(result))
    else {
        return;
    };
    if let Some(path) = in_worktree(worktree, &file) {
        asks.send(Ask::OpenInEditor { path, line });
    }
}

fn on_full_log(
    activate: On<Activate>,
    buttons: Query<&FullLog>,
    models: Res<TestsModels>,
    mut asks: ResMut<Asks>,
) {
    let Ok(FullLog(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(path) = models
        .0
        .get(pr)
        .and_then(|m| m.listed_run())
        .and_then(|run| run.log.clone())
    {
        asks.send(Ask::OpenInEditor { path, line: None });
    }
}

/// Opens the composer on the failing line when the pull request's diff has it, else the general
/// note. The model is read at the click, never a copy taken when the button was drawn.
fn on_comment(
    activate: On<Activate>,
    buttons: Query<&CommentOnLine>,
    model: Res<Model>,
    models: Res<TestsModels>,
    mut tabs: ResMut<ReviewTabs>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(CommentOnLine(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    let Some((_, result)) = models.0.get(pr).and_then(selected_result) else {
        return;
    };
    let Some(tab) = tabs.0.get_mut(pr) else {
        return;
    };
    if unsent(tab.ui.editor.as_ref()) {
        return not_sent(
            &mut toasts,
            &time,
            "Finish or cancel your open comment first",
        );
    }
    let place = failure_place(result);
    let patch = place.as_ref().and_then(|(file, _)| {
        tab.ready()?
            .view
            .diff
            .iter()
            .find(|f| &f.path == file)
            .and_then(|f| f.patch.as_deref())
    });
    let line = place.as_ref().and_then(|(_, line)| *line);
    let on_line = line
        .zip(patch)
        .filter(|(line, patch)| can_comment(patch, Side::Right, *line))
        .map(|(line, patch)| (line, row_of(&parse_patch(patch), Side::Right, line)));
    let (target, prefill) = match (&place, on_line) {
        (Some((file, _)), Some((line, row))) => {
            let target = EditTarget::Line {
                path: file.clone(),
                side: Side::Right,
                start: None,
                line,
            };
            show_file(&mut tab.ui, file);
            // The Diff draws its rows in steps; the composer sits under the row of `line`, so
            // that row (and the ones above it) must be among the drawn ones.
            if let Some(row) = row {
                tab.ui.shown = tab.ui.shown.max((row + 1).div_ceil(SHOW_STEP) * SHOW_STEP);
            }
            (
                target,
                comment_prefill(&result.name, Some((file, Some(line))), true),
            )
        }
        _ => (
            EditTarget::General,
            comment_prefill(
                &result.name,
                place.as_ref().map(|(file, line)| (file.as_str(), *line)),
                false,
            ),
        ),
    };
    tab.ui.editor = Some(Editor {
        target,
        text: prefill,
        error: None,
        ticket: None,
        mode: ComposerMode::Write,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Ask, Connection, Tell, TestsAskKind, TestsTell};
    use crate::fixture;
    use crate::review_state::{EditTarget, ReviewSection, ReviewTabs};
    use crate::testing::{self, NOW};
    use clusia_core::tests::{
        PlanStep, RunOutcome, RunnerKind, StepPlace, StepState, TAIL_MAX, TestRun, TestScope,
        TestStatus, TestsPhase, TestsView, Totals,
    };

    const FILE: &str = "src/auth/refresh.rs";
    const WORKTREE: &str = "~/.clusia/worktrees/rzorzal/clusia/123";

    fn tests_app() -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        (app, pr)
    }

    fn failing(name: &str, file: Option<&str>, message: Option<&str>) -> TestCaseResult {
        TestCaseResult {
            name: name.into(),
            file: file.map(Into::into),
            status: TestStatus::Failed,
            duration_ms: Some(210),
            message: message.map(Into::into),
        }
    }

    fn passing(name: &str) -> TestCaseResult {
        TestCaseResult {
            name: name.into(),
            file: None,
            status: TestStatus::Passed,
            duration_ms: Some(20),
            message: None,
        }
    }

    fn run_of(head: &str, results: Vec<TestCaseResult>) -> TestRun {
        let failed = results
            .iter()
            .filter(|r| r.status == TestStatus::Failed)
            .count() as u32;
        let totals = Totals {
            passed: results.len() as u32 - failed,
            failed,
            skipped: 0,
        };
        let mut run = testing::tests_run(head, NOW - 240, RunOutcome::Failed, totals);
        run.id = "r1".into();
        run.exit_code = Some(101);
        run.command = "cargo test -p store -- --exact refresh".into();
        run.log = Some("/tmp/clusia-test-home/tests/logs/rzorzal~clusia~123/r1.log".into());
        run.results = results;
        run
    }

    fn set_worktree(app: &mut App, pr: &PrRef) {
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(pr)
            .and_then(|t| t.ready_mut())
            .expect("a ready tab")
            .view
            .worktree = Some(WORKTREE.into());
    }

    /// The review open on Tests with `edit` applied to a ready view holding `runs`.
    fn show(runs: Vec<TestRun>, edit: impl FnOnce(&mut TestsView)) -> (App, PrRef) {
        let (mut app, pr) = tests_app();
        set_worktree(&mut app, &pr);
        let mut view = testing::tests_view(&app, &pr, TestsPhase::Ready);
        view.env = Some(testing::tests_env(true));
        view.recipe = Some(testing::tests_recipe());
        view.plan = Some(testing::tests_plan(Vec::new(), true));
        view.runs = runs;
        edit(&mut view);
        testing::open_tests(&mut app, &pr, view);
        testing::recorded(&mut app);
        (app, pr)
    }

    fn head(app: &App, pr: &PrRef) -> String {
        testing::tests_view(app, pr, TestsPhase::Ready).head
    }

    fn refresh_failure() -> TestCaseResult {
        failing(
            "store::refresh::failed_exchange_keeps_old_token",
            Some("crates/store/src/refresh.rs"),
            Some(
                "thread 'refresh' panicked at crates/store/src/refresh.rs:212:9:\nassertion `left == right` failed",
            ),
        )
    }

    fn model(app: &App, pr: &PrRef) -> TestsModel {
        let m = &app.world().resource::<TestsModels>().0[pr];
        TestsModel {
            selected: m.selected.clone(),
            pending: m.pending,
            ..TestsModel::default()
        }
    }

    #[test]
    fn the_meta_line_leaves_out_what_is_missing() {
        let place = Some(("crates/store/src/refresh.rs".to_string(), Some(212)));
        assert_eq!(
            meta_line(place.clone(), Some(101), RunnerKind::Container),
            "crates/store/src/refresh.rs:212 · exit 101 · container"
        );
        assert_eq!(
            meta_line(Some(("a.rs".into(), None)), None, RunnerKind::Container),
            "a.rs · container"
        );
        assert_eq!(meta_line(None, None, RunnerKind::Local), "macOS sandbox");
        assert_eq!(
            meta_line(None, Some(1), RunnerKind::Local),
            "exit 1 · macOS sandbox"
        );
        assert_eq!(
            meta_line(Some(("a\nb.rs".into(), Some(3))), None, RunnerKind::Local),
            "a\\nb.rs:3 · macOS sandbox"
        );
    }

    #[test]
    fn the_tail_keeps_its_end_on_a_character_boundary() {
        assert_eq!(cap_tail("short"), "short");
        let long = "é".repeat(TAIL_MAX);
        let cut = cap_tail(&long);
        assert!(cut.len() <= TAIL_MAX);
        assert!(cut.chars().all(|c| c == 'é'));
        let text = format!("{}END", "x".repeat(TAIL_MAX * 2));
        assert!(cap_tail(&text).ends_with("END"));
        assert!(cap_tail(&text).len() <= TAIL_MAX);
    }

    #[test]
    fn the_first_failure_is_shown_by_default() {
        let (mut app, pr) = tests_app();
        set_worktree(&mut app, &pr);
        let h = head(&app, &pr);
        let mut view = testing::tests_view(&app, &pr, TestsPhase::Ready);
        view.recipe = Some(testing::tests_recipe());
        view.runs = vec![run_of(&h, vec![passing("store::ok"), refresh_failure()])];
        testing::open_tests(&mut app, &pr, view);
        assert!(testing::shows(
            &mut app,
            "store::refresh::failed_exchange_keeps_old_token"
        ));
        assert!(testing::shows(
            &mut app,
            "crates/store/src/refresh.rs:212 · exit 101 · container"
        ));
        assert!(testing::shows(
            &mut app,
            "$ cargo test -p store -- --exact refresh\nthread 'refresh' panicked at crates/store/src/refresh.rs:212:9:\nassertion `left == right` failed"
        ));
        assert!(testing::shows(&mut app, "Run again"));
        assert!(testing::shows(&mut app, "Open in editor"));
        assert!(testing::shows(&mut app, "Full log"));
        assert!(testing::shows(&mut app, "Comment on this line"));
    }

    #[test]
    fn selecting_a_passed_test_shows_it() {
        let (mut app, pr) = tests_app();
        let h = head(&app, &pr);
        let mut view = testing::tests_view(&app, &pr, TestsPhase::Ready);
        view.recipe = Some(testing::tests_recipe());
        view.runs = vec![run_of(&h, vec![passing("store::ok"), refresh_failure()])];
        testing::open_tests(&mut app, &pr, view);
        let row =
            testing::find::<crate::screens::review::tests_tab::list::TestRow>(&mut app, |r| {
                r.name == "store::ok"
            });
        testing::activate(&mut app, row);
        testing::settle(&mut app);
        assert!(testing::shows(
            &mut app,
            "$ cargo test -p store -- --exact refresh"
        ));
        assert!(testing::shows(&mut app, "Comment on this test"));
        assert!(
            !testing::shows(&mut app, "assertion `left == right` failed"),
            "the failure's message is not this test's"
        );
    }

    #[test]
    fn a_run_without_per_test_results_shows_its_tail() {
        let (mut app, pr) = tests_app();
        let h = head(&app, &pr);
        let mut run = run_of(&h, Vec::new());
        run.tail = "error: could not compile `store`\n\tcaused by: boom".into();
        let mut view = testing::tests_view(&app, &pr, TestsPhase::Ready);
        view.recipe = Some(testing::tests_recipe());
        view.runs = vec![run];
        testing::open_tests(&mut app, &pr, view);
        assert!(testing::shows(&mut app, "Run output"));
        assert!(testing::shows(
            &mut app,
            "$ cargo test -p store -- --exact refresh\nerror: could not compile `store`\n\tcaused by: boom"
        ));
        assert!(testing::shows(&mut app, "Full log"));
        assert!(!testing::shows(&mut app, "Comment on this"));
    }

    #[test]
    fn with_no_run_it_asks_to_select() {
        let (mut app, _) = show(Vec::new(), |_| {});
        assert!(testing::shows(&mut app, "Select a test to see its output."));
    }

    #[test]
    fn test_names_and_output_are_plain_text() {
        let nasty_name = "[x](https://evil.example)\u{202e}`tick`";
        let nasty_msg = "boom\n\tat [here](https://evil.example)\u{1b}[31m\u{202e}";
        let (mut app, pr) = show(Vec::new(), |_| {});
        let h = head(&app, &pr);
        let mut view = testing::tests_view(&app, &pr, TestsPhase::Ready);
        view.recipe = Some(testing::tests_recipe());
        view.runs = vec![run_of(
            &h,
            vec![failing(nasty_name, Some("src/a.rs"), Some(nasty_msg))],
        )];
        testing::open_tests(&mut app, &pr, view);
        assert!(testing::shows(
            &mut app,
            "[x](https://evil.example)\\u{202e}`tick`"
        ));
        assert!(testing::shows(
            &mut app,
            "boom\n\tat [here](https://evil.example)\\u{1b}[31m\\u{202e}"
        ));
        assert_eq!(testing::count::<crate::ui::markdown::MdBody>(&mut app), 0);
        assert_eq!(testing::count::<crate::ui::markdown::MdLink>(&mut app), 0);
        // Nothing in the detail acts on its content when it is clicked.
        let hostile: Vec<Entity> = {
            let mut texts = app.world_mut().query::<(Entity, &Text)>();
            texts
                .iter(app.world())
                .filter(|(_, t)| t.0.contains("evil.example"))
                .map(|(e, _)| e)
                .collect()
        };
        assert!(!hostile.is_empty());
        for text in hostile {
            testing::click(&mut app, text);
        }
        assert!(testing::recorded(&mut app).is_empty());
        assert!(
            app.world()
                .get_resource::<crate::platform_open::OpenUrls>()
                .is_none_or(|u| u.0.is_empty())
        );
    }

    #[test]
    fn run_again_runs_that_test_alone() {
        let (mut app, pr) = tests_app();
        let h = head(&app, &pr);
        let mut view = testing::tests_view(&app, &pr, TestsPhase::Ready);
        view.recipe = Some(testing::tests_recipe());
        view.runs = vec![run_of(&h, vec![refresh_failure()])];
        testing::open_tests(&mut app, &pr, view);
        testing::recorded(&mut app);
        let again = testing::find::<RunAgain>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, again);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RunTests {
                pr: pr.clone(),
                scope: TestScope::One {
                    name: "store::refresh::failed_exchange_keeps_old_token".into()
                }
            }]
        );
        assert_eq!(model(&app, &pr).pending, Some(TestsAskKind::Run));
    }

    #[test]
    fn open_in_editor_joins_the_worktree_and_the_file() {
        let (mut app, pr) = show(Vec::new(), |_| {});
        let h = head(&app, &pr);
        let mut view = testing::tests_view(&app, &pr, TestsPhase::Ready);
        view.recipe = Some(testing::tests_recipe());
        view.runs = vec![run_of(&h, vec![refresh_failure()])];
        testing::open_tests(&mut app, &pr, view);
        testing::recorded(&mut app);
        let open = testing::find::<OpenTestInEditor>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, open);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::OpenInEditor {
                path: format!("{WORKTREE}/crates/store/src/refresh.rs"),
                line: Some(212)
            }]
        );
        let log = testing::find::<FullLog>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, log);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::OpenInEditor {
                path: "/tmp/clusia-test-home/tests/logs/rzorzal~clusia~123/r1.log".into(),
                line: None
            }]
        );
    }

    #[test]
    fn a_file_that_could_leave_the_worktree_is_not_opened() {
        for bad in [
            "../../etc/passwd",
            "/etc/passwd",
            "a\u{1b}[2J.rs",
            "a/../../b.rs",
        ] {
            let (mut app, pr) = tests_app();
            set_worktree(&mut app, &pr);
            let h = head(&app, &pr);
            let mut view = testing::tests_view(&app, &pr, TestsPhase::Ready);
            view.recipe = Some(testing::tests_recipe());
            view.runs = vec![run_of(&h, vec![failing("t", Some(bad), Some("x"))])];
            testing::open_tests(&mut app, &pr, view);
            assert_eq!(
                testing::count::<OpenTestInEditor>(&mut app),
                0,
                "{bad:?} has no Open in editor"
            );
        }
    }

    #[test]
    fn links_without_a_target_are_not_drawn() {
        let (mut app, pr) = tests_app();
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .and_then(|t| t.ready_mut())
            .expect("a ready tab")
            .view
            .worktree = None;
        let h = head(&app, &pr);
        let mut run = run_of(&h, vec![failing("t", Some("src/a.rs"), None)]);
        run.log = None;
        let mut view = testing::tests_view(&app, &pr, TestsPhase::Ready);
        view.recipe = Some(testing::tests_recipe());
        view.runs = vec![run];
        testing::open_tests(&mut app, &pr, view);
        assert_eq!(testing::count::<FullLog>(&mut app), 0, "no log stored");
        assert_eq!(
            testing::count::<OpenTestInEditor>(&mut app),
            0,
            "the review has no worktree"
        );
    }

    fn comment_app(file: &str, message: &str) -> (App, PrRef) {
        let (mut app, pr) = tests_app();
        let h = head(&app, &pr);
        let mut view = testing::tests_view(&app, &pr, TestsPhase::Ready);
        view.recipe = Some(testing::tests_recipe());
        view.runs = vec![run_of(
            &h,
            vec![failing("store::refresh::boom", Some(file), Some(message))],
        )];
        testing::open_tests(&mut app, &pr, view);
        testing::recorded(&mut app);
        (app, pr)
    }

    fn editor(app: &App, pr: &PrRef) -> Option<crate::review_state::Editor> {
        app.world().resource::<ReviewTabs>().0[pr].ui.editor.clone()
    }

    #[test]
    fn a_failure_on_a_diff_line_opens_the_composer_in_the_diff() {
        let (mut app, pr) = comment_app(FILE, "panicked at src/auth/refresh.rs:40:9:\nboom");
        assert!(testing::shows(&mut app, "Comment on this line"));
        let comment = testing::find::<CommentOnLine>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, comment);
        let e = editor(&app, &pr).expect("an editor");
        assert_eq!(
            e.target,
            EditTarget::Line {
                path: FILE.into(),
                side: clusia_core::Side::Right,
                start: None,
                line: 40
            }
        );
        assert_eq!(e.text, "`store::refresh::boom` fails here.");
        let ui = &app.world().resource::<ReviewTabs>().0[&pr].ui;
        assert_eq!(ui.section, ReviewSection::Diff);
        assert_eq!(ui.file.as_deref(), Some(FILE));
    }

    #[test]
    fn a_failure_outside_the_diff_opens_the_general_note() {
        let (mut app, pr) = comment_app(FILE, "panicked at src/auth/refresh.rs:400:9:\nboom");
        let comment = testing::find::<CommentOnLine>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, comment);
        let e = editor(&app, &pr).expect("an editor");
        assert_eq!(e.target, EditTarget::General);
        assert_eq!(
            e.text,
            "`store::refresh::boom` fails at src/auth/refresh.rs:400."
        );
        assert_eq!(
            app.world().resource::<ReviewTabs>().0[&pr].ui.section,
            ReviewSection::Tests,
            "the reviewer stays where they were"
        );
    }

    #[test]
    fn a_failure_in_a_file_the_pull_request_does_not_touch_is_a_general_note() {
        let (mut app, pr) = comment_app(
            "crates/store/src/refresh.rs",
            "at crates/store/src/refresh.rs:212:9",
        );
        let comment = testing::find::<CommentOnLine>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, comment);
        let e = editor(&app, &pr).expect("an editor");
        assert_eq!(e.target, EditTarget::General);
        assert_eq!(
            e.text,
            "`store::refresh::boom` fails at crates/store/src/refresh.rs:212."
        );
    }

    #[test]
    fn a_failure_with_no_line_says_test_and_names_the_file() {
        let (mut app, pr) = comment_app(FILE, "no position here");
        assert!(testing::shows(&mut app, "Comment on this test"));
        let comment = testing::find::<CommentOnLine>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, comment);
        let e = editor(&app, &pr).expect("an editor");
        assert_eq!(e.target, EditTarget::General);
        assert_eq!(
            e.text,
            "`store::refresh::boom` fails in src/auth/refresh.rs."
        );
    }

    #[test]
    fn commenting_refuses_an_unsent_editor_and_a_read_only_review() {
        let (mut app, pr) = comment_app(FILE, "at src/auth/refresh.rs:40:9");
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor = Some(crate::review_state::Editor {
            target: EditTarget::General,
            text: "half written".into(),
            error: None,
            ticket: None,
            mode: crate::ui::composer::ComposerMode::Write,
        });
        let comment = testing::find::<CommentOnLine>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, comment);
        assert_eq!(editor(&app, &pr).unwrap().text, "half written", "kept");
        let toasts = &app.world().resource::<crate::bridge::Toasts>().0;
        assert!(
            toasts
                .iter()
                .any(|t| t.text == "Finish or cancel your open comment first")
        );

        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor = None;
        app.world_mut()
            .resource_mut::<crate::bridge::Model>()
            .connection = Connection::Lost("gone".into());
        let comment = testing::find::<CommentOnLine>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, comment);
        assert!(editor(&app, &pr).is_none());
    }

    fn live_app(results: usize) -> (App, PrRef) {
        let (mut app, pr) = tests_app();
        let mut view = testing::tests_view(&app, &pr, TestsPhase::Running { run: "r9".into() });
        view.env = Some(testing::tests_env(true));
        view.recipe = Some(testing::tests_recipe());
        view.plan = Some(testing::tests_plan(Vec::new(), true));
        testing::open_tests(&mut app, &pr, view);
        testing::tell(
            &mut app,
            Tell::Tests(TestsTell::RunStarted {
                pr: pr.clone(),
                run: "r9".into(),
            }),
        );
        for i in 0..results {
            testing::tell(
                &mut app,
                Tell::Tests(TestsTell::Result {
                    pr: pr.clone(),
                    run: "r9".into(),
                    result: passing(&format!("live::t{i:03}")),
                }),
            );
        }
        (app, pr)
    }

    fn log_text(app: &mut App, pr: &PrRef) -> String {
        let e = testing::find::<LiveLog>(app, |l| &l.0 == pr);
        app.world().get::<Text>(e).unwrap().0.clone()
    }

    fn output(app: &mut App, pr: &PrRef, tail: String) {
        testing::tell(
            app,
            Tell::Tests(TestsTell::Output {
                pr: pr.clone(),
                run: "r9".into(),
                tail,
            }),
        );
        // Time passes, as it does while a run goes: the model's flush lets the tail through.
        for _ in 0..20 {
            app.update();
        }
    }

    #[test]
    fn the_live_log_is_replaced_in_place() {
        let (mut app, pr) = live_app(2);
        assert!(testing::shows(&mut app, "Running"));
        let log = testing::find::<LiveLog>(&mut app, |l| l.0 == pr);
        output(&mut app, &pr, "compiling store\n".into());
        assert_eq!(log_text(&mut app, &pr), "compiling store\n");
        output(&mut app, &pr, "compiling store\nrunning 2 tests\n".into());
        assert_eq!(
            log_text(&mut app, &pr),
            "compiling store\nrunning 2 tests\n"
        );
        assert_eq!(
            testing::find::<LiveLog>(&mut app, |l| l.0 == pr),
            log,
            "the same node"
        );
        assert_eq!(testing::count::<LiveLog>(&mut app), 1);
    }

    #[test]
    fn the_live_log_keeps_one_text_and_the_tail() {
        let (mut app, pr) = live_app(1);
        // Ten thousand outputs reach the pump in one frame, as a burst from a chatty test does.
        for i in 0..10_000 {
            let _ = app
                .world()
                .resource::<crate::bridge::Outbox>()
                .0
                .send(Tell::Tests(TestsTell::Output {
                    pr: pr.clone(),
                    run: "r9".into(),
                    tail: format!("line {i}\n"),
                }));
        }
        app.update();
        output(&mut app, &pr, "é".repeat(6_000));
        assert_eq!(
            testing::count::<LiveLog>(&mut app),
            1,
            "one Text, never one per output"
        );
        let text = log_text(&mut app, &pr);
        assert!(text.len() <= TAIL_MAX, "{} bytes", text.len());
        let held = app.world().resource::<TestsModels>().0[&pr]
            .live
            .as_ref()
            .map(|l| l.tail.clone())
            .unwrap_or_default();
        assert_eq!(
            text,
            cap_tail(&clusia_core::printable::printable_lines(&held))
        );
    }

    #[test]
    fn setting_up_shows_the_step_title_and_its_tail() {
        let (mut app, pr) = tests_app();
        let mut view = testing::tests_view(
            &app,
            &pr,
            TestsPhase::Preparing {
                step: "image".into(),
            },
        );
        view.env = Some(testing::tests_env(true));
        view.recipe = Some(testing::tests_recipe());
        view.plan = Some(testing::tests_plan(
            vec![PlanStep {
                id: "image".into(),
                title: "Build the image".into(),
                command: "docker build .".into(),
                place: StepPlace::Container,
                network: true,
                hint: None,
                state: StepState::Running,
                approved: true,
            }],
            true,
        ));
        testing::open_tests(&mut app, &pr, view);
        testing::tell(
            &mut app,
            Tell::Tests(TestsTell::Step {
                pr: pr.clone(),
                step: "image".into(),
                state: StepState::Running,
                tail: Some("Step 3/7 : RUN cargo fetch".into()),
            }),
        );
        assert!(testing::shows(&mut app, "Setting up · Build the image"));
        assert_eq!(log_text(&mut app, &pr), "Step 3/7 : RUN cargo fetch");
    }

    #[test]
    fn listing_shows_a_note() {
        let (mut app, pr) = tests_app();
        let mut view = testing::tests_view(&app, &pr, TestsPhase::Running { run: "list".into() });
        view.env = Some(testing::tests_env(true));
        view.recipe = Some(testing::tests_recipe());
        view.plan = Some(testing::tests_plan(Vec::new(), true));
        testing::open_tests(&mut app, &pr, view);
        assert!(testing::shows(&mut app, "Listing the tests…"));
        assert_eq!(testing::count::<LiveLog>(&mut app), 0);
    }

    #[test]
    fn a_settled_detail_rebuilds_nothing() {
        let (mut app, pr) = tests_app();
        let h = head(&app, &pr);
        let mut view = testing::tests_view(&app, &pr, TestsPhase::Ready);
        view.recipe = Some(testing::tests_recipe());
        view.runs = vec![run_of(&h, vec![refresh_failure()])];
        testing::open_tests(&mut app, &pr, view);
        testing::settle(&mut app);
        let region = testing::find::<DetailRegion>(&mut app, |r| r.0 == pr);
        let kids: Vec<Entity> = app.world().get::<Children>(region).unwrap().to_vec();
        for _ in 0..12 {
            app.update();
        }
        let after: Vec<Entity> = app.world().get::<Children>(region).unwrap().to_vec();
        assert_eq!(kids, after);
    }
}
