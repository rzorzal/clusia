//! The list of changed functions: one row per function with its cases and last results, the
//! files no pack reads as greyed rows, and **Run all cases** or **Stop** pinned at the bottom.
//! Every name and path is drawn through `printable` as plain text.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::functions::{CaseScope, FunctionChange, FunctionsActivity, NO_CASES, Support};
use clusia_core::printable::{printable, printable_lines};

use super::{FunctionsModel, FunctionsModels, disabled_reason};
use crate::bridge::{Ask, Asks, FunctionsAskKind, Model};
use crate::fonts::UiFonts;
use crate::review_state::ReviewTabs;
use crate::screens::review::tests_tab::{TestsModel, TestsModels};
use crate::theme::Swatch;
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Type, Variant, button, card, disabled_button, divider, text,
    wrapped_text,
};

/// The card of changed functions.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FunctionsList(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FunctionRow {
    pub pr: PrRef,
    pub function: String,
}

/// A file of the pull request that no pack reads. It is drawn and nothing more.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedFile {
    pub pr: PrRef,
    pub path: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RunAllCases(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct StopCases(pub PrRef);

/// The `ListView` the region it sits on was drawn from.
#[derive(Component)]
pub struct ListBuilt(ListView);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Status {
    Counts { passed: u32, failed: u32 },
    NotRun,
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RowView {
    pub slug: String,
    pub name: String,
    pub meta: String,
    pub status: Status,
    pub selected: bool,
    pub greyed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Footer {
    RunAll { n: u32 },
    Off { reason: &'static str, n: u32 },
    Stop,
}

/// Everything the list draws, as text: two lists that draw the same are equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ListView {
    pub head: String,
    pub rows: Vec<RowView>,
    /// `(title, meta)` of each unsupported file, with its path.
    pub files: Vec<(String, String, String)>,
    pub note: Option<String>,
    pub failed: Option<String>,
    pub empty: Option<&'static str>,
    pub footer: Footer,
}

const SECTION: Type = Type {
    size: 11.0,
    weight: 600,
    ink: Swatch::Muted,
    mono: false,
};

fn change_word(change: FunctionChange) -> &'static str {
    match change {
        FunctionChange::Changed => "changed",
        FunctionChange::New => "new",
        FunctionChange::Signature => "signature",
        FunctionChange::Moved => "moved",
    }
}

fn cases_word(n: usize) -> String {
    if n == 1 {
        "1 case".into()
    } else {
        format!("{n} cases")
    }
}

/// What the list draws for `model`; `None` until the model has a view. `reason` is why the run
/// button is off, when it is.
pub(super) fn list_view(model: &FunctionsModel, reason: Option<&'static str>) -> Option<ListView> {
    let view = model.view.as_ref()?;
    let selected = model.selected_view().map(|f| f.slug.as_str());
    let running = match &view.activity {
        Some(FunctionsActivity::Running {
            current: Some(c), ..
        }) => Some(c.function.as_str()),
        _ => None,
    };
    let rows = view
        .functions
        .iter()
        .map(|f| {
            let file = printable(&f.file);
            let meta = match &f.support {
                Support::Unsupported { .. } => format!("{file} · not supported yet"),
                Support::NoCases | Support::Stale { .. } | Support::NoCommand
                    if f.cases.is_empty() =>
                {
                    format!("{file} · no cases yet")
                }
                _ => format!(
                    "{file} · {} · {}",
                    change_word(f.change),
                    cases_word(f.cases.len())
                ),
            };
            let (passed, failed) = model.counts(f);
            let status = if running == Some(f.slug.as_str()) {
                Status::Running
            } else if passed + failed == 0 {
                Status::NotRun
            } else {
                Status::Counts { passed, failed }
            };
            RowView {
                slug: f.slug.clone(),
                name: printable(&f.qualified),
                meta,
                status,
                selected: selected == Some(f.slug.as_str()),
                greyed: matches!(f.support, Support::Unsupported { .. }),
            }
        })
        .collect();
    let files = view
        .unsupported
        .iter()
        .map(|path| {
            let name = path.rsplit('/').next().unwrap_or(path);
            (
                printable(name),
                format!("{} · not supported yet", printable(path)),
                path.clone(),
            )
        })
        .collect();
    let n = model.runnable_total();
    let footer = if matches!(view.activity, Some(FunctionsActivity::Running { .. })) {
        Footer::Stop
    } else if let Some(reason) = reason {
        Footer::Off { reason, n }
    } else if n == 0 {
        Footer::Off {
            reason: NO_CASES,
            n,
        }
    } else {
        Footer::RunAll { n }
    };
    let empty = if view.reading {
        Some("Reading the changed functions…")
    } else if view.functions.is_empty() && view.unsupported.is_empty() {
        Some("No changed functions in this pull request.")
    } else {
        None
    };
    Some(ListView {
        head: format!(
            "CHANGED FUNCTIONS · {}",
            view.functions.len() + view.unsupported.len()
        ),
        rows,
        files,
        note: view.note.as_deref().map(printable_lines),
        failed: view.failed.as_deref().map(printable_lines),
        empty,
        footer,
    })
}

pub fn fill_list(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    tests: Res<TestsModels>,
    functions: Res<FunctionsModels>,
    app_model: Res<Model>,
    fonts: Res<UiFonts>,
    regions: Query<(Entity, &FunctionsList, Option<&ListBuilt>)>,
) {
    for (entity, region, built) in &regions {
        let pr = &region.0;
        if tabs.0.get(pr).and_then(|t| t.ready()).is_none() {
            continue;
        }
        let Some(model) = functions.0.get(pr) else {
            continue;
        };
        let reason = disabled_reason(&tabs, &app_model, tests.0.get(pr), model, pr, true);
        let Some(view) = list_view(model, reason) else {
            continue;
        };
        if built.is_some_and(|b| b.0 == view) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| draw(p, &fonts, pr, &view));
        commands.entity(entity).insert(ListBuilt(view));
    }
}

type Spawner<'a> = ChildSpawnerCommands<'a>;

fn draw(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, view: &ListView) {
    p.spawn(card(Node {
        flex_direction: FlexDirection::Column,
        flex_grow: 1.0,
        min_height: px(0),
        ..default()
    }))
    .with_children(|c| {
        c.spawn(Node {
            width: percent(100),
            flex_shrink: 0.0,
            padding: UiRect::axes(px(14), px(10)),
            ..default()
        })
        .with_child(text(fonts, view.head.clone(), SECTION));
        c.spawn(divider());
        c.spawn((
            Node {
                width: percent(100),
                flex_grow: 1.0,
                min_height: px(0),
                flex_direction: FlexDirection::Column,
                overflow: Overflow::scroll_y(),
                ..default()
            },
            ScrollArea,
            ScrollPosition::default(),
        ))
        .with_children(|body| {
            for line in [&view.note, &view.failed].into_iter().flatten() {
                body.spawn(Node {
                    width: percent(100),
                    padding: UiRect::axes(px(14), px(8)),
                    ..default()
                })
                .with_child(wrapped_text(fonts, line.clone(), Type::MUTED));
            }
            if let Some(empty) = view.empty {
                body.spawn(Node {
                    width: percent(100),
                    padding: UiRect::axes(px(14), px(12)),
                    ..default()
                })
                .with_child(wrapped_text(fonts, empty, Type::MUTED));
            }
            for row in &view.rows {
                draw_row(body, fonts, pr, row);
            }
            for (title, meta, path) in &view.files {
                draw_file(body, fonts, pr, title, meta, path);
            }
        });
        c.spawn(divider());
        c.spawn(Node {
            width: percent(100),
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Stretch,
            row_gap: px(6),
            padding: UiRect::all(px(12)),
            ..default()
        })
        .with_children(|foot| match &view.footer {
            Footer::RunAll { n } => {
                foot.spawn((
                    button(fonts, &format!("Run all cases · {n}"), Variant::Secondary),
                    RunAllCases(pr.clone()),
                    observe(on_run_all),
                ))
                .entry::<Node>()
                .and_modify(wide);
            }
            Footer::Off { reason, n } => {
                foot.spawn(disabled_button(fonts, &format!("Run all cases · {n}")))
                    .entry::<Node>()
                    .and_modify(wide);
                foot.spawn(wrapped_text(fonts, *reason, Type::META));
            }
            Footer::Stop => {
                foot.spawn((
                    button(fonts, "Stop", Variant::Secondary),
                    StopCases(pr.clone()),
                    observe(on_stop),
                ))
                .entry::<Node>()
                .and_modify(wide);
            }
        });
    });
}

/// A button's own `align_self` keeps it content-wide; the footer's buttons take the row.
fn wide(mut node: Mut<Node>) {
    node.align_self = AlignSelf::Stretch;
}

fn draw_row(body: &mut Spawner, fonts: &UiFonts, pr: &PrRef, row: &RowView) {
    let (name_type, meta_type) = if row.greyed {
        (Type::MONO.ink(Swatch::Faint), Type::META)
    } else {
        (
            Type {
                weight: if row.selected { 600 } else { 400 },
                ..Type::MONO.ink(Swatch::Fg)
            },
            Type::META,
        )
    };
    body.spawn((
        Node {
            width: percent(100),
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            padding: UiRect::axes(px(14), px(9)),
            border: UiRect::bottom(px(1)),
            ..default()
        },
        (WidgetButton, Clickable),
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(if row.selected {
            Swatch::GreenTint
        } else {
            Swatch::Clear
        }),
        HoverFill(Swatch::Hover),
        BorderColor::default(),
        crate::ui::kit::Stroke(Swatch::Line),
        FunctionRow {
            pr: pr.clone(),
            function: row.slug.clone(),
        },
        observe(on_row),
    ))
    .with_children(|r| {
        r.spawn(text(fonts, row.name.clone(), name_type));
        r.spawn(text(fonts, row.meta.clone(), meta_type));
        r.spawn(Node {
            column_gap: px(4),
            ..default()
        })
        .with_children(|s| status_line(s, fonts, &row.status));
    });
}

fn status_line(s: &mut Spawner, fonts: &UiFonts, status: &Status) {
    match status {
        Status::NotRun => {
            s.spawn(text(fonts, "not run", Type::META));
        }
        Status::Running => {
            s.spawn(text(fonts, "running…", Type::META.ink(Swatch::Muted)));
        }
        Status::Counts { passed, failed } => {
            if *passed > 0 {
                s.spawn(text(
                    fonts,
                    format!("{passed} passed"),
                    Type::META.ink(Swatch::Green),
                ));
            }
            if *passed > 0 && *failed > 0 {
                s.spawn(text(fonts, "·", Type::META));
            }
            if *failed > 0 {
                s.spawn(text(
                    fonts,
                    format!("{failed} failed"),
                    Type::META.ink(Swatch::Orange),
                ));
            }
        }
    }
}

fn draw_file(body: &mut Spawner, fonts: &UiFonts, pr: &PrRef, title: &str, meta: &str, path: &str) {
    body.spawn((
        Node {
            width: percent(100),
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            padding: UiRect::axes(px(14), px(9)),
            border: UiRect::bottom(px(1)),
            ..default()
        },
        BorderColor::default(),
        crate::ui::kit::Stroke(Swatch::Line),
        UnsupportedFile {
            pr: pr.clone(),
            path: path.to_string(),
        },
    ))
    .with_children(|r| {
        r.spawn(text(
            fonts,
            title.to_string(),
            Type::MONO.ink(Swatch::Faint),
        ));
        r.spawn(text(fonts, meta.to_string(), Type::META));
    });
}

fn on_row(activate: On<Activate>, rows: Query<&FunctionRow>, mut models: ResMut<FunctionsModels>) {
    let Ok(FunctionRow { pr, function }) = rows.get(activate.entity) else {
        return;
    };
    if let Some(model) = models.0.get_mut(pr) {
        model.select(function);
    }
}

fn on_run_all(
    activate: On<Activate>,
    buttons: Query<&RunAllCases>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    tests: Res<TestsModels>,
    mut models: ResMut<FunctionsModels>,
    mut toasts: ResMut<crate::bridge::Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(RunAllCases(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    let tests: Option<&TestsModel> = tests.0.get(pr);
    if let Some(reason) = disabled_reason(&tabs, &app_model, tests, model, pr, true) {
        return crate::screens::review::editor::not_sent(&mut toasts, &time, reason);
    }
    let Some(seen) = model.view.as_ref().map(|v| v.hash.clone()) else {
        return;
    };
    asks.send(Ask::RunCases {
        pr: pr.clone(),
        scope: CaseScope::All,
        seen,
    });
    model.sent(FunctionsAskKind::Run);
}

fn on_stop(activate: On<Activate>, buttons: Query<&StopCases>, mut asks: ResMut<Asks>) {
    let Ok(StopCases(pr)) = buttons.get(activate.entity) else {
        return;
    };
    asks.send(Ask::StopTests(pr.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, DemoFunctions};
    use crate::screens::review::tests_tab::functions;
    use crate::testing::{self, NOW};
    use clusia_core::functions::{CaseScope, FunctionsActivity};
    use clusia_core::tests::TestsPhase;

    fn app_with(view: clusia_core::functions::FunctionsView) -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, crate::review_state::ReviewSection::Tests);
        testing::open_functions(&mut app, &pr, view);
        testing::recorded(&mut app);
        (app, pr)
    }

    fn mockup() -> clusia_core::functions::FunctionsView {
        fixture::demo_functions(NOW, DemoFunctions::Mockup)
    }

    #[test]
    fn the_list_is_the_mockups() {
        let (mut app, _pr) = app_with(mockup());
        for line in [
            "CHANGED FUNCTIONS · 5",
            "TokenStore::refresh",
            "store/refresh.rs · changed · 4 cases",
            "3 passed",
            "1 failed",
            "TokenStore::exchange",
            "store/refresh.rs · new · 3 cases",
            "not run",
            "backoff_delay",
            "http/retry.rs · signature · 5 cases",
            "5 passed",
            "Config::token_path",
            "config.rs · changed · 2 cases",
            "badge.swift",
            "ui/badge.swift · not supported yet",
            "Run all cases · 14",
        ] {
            assert!(testing::shows(&mut app, line), "missing {line:?}");
        }
    }

    #[test]
    fn the_first_ready_function_is_selected_and_a_click_selects_another() {
        let (mut app, pr) = app_with(mockup());
        let model = &app.world().resource::<FunctionsModels>().0[&pr];
        let slug = model.selected_view().expect("a selection").slug.clone();
        assert!(
            model.selected_view().unwrap().qualified == "TokenStore::refresh",
            "the first ready function"
        );
        let other = app.world().resource::<FunctionsModels>().0[&pr]
            .view
            .as_ref()
            .unwrap()
            .functions[2]
            .slug
            .clone();
        assert_ne!(slug, other);
        let row = testing::find::<FunctionRow>(&mut app, |r| r.function == other);
        testing::activate(&mut app, row);
        let model = &app.world().resource::<FunctionsModels>().0[&pr];
        assert_eq!(model.selected.as_deref(), Some(other.as_str()));
        assert_eq!(
            model.open_case.as_deref(),
            Some("a1"),
            "the open case is the selected function's default"
        );
    }

    #[test]
    fn an_unsupported_file_is_a_greyed_row_that_cannot_be_selected() {
        let (mut app, pr) = app_with(mockup());
        let file = testing::find::<UnsupportedFile>(&mut app, |f| f.path == "ui/badge.swift");
        testing::activate(&mut app, file);
        let model = &app.world().resource::<FunctionsModels>().0[&pr];
        assert_eq!(
            model.selected_view().unwrap().qualified,
            "TokenStore::refresh"
        );
    }

    #[test]
    fn run_all_cases_sends_the_hash_of_what_the_list_drew() {
        let (mut app, pr) = app_with(mockup());
        let seen = app.world().resource::<FunctionsModels>().0[&pr]
            .view
            .as_ref()
            .unwrap()
            .hash
            .clone();
        let run = testing::find::<RunAllCases>(&mut app, |r| r.0 == pr);
        testing::activate(&mut app, run);
        assert_eq!(
            testing::recorded(&mut app),
            vec![Ask::RunCases {
                pr: pr.clone(),
                scope: CaseScope::All,
                seen
            }]
        );
    }

    #[test]
    fn run_all_cases_is_disabled_while_the_auto_tests_run_and_says_why() {
        let (mut app, pr) = app_with(mockup());
        let mut tests = testing::tests_view(
            &app,
            &pr,
            TestsPhase::Running {
                run: "run-4".into(),
            },
        );
        tests.plan = Some(testing::tests_plan(Vec::new(), true));
        tests.recipe = Some(testing::tests_recipe());
        testing::open_tests(&mut app, &pr, tests);
        assert!(testing::shows(&mut app, "The tests are busy"));
        assert_eq!(testing::count::<RunAllCases>(&mut app), 0);
    }

    #[test]
    fn a_running_activity_turns_run_all_into_stop_and_marks_the_running_function() {
        let mut view = mockup();
        let slug = view.functions[0].slug.clone();
        let case = view.functions[0].cases[0].id.clone();
        view.activity = Some(FunctionsActivity::Running {
            run: "r1".into(),
            scope: CaseScope::All,
            total: 14,
            done: 2,
            current: Some(clusia_core::functions::CaseRef {
                function: slug,
                case,
            }),
        });
        let (mut app, pr) = app_with(view);
        assert!(testing::shows(&mut app, "running…"));
        let stop = testing::find::<StopCases>(&mut app, |s| s.0 == pr);
        testing::activate(&mut app, stop);
        assert_eq!(
            testing::recorded(&mut app),
            vec![Ask::StopTests(pr.clone())]
        );
    }

    #[test]
    fn the_empty_states_read_plainly() {
        let mut view = clusia_core::functions::FunctionsView::new("abc");
        view.reading = true;
        let (mut app, _pr) = app_with(view);
        assert!(testing::shows(&mut app, "Reading the changed functions…"));
        let (mut app, _pr) = app_with(clusia_core::functions::FunctionsView::new("abc"));
        assert!(testing::shows(
            &mut app,
            "No changed functions in this pull request."
        ));
    }

    #[test]
    fn ten_function_edits_rebuild_no_other_region() {
        let (mut app, pr) = app_with(mockup());
        let list = testing::find::<FunctionsList>(&mut app, |l| l.0 == pr);
        let before = testing::tree_signature(&app, list);
        let result = testing::find::<functions::FunctionsResult>(&mut app, |r| r.0 == pr);
        let result_before = testing::tree_signature(&app, result);
        for n in 0..10 {
            let mut view = app.world().resource::<FunctionsModels>().0[&pr]
                .view
                .clone()
                .unwrap();
            view.functions[0].cases[1]
                .values
                .insert("endpoint_status".into(), format!("50{n}"));
            testing::tell(
                &mut app,
                crate::bridge::Tell::Functions(crate::bridge::FunctionsTell::Loaded {
                    pr: pr.clone(),
                    view: Box::new(view),
                }),
            );
        }
        assert_eq!(
            testing::tree_signature(&app, list),
            before,
            "the list is the same entities"
        );
        assert_eq!(testing::tree_signature(&app, result), result_before);
    }

    #[test]
    fn run_all_cases_is_a_full_width_secondary_button() {
        let (mut app, pr) = app_with(mockup());
        let run = testing::find::<RunAllCases>(&mut app, |r| r.0 == pr);
        let node = app.world().get::<Node>(run).unwrap();
        assert_eq!(node.align_self, AlignSelf::Stretch);
        let secondary = crate::ui::kit::Fill(Swatch::Surface);
        assert_eq!(
            app.world().get::<crate::ui::kit::Fill>(run),
            Some(&secondary)
        );
    }
}
