//! The driver of a changed function in a dialog, and the comment on a function. The driver's
//! text, its path and the names in the dialog come from the pull request's agent: they are drawn
//! as plain text, and the only path the window opens is the one the function itself names.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, ScrollArea, observe};
use clusia_core::functions::{DRIVER_MAX_BYTES, function_comment_prefill, result_problem};
use clusia_core::printable::{plain_relative, printable, printable_lines};
use clusia_core::{PrRef, Side, can_comment};
use clusia_view::diff::{parse_patch, row_of};

use super::{FunctionsModel, FunctionsModels};
use crate::agent_name;
use crate::bridge::{Ask, Asks, FunctionsAskKind, Model, Toasts};
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen};
use crate::review_state::{EditTarget, Editor, Modal, ReviewTabs, SHOW_STEP};
use crate::screens::review::agent::suggestion::show_file;
use crate::screens::review::diff::unsent;
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::screens::review::shell::ModalFor;
use crate::theme::Swatch;
use crate::ui::composer::ComposerMode;
use crate::ui::kit::{Fill, Type, Variant, button, text, wrapped_text};
use crate::ui::modal::{BackdropClicked, escape_pressed, modal_card, modal_root};

/// The longest path of a driver the window still opens in the editor.
const PATH_MAX: usize = 512;

/// The tallest the driver's text grows before it scrolls.
const TEXT_HEIGHT: f32 = 480.0;

/// Opens the driver of `function`, a function's slug.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ShowDriver {
    pub pr: PrRef,
    pub function: String,
}

/// Opens the composer on `function`, a function's slug.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CommentOnFunction {
    pub pr: PrRef,
    pub function: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CloseDriver(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DriverInEditor(pub PrRef);

/// What the dialog draws, all of it already escaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DriverView {
    pub title: String,
    pub meta: String,
    /// `None` while the driver is on its way.
    pub text: Option<String>,
    pub can_open: bool,
}

/// The view a dialog was drawn from, and the card it was drawn in.
#[derive(Component)]
pub struct DriverBuilt {
    view: DriverView,
    card: Entity,
}

/// The driver's path when it is the one `function` names and is a plain relative path.
fn own_path(model: &FunctionsModel) -> Option<&str> {
    let modal = model.driver.as_ref()?;
    let code = modal.code.as_ref()?;
    let function = model
        .view
        .as_ref()?
        .functions
        .iter()
        .find(|f| f.slug == code.function)?;
    (function.driver.as_deref() == Some(code.path.as_str()) && plain_relative(&code.path, PATH_MAX))
        .then_some(code.path.as_str())
}

/// `{worktree}/{path}` for the driver the dialog shows, when the window may open it.
fn editor_path(model: &FunctionsModel, worktree: Option<&str>) -> Option<String> {
    Some(format!("{}/{}", worktree?, own_path(model)?))
}

/// The end of `text` that still fits a driver, cut on a character boundary.
fn driver_text(text: &str) -> &str {
    let mut end = text.len().min(DRIVER_MAX_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(super) fn driver_view(
    model: &FunctionsModel,
    worktree: Option<&str>,
    agent: &str,
) -> Option<DriverView> {
    let modal = model.driver.as_ref()?;
    let qualified = model
        .view
        .as_ref()
        .and_then(|v| v.functions.iter().find(|f| f.slug == modal.function))
        .map_or("the function", |f| f.qualified.as_str());
    let title = format!("Driver for {}", printable(qualified));
    Some(match &modal.code {
        Some(code) if !modal.loading => DriverView {
            title,
            meta: format!("{} · written by {agent}", printable(&code.path)),
            text: Some(printable_lines(driver_text(&code.text))),
            can_open: editor_path(model, worktree).is_some(),
        },
        _ => DriverView {
            title,
            meta: String::new(),
            text: None,
            can_open: false,
        },
    })
}

/// Opens, redraws and closes the dialog after the model: it is open exactly while the model
/// holds a driver (and no other dialog is up), and a driver that failed to come closes it.
#[allow(clippy::too_many_arguments)]
pub fn fill_driver(
    mut commands: Commands,
    nav: Res<Nav>,
    mut tabs: ResMut<ReviewTabs>,
    mut models: ResMut<FunctionsModels>,
    app_model: Res<Model>,
    fonts: Res<UiFonts>,
    roots: Query<(Entity, &ModalFor, Option<&DriverBuilt>)>,
) {
    // What to change in the model and the tabs is decided first, so a settled dialog writes
    // nothing and wakes nothing.
    let mut dead: Vec<PrRef> = Vec::new();
    let mut close: Vec<PrRef> = Vec::new();
    let mut open: Vec<PrRef> = Vec::new();
    for (pr, tab) in &tabs.0 {
        let driver = models.0.get(pr).and_then(|m| m.driver.as_ref());
        match driver {
            None => {
                if tab.ui.modal == Some(Modal::DriverCode) {
                    close.push(pr.clone());
                }
            }
            Some(modal) if !modal.loading && modal.code.is_none() => dead.push(pr.clone()),
            Some(_) => {
                if tab.ui.modal.is_none() && nav.screen == Screen::Review(pr.clone()) {
                    open.push(pr.clone());
                }
            }
        }
    }
    for pr in dead {
        if let Some(model) = models.0.get_mut(&pr) {
            model.close_driver();
        }
    }
    for pr in close.into_iter() {
        if let Some(tab) = tabs.0.get_mut(&pr) {
            tab.ui.modal = None;
        }
    }
    for pr in open {
        if let Some(tab) = tabs.0.get_mut(&pr) {
            tab.ui.modal = Some(Modal::DriverCode);
        }
    }
    for (pr, tab) in &tabs.0 {
        if tab.ui.modal != Some(Modal::DriverCode) {
            continue;
        }
        let (Some(ready), Some(model)) = (tab.ready(), models.0.get(pr)) else {
            continue;
        };
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let Some(view) = driver_view(model, ready.view.worktree.as_deref(), &agent) else {
            continue;
        };
        let root = roots
            .iter()
            .find(|(_, m, _)| &m.pr == pr && m.modal == Modal::DriverCode);
        match root {
            None => {
                let mut card = Entity::PLACEHOLDER;
                let root = commands
                    .spawn((
                        modal_root(),
                        ModalFor {
                            pr: pr.clone(),
                            modal: Modal::DriverCode,
                        },
                    ))
                    .with_children(|r| {
                        let mut c = r.spawn(modal_card(640.0));
                        card = c.id();
                        c.with_children(|c| draw(c, &fonts, pr, &view));
                    })
                    .id();
                commands.entity(root).insert(DriverBuilt { view, card });
            }
            Some((_, _, Some(built))) if built.view == view => {}
            Some((root, _, Some(built))) => {
                let card = built.card;
                commands.entity(card).despawn_related::<Children>();
                commands
                    .entity(card)
                    .with_children(|c| draw(c, &fonts, pr, &view));
                commands.entity(root).insert(DriverBuilt { view, card });
            }
            // Spawned this frame: its content is queued already.
            Some((_, _, None)) => {}
        }
    }
}

type Spawner<'a> = ChildSpawnerCommands<'a>;

fn draw(card: &mut Spawner, fonts: &UiFonts, pr: &PrRef, view: &DriverView) {
    card.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(4),
        padding: UiRect {
            left: px(24),
            right: px(24),
            top: px(22),
            bottom: px(10),
        },
        ..default()
    })
    .with_children(|head| {
        head.spawn(wrapped_text(
            fonts,
            view.title.clone(),
            Type::HEADING.size(18.0),
        ));
        if !view.meta.is_empty() {
            head.spawn(wrapped_text(fonts, view.meta.clone(), Type::MUTED));
        }
    });
    card.spawn(Node {
        flex_direction: FlexDirection::Column,
        flex_shrink: 1.0,
        min_height: px(0),
        padding: UiRect::axes(px(24), px(4)),
        ..default()
    })
    .with_children(|body| match &view.text {
        None => {
            body.spawn(text(fonts, "Loading…", Type::MUTED));
        }
        Some(code) => {
            body.spawn((
                Node {
                    width: percent(100),
                    max_height: px(TEXT_HEIGHT),
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
            .with_child(wrapped_text(
                fonts,
                code.clone(),
                Type::MONO.ink(Swatch::Fg),
            ));
        }
    });
    card.spawn(Node {
        justify_content: JustifyContent::FlexEnd,
        align_items: AlignItems::Center,
        column_gap: px(10),
        padding: UiRect::axes(px(24), px(16)),
        ..default()
    })
    .with_children(|foot| {
        if view.can_open {
            foot.spawn((
                button(fonts, "Open in editor", Variant::Secondary),
                DriverInEditor(pr.clone()),
                observe(on_editor),
            ));
        }
        foot.spawn((
            button(fonts, "Close", Variant::Primary),
            CloseDriver(pr.clone()),
            observe(on_close),
        ));
    });
}

/// Whether any driver dialog is up.
pub fn driver_up(roots: Query<&ModalFor>) -> bool {
    roots.iter().any(|m| m.modal == Modal::DriverCode)
}

fn close(pr: &PrRef, tabs: &mut ReviewTabs, models: &mut FunctionsModels) {
    if let Some(model) = models.0.get_mut(pr) {
        model.close_driver();
    }
    if let Some(tab) = tabs.0.get_mut(pr)
        && tab.ui.modal == Some(Modal::DriverCode)
    {
        tab.ui.modal = None;
    }
}

/// Esc and a click outside the card close the dialog.
pub fn cancel_driver(
    keys: Res<ButtonInput<KeyCode>>,
    mut clicks: MessageReader<BackdropClicked>,
    roots: Query<&ModalFor>,
    nav: Res<Nav>,
    mut tabs: ResMut<ReviewTabs>,
    mut models: ResMut<FunctionsModels>,
) {
    let outside = clicks
        .read()
        .any(|c| roots.get(c.0).is_ok_and(|m| m.modal == Modal::DriverCode));
    if !(escape_pressed(&keys) || outside) {
        return;
    }
    if let Screen::Review(pr) = &nav.screen {
        close(pr, &mut tabs, &mut models);
    }
}

fn on_close(
    activate: On<Activate>,
    buttons: Query<&CloseDriver>,
    mut tabs: ResMut<ReviewTabs>,
    mut models: ResMut<FunctionsModels>,
) {
    if let Ok(CloseDriver(pr)) = buttons.get(activate.entity) {
        close(pr, &mut tabs, &mut models);
    }
}

/// Asks for the driver and opens the dialog waiting for it.
pub(super) fn on_show(
    activate: On<Activate>,
    buttons: Query<&ShowDriver>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<FunctionsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(ShowDriver { pr, function }) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &app_model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    model.sent(FunctionsAskKind::Driver);
    let ask = model.open_driver(pr, function);
    asks.send(ask);
}

/// Opens the driver the dialog shows, at the path the function itself names.
fn on_editor(
    activate: On<Activate>,
    buttons: Query<&DriverInEditor>,
    tabs: Res<ReviewTabs>,
    models: Res<FunctionsModels>,
    mut asks: ResMut<Asks>,
) {
    let Ok(DriverInEditor(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let worktree = tabs
        .0
        .get(pr)
        .and_then(|t| t.ready())
        .and_then(|r| r.view.worktree.as_deref());
    if let Some(path) = models
        .0
        .get(pr)
        .and_then(|model| editor_path(model, worktree))
    {
        asks.send(Ask::OpenInEditor { path, line: None });
    }
}

/// Opens the composer on the function's changed lines when the pull request's diff has them, else
/// as a general note naming the file and line. The model is read at the click.
pub(super) fn on_comment(
    activate: On<Activate>,
    buttons: Query<&CommentOnFunction>,
    app_model: Res<Model>,
    models: Res<FunctionsModels>,
    mut tabs: ResMut<ReviewTabs>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(CommentOnFunction { pr, function }) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &app_model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    let Some(model) = models.0.get(pr) else {
        return;
    };
    let Some(f) = model
        .view
        .as_ref()
        .and_then(|v| v.functions.iter().find(|f| f.slug == *function))
    else {
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
    let open = if model.selected.as_deref() == Some(function.as_str()) {
        model.open_view()
    } else {
        None
    };
    let failing = open.and_then(|case| {
        let result = model.current_result(case)?;
        let problem = result_problem(result)?;
        Some((case.name.clone(), problem))
    });
    let prefill = function_comment_prefill(
        &f.qualified,
        failing
            .as_ref()
            .map(|(case, problem)| (case.as_str(), problem.as_str())),
    );
    let patch = f.diff_lines.and_then(|lines| {
        let patch = tab
            .ready()?
            .view
            .diff
            .iter()
            .find(|d| d.path == f.file)?
            .patch
            .clone()?;
        can_comment(&patch, Side::Right, lines.1).then_some((lines, patch))
    });
    let (target, text) = match patch {
        Some(((start, end), patch)) => {
            show_file(&mut tab.ui, &f.file);
            // The Diff draws its rows in steps; the composer sits under the row of `end`, so
            // that row (and the ones above it) must be among the drawn ones.
            if let Some(row) = row_of(&parse_patch(&patch), Side::Right, end) {
                tab.ui.shown = tab.ui.shown.max((row + 1).div_ceil(SHOW_STEP) * SHOW_STEP);
            }
            (
                EditTarget::Line {
                    path: f.file.clone(),
                    side: Side::Right,
                    start: (start != end).then_some(start),
                    line: end,
                },
                prefill,
            )
        }
        None => (
            EditTarget::General,
            format!("{}:{}: {prefill}", printable(&f.file), f.start_line),
        ),
    };
    tab.ui.editor = Some(Editor {
        target,
        text,
        error: None,
        ticket: None,
        mode: ComposerMode::Write,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{FunctionsTell, Tell};
    use crate::fixture::{self, DemoFunctions};
    use crate::review_state::{EditTarget, Modal};
    use crate::testing::{self, NOW};
    use clusia_core::functions::DriverCode;

    fn app_with(view: clusia_core::functions::FunctionsView) -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, crate::review_state::ReviewSection::Tests);
        testing::open_functions(&mut app, &pr, view);
        app.world_mut()
            .resource_mut::<crate::bridge::Model>()
            .snapshot
            .clone_from(&fixture::demo(NOW));
        testing::recorded(&mut app);
        (app, pr)
    }

    fn mockup() -> clusia_core::functions::FunctionsView {
        fixture::demo_functions(NOW, DemoFunctions::Mockup)
    }

    fn open_driver(app: &mut App, pr: &PrRef) -> String {
        let button = testing::find::<ShowDriver>(app, |s| &s.pr == pr);
        testing::activate(app, button);
        let asks = testing::recorded(app);
        let [crate::bridge::Ask::GetDriver { function, .. }] = asks.as_slice() else {
            panic!("one GetDriver, got {asks:?}");
        };
        function.clone()
    }

    fn deliver(app: &mut App, pr: &PrRef, function: &str, path: &str, text: &str) {
        testing::tell(
            app,
            Tell::Functions(FunctionsTell::Driver {
                pr: pr.clone(),
                code: Box::new(DriverCode {
                    function: function.into(),
                    path: path.into(),
                    text: text.into(),
                    hash: "h".into(),
                }),
            }),
        );
    }

    #[test]
    fn show_driver_code_asks_then_opens_the_modal_with_the_text() {
        let (mut app, pr) = app_with(mockup());
        let function = open_driver(&mut app, &pr);
        assert_eq!(testing::tab(&app, &pr).ui.modal, Some(Modal::DriverCode));
        assert!(testing::shows(&mut app, "Loading…"));
        let path = format!(".clusia/functions/{function}/driver.rs");
        deliver(
            &mut app,
            &pr,
            &function,
            &path,
            "#[test]\nfn clusia_case() {}\n",
        );
        for line in [
            "Driver for TokenStore::refresh",
            &format!("{path} · written by Claude Code"),
            "fn clusia_case() {}",
            "Open in editor",
            "Close",
        ] {
            assert!(testing::shows(&mut app, line), "missing {line:?}");
        }
    }

    #[test]
    fn close_esc_and_the_backdrop_all_close_it() {
        for how in 0..3 {
            let (mut app, pr) = app_with(mockup());
            let function = open_driver(&mut app, &pr);
            deliver(
                &mut app,
                &pr,
                &function,
                ".clusia/functions/x/driver.rs",
                "a",
            );
            match how {
                0 => {
                    let close = testing::find::<CloseDriver>(&mut app, |c| c.0 == pr);
                    testing::activate(&mut app, close);
                }
                1 => testing::press_escape(&mut app),
                _ => testing::click_backdrop(&mut app),
            }
            assert_eq!(testing::tab(&app, &pr).ui.modal, None, "way {how}");
            assert!(
                app.world().resource::<FunctionsModels>().0[&pr]
                    .driver
                    .is_none()
            );
        }
    }

    #[test]
    fn open_in_editor_names_the_worktree_path_only_for_a_plain_functions_path() {
        let (mut app, pr) = app_with(mockup());
        let function = open_driver(&mut app, &pr);
        let own = app.world().resource::<FunctionsModels>().0[&pr]
            .view
            .as_ref()
            .unwrap()
            .functions[0]
            .driver
            .clone()
            .expect("the fixture's function has a driver");
        deliver(&mut app, &pr, &function, &own, "a");
        let open = testing::find::<DriverInEditor>(&mut app, |d| d.0 == pr);
        testing::activate(&mut app, open);
        let asks = testing::recorded(&mut app);
        assert!(matches!(
            asks.as_slice(),
            [crate::bridge::Ask::OpenInEditor { path, line: None }] if path.ends_with(&format!("/{own}"))
        ));
        for bad in [
            "../../etc/passwd",
            "/etc/passwd",
            "src/lib.rs",
            ".clusia/functions/a\\b/driver.rs",
            ".clusia/functions/other_fn-0123456789/driver.rs",
        ] {
            let (mut app, pr) = app_with(mockup());
            let function = open_driver(&mut app, &pr);
            deliver(&mut app, &pr, &function, bad, "a");
            assert_eq!(testing::count::<DriverInEditor>(&mut app), 0, "{bad}");
        }
    }

    #[test]
    fn a_function_with_no_driver_has_no_link() {
        let mut view = mockup();
        view.functions[0].driver = None;
        let (mut app, _pr) = app_with(view);
        assert!(!testing::shows(&mut app, "Show driver code"));
    }

    #[test]
    fn the_driver_text_is_plain() {
        let (mut app, pr) = app_with(mockup());
        let function = open_driver(&mut app, &pr);
        deliver(
            &mut app,
            &pr,
            &function,
            ".clusia/functions/x/driver.rs",
            "// [x](https://example.test) **b**\u{202e}\u{1b}[31m\nfn a() {}",
        );
        assert!(testing::shows(
            &mut app,
            "// [x](https://example.test) **b**"
        ));
        assert_eq!(testing::count::<crate::ui::markdown::MdBody>(&mut app), 0);
        assert_eq!(testing::count::<crate::ui::markdown::MdLink>(&mut app), 0);
    }

    #[test]
    fn a_refused_driver_closes_the_modal() {
        let (mut app, pr) = app_with(mockup());
        open_driver(&mut app, &pr);
        testing::tell(
            &mut app,
            Tell::Functions(FunctionsTell::Refused {
                pr: pr.clone(),
                ask: crate::bridge::FunctionsAskKind::Driver,
                message: "No driver".into(),
            }),
        );
        assert_eq!(testing::tab(&app, &pr).ui.modal, None);
    }

    fn comment(app: &mut App, pr: &PrRef) -> crate::review_state::Editor {
        let button = testing::find::<CommentOnFunction>(app, |c| &c.pr == pr);
        testing::activate(app, button);
        testing::tab(app, pr).ui.editor.expect("an editor")
    }

    #[test]
    fn a_function_on_changed_lines_comments_on_them_with_the_failing_case() {
        let mut view = mockup();
        view.functions[0].diff_lines = Some((40, 52));
        let (mut app, pr) = app_with(view);
        testing::allow_comments_on(&mut app, &pr, "store/refresh.rs", 52);
        let editor = comment(&mut app, &pr);
        assert_eq!(
            editor.target,
            EditTarget::Line {
                path: "store/refresh.rs".into(),
                side: clusia_core::Side::Right,
                start: Some(40),
                line: 52
            }
        );
        assert_eq!(
            editor.text,
            "`TokenStore::refresh` fails the case \"Endpoint fails, old token kept\": token is \"\", expected \"old\"."
        );
    }

    #[test]
    fn a_function_off_the_changed_lines_comments_in_general_with_its_file_and_line() {
        let mut view = mockup();
        view.functions[0].diff_lines = None;
        view.functions[0].start_line = 41;
        let (mut app, pr) = app_with(view);
        let editor = comment(&mut app, &pr);
        assert_eq!(editor.target, EditTarget::General);
        assert!(
            editor
                .text
                .starts_with("store/refresh.rs:41: `TokenStore::refresh` fails the case"),
            "{}",
            editor.text
        );
    }

    #[test]
    fn a_passing_function_gets_a_bare_prefix() {
        let mut view = mockup();
        view.functions[0].diff_lines = None;
        let function = &mut view.functions[2];
        function.diff_lines = None;
        let slug = function.slug.clone();
        let (mut app, pr) = app_with(view);
        let models = &mut app.world_mut().resource_mut::<FunctionsModels>();
        models.0.get_mut(&pr).unwrap().select(&slug);
        app.update();
        let editor = comment(&mut app, &pr);
        assert!(
            editor.text.ends_with("`backoff_delay`: "),
            "{}",
            editor.text
        );
    }

    #[test]
    fn commenting_refuses_an_unsent_editor_and_a_read_only_review() {
        let (mut app, pr) = app_with(mockup());
        let _ = comment(&mut app, &pr);
        let again = testing::find::<CommentOnFunction>(&mut app, |c| c.pr == pr);
        testing::activate(&mut app, again);
        assert!(testing::shows(
            &mut app,
            "Finish or cancel your open comment first"
        ));
        let (mut app, pr) = app_with(mockup());
        testing::go_offline(&mut app);
        let button = testing::find::<CommentOnFunction>(&mut app, |c| c.pr == pr);
        testing::activate(&mut app, button);
        assert!(testing::tab(&app, &pr).ui.editor.is_none());
    }
}
