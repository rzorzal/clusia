//! The cockpit's list: scenarios and benchmarks as rows, the writing states, and the bottom
//! block with **Run on main too** and **Run all**. Titles and statuses arrive printable and
//! bounded in the view, and are drawn as plain text.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::cockpit::{CockpitScope, ItemRef, MainPhase, ScenarioAsk};

use super::frame::{CockpitList, failed_line};
use super::views::{ListView, Row, Tone, list_view};
use super::{CockpitModal, CockpitModel, CockpitModels};
use crate::agent_name;
use crate::bridge::{Ask, Asks, CockpitAskKind, Model, Toasts};
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::review_state::ReviewTabs;
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::screens::review::tests_tab::approvable_ids;
use crate::theme::Swatch;
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Stroke, Type, Variant, button, checkbox, disabled_button, divider,
    text, wrapped_text,
};
use crate::ui::spinner::spinner;

type Spawner<'a> = ChildSpawnerCommands<'a>;

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitRunAll(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitOnMain(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitAskScenario(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitRow {
    pub pr: PrRef,
    pub item: ItemRef,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitWriteScenarios(pub PrRef);

/// The `ListView` the list region was drawn from.
#[derive(Component)]
pub struct ListBuilt(ListView);

/// The ink of a status line.
pub(super) fn tone_swatch(tone: Tone) -> Swatch {
    match tone {
        Tone::Running => Swatch::Green,
        Tone::Worse => Swatch::Orange,
        Tone::Warn => Swatch::Amber,
        Tone::Faint => Swatch::Faint,
    }
}

/// Opens main's setup plan with the steps the plan modal checks on its own.
pub fn open_prepare_main(model: &mut CockpitModel) {
    let checked = model
        .view
        .as_ref()
        .and_then(|v| v.main.plan.as_ref())
        .map(approvable_ids)
        .unwrap_or_default();
    model.modal = Some(CockpitModal::PrepareMain { checked });
    model.touch();
}

/// Asks the copilot's input to take the focus.
pub fn focus_copilot_input(model: &mut CockpitModel) {
    model.copilot_focus = true;
    model.touch();
}

pub fn fill_list(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    clock: Res<Clock>,
    models: Res<CockpitModels>,
    fonts: Res<UiFonts>,
    regions: Query<(Entity, &CockpitList, Option<&ListBuilt>)>,
) {
    let now = clock.now();
    for (entity, region, built) in &regions {
        let pr = &region.0;
        let Some(tab) = tabs.0.get(pr).filter(|t| t.ready().is_some()) else {
            continue;
        };
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let default = CockpitModel::default();
        let model = models.0.get(pr).unwrap_or(&default);
        let view = list_view(model, &agent, now);
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

const GROUP: Type = Type {
    size: 11.0,
    weight: 600,
    ink: Swatch::Muted,
    mono: false,
};

fn draw(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, view: &ListView) {
    p.spawn((
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
        if let Some(failed) = &view.failed {
            failed_line(body, fonts, pr, failed);
        }
        let scenarios: Vec<&Row> = view.rows.iter().filter(|r| !r.mono).collect();
        let benches: Vec<&Row> = view.rows.iter().filter(|r| r.mono).collect();
        group_head(body, fonts, &format!("SCENARIOS · {}", scenarios.len()));
        if let Some(writing) = &view.scenarios_writing {
            body.spawn(Node {
                width: percent(100),
                align_items: AlignItems::Center,
                column_gap: px(8),
                padding: UiRect::axes(px(14), px(6)),
                ..default()
            })
            .with_children(|line| {
                line.spawn(spinner());
                line.spawn(text(fonts, writing.clone(), Type::MUTED));
            });
        }
        if scenarios.is_empty() {
            empty_line(
                body,
                fonts,
                format!(
                    "No scenarios yet. Ask {} to write them from the diff.",
                    view.agent
                ),
            );
        }
        for row in scenarios {
            draw_row(body, fonts, pr, row);
        }
        group_head(
            body,
            fonts,
            &format!("BENCHMARKS · ON DEMAND · {}", benches.len()),
        );
        if benches.is_empty() {
            empty_line(
                body,
                fonts,
                "Ask the copilot for one on a changed function.".into(),
            );
        }
        for row in benches {
            draw_row(body, fonts, pr, row);
        }
        group_head(body, fonts, "LOAD · COMES LATER");
        body.spawn(Node {
            width: percent(100),
            padding: UiRect::new(px(14), px(14), px(10), px(12)),
            ..default()
        })
        .with_child((
            button(
                fonts,
                "+ Ask the copilot for a scenario",
                Variant::Secondary,
            ),
            CockpitAskScenario(pr.clone()),
            observe(on_ask_scenario),
        ));
    });
    p.spawn(divider());
    p.spawn(Node {
        width: percent(100),
        flex_shrink: 0.0,
        flex_direction: FlexDirection::Column,
        align_items: AlignItems::Stretch,
        row_gap: px(6),
        padding: UiRect::all(px(12)),
        ..default()
    })
    .with_children(|foot| {
        foot.spawn(Node {
            align_items: AlignItems::Center,
            column_gap: px(8),
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                checkbox(view.on_main),
                CockpitOnMain(pr.clone()),
                observe(on_toggle_main),
            ));
            row.spawn(text(fonts, "Run on main too", Type::BODY));
        });
        if let Some(note) = &view.on_main_note {
            foot.spawn(wrapped_text(fonts, note.clone(), Type::META));
        }
        match &view.run_all.off {
            None => {
                foot.spawn((
                    button(fonts, &view.run_all.label, Variant::Secondary),
                    CockpitRunAll(pr.clone()),
                    observe(on_run_all),
                ))
                .entry::<Node>()
                .and_modify(wide);
            }
            Some(reason) => {
                foot.spawn(disabled_button(fonts, &view.run_all.label))
                    .entry::<Node>()
                    .and_modify(wide);
                foot.spawn(wrapped_text(fonts, reason.clone(), Type::META));
            }
        }
    });
}

/// A button keeps its content width on its own; the bottom block's button takes the row.
fn wide(mut node: Mut<Node>) {
    node.align_self = AlignSelf::Stretch;
}

fn group_head(p: &mut Spawner, fonts: &UiFonts, label: &str) {
    p.spawn(Node {
        width: percent(100),
        flex_shrink: 0.0,
        padding: UiRect::new(px(14), px(14), px(12), px(6)),
        ..default()
    })
    .with_child(text(fonts, label.to_string(), GROUP));
}

fn empty_line(p: &mut Spawner, fonts: &UiFonts, line: String) {
    p.spawn(Node {
        width: percent(100),
        padding: UiRect::axes(px(14), px(4)),
        ..default()
    })
    .with_child(wrapped_text(fonts, line, Type::META));
}

fn draw_row(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, row: &Row) {
    let title = Type {
        weight: if row.selected { 700 } else { 400 },
        mono: row.mono,
        ink: Swatch::Fg,
        ..Type::BODY
    };
    p.spawn((
        Node {
            width: percent(100),
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            padding: UiRect::axes(px(14), px(8)),
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
        Stroke(Swatch::Line),
        CockpitRow {
            pr: pr.clone(),
            item: row.item.clone(),
        },
        observe(on_row),
    ))
    .with_children(|r| {
        r.spawn(wrapped_text(fonts, row.title.clone(), title));
        r.spawn(wrapped_text(
            fonts,
            row.status.clone(),
            Type::META.ink(tone_swatch(row.tone)),
        ));
    });
}

fn on_row(activate: On<Activate>, rows: Query<&CockpitRow>, mut models: ResMut<CockpitModels>) {
    let Ok(CockpitRow { pr, item }) = rows.get(activate.entity) else {
        return;
    };
    if let Some(model) = models.0.get_mut(pr) {
        model.select(item.clone());
    }
}

fn on_ask_scenario(
    activate: On<Activate>,
    buttons: Query<&CockpitAskScenario>,
    mut models: ResMut<CockpitModels>,
) {
    let Ok(CockpitAskScenario(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(model) = models.0.get_mut(pr) {
        focus_copilot_input(model);
    }
}

#[allow(clippy::too_many_arguments)]
fn on_run_all(
    activate: On<Activate>,
    buttons: Query<&CockpitRunAll>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(CockpitRunAll(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &app_model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    let on_main = model.on_main;
    if model
        .run_disabled_reason(None, on_main, model.tests_busy)
        .is_some()
    {
        return;
    }
    let Some(view) = model.view.as_ref() else {
        return;
    };
    if on_main && view.main.phase == MainPhase::NotCreated {
        // Main is made first; the reviewer clicks again once it is ready.
        if model.pending != Some(CockpitAskKind::OpenMain) {
            asks.send(Ask::OpenMain(pr.clone()));
            model.sent(CockpitAskKind::OpenMain);
        }
        return;
    }
    let seen = view.hash.clone();
    if let Some(ask) = model.click_run(pr, CockpitScope::Scenarios, on_main, seen) {
        asks.send(ask);
    }
}

#[allow(clippy::too_many_arguments)]
fn on_toggle_main(
    activate: On<Activate>,
    boxes: Query<&CockpitOnMain>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(CockpitOnMain(pr)) = boxes.get(activate.entity) else {
        return;
    };
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if model.on_main {
        model.on_main = false;
        model.touch();
        return;
    }
    let phase = model.view.as_ref().map(|v| v.main.phase.clone());
    match phase {
        // The reason shows under the box; the box stays unticked.
        Some(MainPhase::Unavailable { .. }) | None => {}
        Some(MainPhase::Ready | MainPhase::Creating | MainPhase::Preparing { .. }) => {
            model.on_main = true;
            model.touch();
        }
        Some(MainPhase::NotCreated) => {
            if let Some(reason) = read_only_reason(&tabs, &app_model, pr) {
                return not_sent(&mut toasts, &time, reason);
            }
            model.on_main = true;
            if model.pending != Some(CockpitAskKind::OpenMain) {
                asks.send(Ask::OpenMain(pr.clone()));
                model.sent(CockpitAskKind::OpenMain);
            } else {
                model.touch();
            }
        }
        Some(MainPhase::NeedsSetup | MainPhase::Failed { .. }) => {
            model.on_main = true;
            open_prepare_main(model);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn on_write_scenarios(
    activate: On<Activate>,
    buttons: Query<&CockpitWriteScenarios>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(CockpitWriteScenarios(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if model.pending == Some(CockpitAskKind::Write) || model.busy() {
        return;
    }
    if let Some(reason) = read_only_reason(&tabs, &app_model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    asks.send(Ask::WriteScenarios {
        pr: pr.clone(),
        ask: ScenarioAsk::Fresh,
    });
    model.sent(CockpitAskKind::Write);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{DemoCockpit, demo_cockpit};
    use crate::testing::NOW;
    use clusia_core::cockpit::{CockpitView, MAIN_FIRST, NO_SCENARIOS};

    fn loaded_model(state: DemoCockpit) -> CockpitModel {
        model_of(demo_cockpit(NOW, state))
    }

    fn model_of(view: CockpitView) -> CockpitModel {
        let mut m = CockpitModel::default();
        m.apply(&crate::bridge::CockpitTell::Loaded {
            pr: crate::fixture::demo_pr(),
            view: Box::new(view),
        });
        m
    }

    #[test]
    fn each_tone_has_a_theme_swatch() {
        assert_eq!(tone_swatch(Tone::Running), Swatch::Green);
        assert_eq!(tone_swatch(Tone::Worse), Swatch::Orange);
        assert_eq!(tone_swatch(Tone::Warn), Swatch::Amber);
        assert_eq!(tone_swatch(Tone::Faint), Swatch::Faint);
    }

    #[test]
    fn list_view_of_the_scenario_demo_has_one_selected_row_and_the_groups() {
        let model = loaded_model(DemoCockpit::Scenario);
        let v = list_view(&model, "Claude", NOW);
        assert_eq!(v.rows.iter().filter(|r| r.selected).count(), 1);
        assert!(v.rows.iter().any(|r| r.mono), "a benchmark row");
        assert!(v.rows.iter().any(|r| !r.mono), "a scenario row");
        assert_eq!(v.empty_center, None);
    }

    #[test]
    fn a_running_item_shows_running_and_its_neighbours_do_not() {
        let model = loaded_model(DemoCockpit::BenchRunning);
        let v = list_view(&model, "Claude", NOW);
        let running: Vec<&Row> = v.rows.iter().filter(|r| r.tone == Tone::Running).collect();
        assert_eq!(running.len(), 1, "{:?}", v.rows);
        assert!(running[0].status.starts_with("running"));
    }

    #[test]
    fn run_all_is_drawn_from_the_list_view() {
        let mut model = loaded_model(DemoCockpit::Scenario);
        let run_all = |m: &CockpitModel| list_view(m, "Claude", NOW).run_all;
        let ready = run_all(&model);
        assert_eq!(ready.label, "Run all · this PR");
        model.tests_busy = true;
        assert_eq!(run_all(&model).off.as_deref(), Some("Tests are running"));
        model.tests_busy = false;
        model.on_main = true;
        let with_main = run_all(&model);
        assert_eq!(with_main.label, "Run all · this PR and main");
        // The demo's main is ready: nothing to wait for.
        assert_eq!(with_main.off, None);
        let mut needs = model_of({
            let mut v = demo_cockpit(NOW, DemoCockpit::Scenario);
            v.main.phase = MainPhase::NeedsSetup;
            v
        });
        needs.on_main = true;
        assert_eq!(run_all(&needs).off.as_deref(), Some(MAIN_FIRST));
        let mut not_created = model_of({
            let mut v = demo_cockpit(NOW, DemoCockpit::Scenario);
            v.main.phase = MainPhase::NotCreated;
            v
        });
        not_created.on_main = true;
        assert_eq!(run_all(&not_created).off, None);
        let empty = loaded_model(DemoCockpit::Empty);
        assert_eq!(run_all(&empty).off.as_deref(), Some(NO_SCENARIOS));
        let running = loaded_model(DemoCockpit::BenchRunning);
        assert_eq!(
            run_all(&running).off.as_deref(),
            Some("The QA cockpit is running")
        );
    }

    #[test]
    fn the_empty_view_has_the_empty_center_and_no_rows() {
        let v = list_view(&loaded_model(DemoCockpit::Empty), "Claude", NOW);
        assert!(v.rows.is_empty());
        assert!(v.empty_center.is_some());
    }

    #[test]
    fn hostile_titles_stay_plain_in_the_row() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.scenarios[0].title = "[x](http://evil) \u{202e}**".into();
        let v = list_view(&model_of(view.clone()), "Claude", NOW);
        let row = v.rows.iter().find(|r| !r.mono).expect("a scenario row");
        assert!(!row.title.contains('\u{202e}'));
        assert!(row.title.contains("[x](http://evil)"));
        view.scenarios[0].title = "a".repeat(10_000);
        let v = list_view(&model_of(view), "Claude", NOW);
        let row = v.rows.iter().find(|r| !r.mono).expect("a scenario row");
        assert!(row.title.chars().count() <= super::super::TITLE_SHOWN + 1);
    }

    use crate::fixture;
    use crate::review_state::ReviewSection;
    use crate::testing;
    use clusia_core::cockpit::ItemKind;

    fn app_with(view: CockpitView) -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        testing::open_cockpit(&mut app, &pr, view);
        testing::recorded(&mut app);
        (app, pr)
    }

    fn scenario_with_main(phase: MainPhase) -> CockpitView {
        let mut v = demo_cockpit(NOW, DemoCockpit::Scenario);
        v.main.phase = phase;
        v
    }

    fn model<'a>(app: &'a App, pr: &PrRef) -> &'a CockpitModel {
        &app.world().resource::<CockpitModels>().0[pr]
    }

    fn click<C: Component>(app: &mut App, want: impl Fn(&C) -> bool) {
        let e = testing::find::<C>(app, want);
        testing::activate(app, e);
        testing::settle(app);
    }

    #[test]
    fn clicking_a_row_selects_it_and_sends_nothing() {
        let (mut app, pr) = app_with(demo_cockpit(NOW, DemoCockpit::Scenario));
        let bench = ItemRef {
            kind: ItemKind::Bench,
            id: model(&app, &pr).view.as_ref().expect("a view").benches[0]
                .slug
                .clone(),
        };
        click::<CockpitRow>(&mut app, |r| r.item == bench);
        assert_eq!(testing::recorded(&mut app), []);
        assert_eq!(model(&app, &pr).selected.as_ref(), Some(&bench));
        let v = list_view(model(&app, &pr), "Claude Code", NOW);
        assert_eq!(v.rows.iter().filter(|r| r.selected).count(), 1);
    }

    #[test]
    fn run_all_sends_the_hash_the_list_drew() {
        let (mut app, pr) = app_with(demo_cockpit(NOW, DemoCockpit::Scenario));
        let hash = model(&app, &pr).view.as_ref().expect("a view").hash.clone();
        click::<CockpitRunAll>(&mut app, |b| b.0 == pr);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RunCockpit {
                pr: pr.clone(),
                scope: CockpitScope::Scenarios,
                on_main: false,
                seen: hash,
            }]
        );
        assert_eq!(model(&app, &pr).pending, Some(CockpitAskKind::Run));
        assert_eq!(
            testing::count::<CockpitRunAll>(&mut app),
            0,
            "the button is drawn off while the run is pending"
        );
        let all_off = testing::shows(&mut app, "The QA cockpit is running");
        assert!(all_off);
    }

    #[test]
    fn run_all_with_on_main_and_main_not_created_opens_main_and_does_not_run() {
        let (mut app, pr) = app_with(scenario_with_main(MainPhase::NotCreated));
        click::<CockpitOnMain>(&mut app, |b| b.0 == pr);
        assert_eq!(testing::recorded(&mut app), [Ask::OpenMain(pr.clone())]);
        assert!(model(&app, &pr).on_main);
        click::<CockpitRunAll>(&mut app, |b| b.0 == pr);
        let sent = testing::recorded(&mut app);
        assert!(
            !sent.iter().any(|a| matches!(a, Ask::RunCockpit { .. })),
            "{sent:?}"
        );
        assert!(
            !sent.contains(&Ask::OpenMain(pr.clone())),
            "main was asked for already: {sent:?}"
        );
    }

    #[test]
    fn run_all_after_main_is_ready_runs_on_both() {
        let (mut app, pr) = app_with(demo_cockpit(NOW, DemoCockpit::Scenario));
        click::<CockpitOnMain>(&mut app, |b| b.0 == pr);
        assert_eq!(testing::recorded(&mut app), [], "ready main: a plain tick");
        assert!(model(&app, &pr).on_main);
        click::<CockpitRunAll>(&mut app, |b| b.0 == pr);
        let sent = testing::recorded(&mut app);
        assert!(matches!(
            sent.as_slice(),
            [Ask::RunCockpit { on_main: true, .. }]
        ));
    }

    #[test]
    fn unticking_run_on_main_clears_it() {
        let (mut app, pr) = app_with(demo_cockpit(NOW, DemoCockpit::Scenario));
        click::<CockpitOnMain>(&mut app, |b| b.0 == pr);
        click::<CockpitOnMain>(&mut app, |b| b.0 == pr);
        assert!(!model(&app, &pr).on_main);
        assert_eq!(testing::recorded(&mut app), []);
    }

    #[test]
    fn ticking_run_on_main_per_phase() {
        let tick = |phase: MainPhase| {
            let (mut app, pr) = app_with(scenario_with_main(phase));
            click::<CockpitOnMain>(&mut app, |b| b.0 == pr);
            let sent = testing::recorded(&mut app);
            (app, pr, sent)
        };
        let (app, pr, sent) = tick(MainPhase::NotCreated);
        assert!(model(&app, &pr).on_main);
        assert_eq!(sent, [Ask::OpenMain(pr.clone())]);
        let (app, pr, sent) = tick(MainPhase::NeedsSetup);
        assert!(model(&app, &pr).on_main);
        assert_eq!(sent, []);
        assert!(matches!(
            model(&app, &pr).modal,
            Some(CockpitModal::PrepareMain { .. })
        ));
        let (mut app, pr, sent) = tick(MainPhase::Unavailable {
            reason: "docker is stopped".into(),
        });
        assert!(!model(&app, &pr).on_main);
        assert_eq!(sent, []);
        assert!(testing::shows(&mut app, "docker is stopped"));
        let (app, pr, sent) = tick(MainPhase::Preparing { step: "b".into() });
        assert!(model(&app, &pr).on_main);
        assert_eq!(sent, []);
        assert_eq!(model(&app, &pr).modal, None);
        let (app, pr, sent) = tick(MainPhase::Failed {
            message: "x".into(),
        });
        assert!(model(&app, &pr).on_main);
        assert_eq!(sent, []);
        assert!(matches!(
            model(&app, &pr).modal,
            Some(CockpitModal::PrepareMain { .. })
        ));
    }

    #[test]
    fn ask_the_copilot_for_a_scenario_sends_nothing_and_asks_for_focus() {
        let (mut app, pr) = app_with(demo_cockpit(NOW, DemoCockpit::Scenario));
        click::<CockpitAskScenario>(&mut app, |b| b.0 == pr);
        assert!(model(&app, &pr).copilot_focus);
        assert_eq!(testing::recorded(&mut app), []);
    }

    #[test]
    fn write_scenarios_sends_fresh_once_and_disables_while_writing() {
        let (mut app, pr) = app_with(demo_cockpit(NOW, DemoCockpit::Empty));
        assert!(testing::shows(&mut app, "Check this change end to end"));
        click::<CockpitWriteScenarios>(&mut app, |b| b.0 == pr);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::WriteScenarios {
                pr: pr.clone(),
                ask: ScenarioAsk::Fresh
            }]
        );
        assert_eq!(testing::count::<CockpitWriteScenarios>(&mut app), 0);
        assert!(testing::shows(&mut app, "Write scenarios with"));
    }

    #[test]
    fn the_writing_state_draws_one_spinner_per_place_and_none_idle() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Empty);
        view.activity = Some(clusia_core::cockpit::CockpitActivity::Writing {
            what: "scenarios".into(),
        });
        let (mut app, _pr) = app_with(view);
        assert!(testing::shows(&mut app, "is writing scenarios…"));
        assert_eq!(
            testing::count::<crate::ui::spinner::Spinner>(&mut app),
            2,
            "the list and the empty card"
        );
        assert_eq!(testing::count::<CockpitWriteScenarios>(&mut app), 0);
    }

    #[test]
    fn the_empty_groups_say_what_to_do() {
        let (mut app, _pr) = app_with(demo_cockpit(NOW, DemoCockpit::Empty));
        for line in [
            "SCENARIOS · 0",
            "BENCHMARKS · ON DEMAND · 0",
            "LOAD · COMES LATER",
            "No scenarios yet. Ask Claude Code to write them from the diff.",
            "Ask the copilot for one on a changed function.",
            "Run on main too",
        ] {
            assert!(testing::shows(&mut app, line), "{line}");
        }
    }

    #[test]
    fn hostile_titles_are_drawn_as_plain_text() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.scenarios[0].title = "[x](http://evil) \u{202e}**".into();
        let (mut app, _pr) = app_with(view);
        assert!(testing::shows(&mut app, "[x](http://evil)"));
        assert!(
            !testing::shown_text(&mut app)
                .iter()
                .any(|t| t.contains('\u{202e}'))
        );
    }

    #[test]
    fn preparing_main_checks_the_steps_the_plan_modal_checks() {
        let mut model = loaded_model(DemoCockpit::Main);
        open_prepare_main(&mut model);
        let Some(CockpitModal::PrepareMain { checked }) = &model.modal else {
            panic!("the Prepare main modal is open");
        };
        let plan = model
            .view
            .as_ref()
            .and_then(|v| v.main.plan.as_ref())
            .expect("the demo has main's plan");
        assert_eq!(checked, &approvable_ids(plan));
    }

    #[test]
    fn focus_copilot_input_sets_the_flag_and_bumps_the_version() {
        let mut model = CockpitModel::default();
        let before = model.version;
        focus_copilot_input(&mut model);
        assert!(model.copilot_focus);
        assert!(model.version > before);
    }
}
