//! The body of the Tests tab: what the center of a ready review shows in its Tests section
//! before and around the Auto Tests regions. `body_view` is a pure function of the model; the
//! body is rebuilt only when its `Shape` differs from the one it drew.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::printable::{printable, printable_lines};
use clusia_core::tests::{ACTIVITY_WRITING, RECIPE_FILE, TestsView};

use super::{SubTab, TabState, TestsModel, TestsModels, engine_label, tab_state};
use crate::agent_name;
use crate::bridge::{Ask, Asks, Model, TestsAskKind, Toasts};
use crate::fonts::UiFonts;
use crate::review_state::{ReviewSection, ReviewTabs};
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::screens::review::shell::SectionBody;
use crate::theme::Swatch;
use crate::ui::kit::{
    Type, Variant, button, card, disabled_button, panel, segment, segments, text, wrapped_text,
};

/// What `Failed` and `Refused` need to draw their buttons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Actions {
    /// Why nothing can be sent now (`read_only_reason`).
    pub off: Option<&'static str>,
    /// A `GenerateTests` is in flight.
    pub starting: bool,
    /// The worktree is known, so **Edit recipe** has a file to open.
    pub can_edit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Shape {
    Loading,
    Empty {
        agent: String,
        engine: Option<String>,
        off: Option<&'static str>,
        starting: bool,
    },
    Generating {
        agent: String,
        step: u8,
        stopping: bool,
    },
    Failed {
        message: String,
        actions: Actions,
    },
    Refused {
        reason: String,
        actions: Actions,
    },
    /// `agent` is `Some` only under Run Function tests, whose note names it.
    Auto {
        sub: SubTab,
        agent: Option<String>,
    },
}

pub(super) struct Inputs<'a> {
    pub model: Option<&'a TestsModel>,
    pub agent: &'a str,
    pub read_only: Option<&'static str>,
    pub can_edit: bool,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct TestsBody(pub PrRef);

/// The empty column holding the top cards of the Auto Tests layout.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct TopRegion(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ListRegion(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DetailRegion(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ChecksRegion(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SubTabButton {
    pub pr: PrRef,
    pub sub: SubTab,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct GenerateButton(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct StopButton(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct EditRecipe(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CheckAgain(pub Option<PrRef>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Light {
    Idle,
    Done,
    Lit,
    Waiting,
}

/// A numbered circle of the steps card.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct StepDot {
    pub n: u8,
    pub light: Light,
}

/// The `Shape` the `TestsBody` it sits on was drawn from.
#[derive(Component)]
pub(super) struct Built(Shape);

/// `Docker (here: Colima, running)`'s parenthesis: the engine an earlier probe found.
fn engine_here(view: &TestsView) -> Option<String> {
    let engine = view.env.as_ref()?.engine.as_ref()?;
    let state = if engine.running { "running" } else { "stopped" };
    Some(printable(&format!(
        "{}, {state}",
        engine_label(engine).trim_start_matches("Docker · ")
    )))
}

pub(super) fn body_view(i: &Inputs) -> Shape {
    let pending = i.model.and_then(|m| m.pending);
    let Some((model, view)) = i.model.and_then(|m| Some((m, m.view.as_ref()?))) else {
        return match i.read_only {
            None => Shape::Loading,
            Some(off) => Shape::Empty {
                agent: i.agent.to_string(),
                engine: None,
                off: Some(off),
                starting: false,
            },
        };
    };
    let actions = || Actions {
        off: i.read_only,
        starting: pending == Some(TestsAskKind::Generate),
        can_edit: i.can_edit,
    };
    match tab_state(view, model.live.as_ref()) {
        TabState::Empty => Shape::Empty {
            agent: i.agent.to_string(),
            engine: engine_here(view),
            off: i.read_only,
            starting: pending == Some(TestsAskKind::Generate),
        },
        TabState::Generating { activity } => Shape::Generating {
            agent: i.agent.to_string(),
            step: if activity.as_deref() == Some(ACTIVITY_WRITING) {
                2
            } else {
                1
            },
            stopping: pending == Some(TestsAskKind::Stop),
        },
        TabState::Failed { message } => Shape::Failed {
            message: printable_lines(&message),
            actions: actions(),
        },
        TabState::Refused { reason } => Shape::Refused {
            reason: printable_lines(&reason),
            actions: actions(),
        },
        _ => Shape::Auto {
            sub: model.sub,
            agent: (model.sub == SubTab::Functions).then(|| i.agent.to_string()),
        },
    }
}

pub(super) fn on_generate(
    activate: On<Activate>,
    buttons: Query<&GenerateButton>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut models: ResMut<TestsModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(GenerateButton(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    asks.send(Ask::GenerateTests(pr.clone()));
    models
        .0
        .entry(pr.clone())
        .or_default()
        .sent(TestsAskKind::Generate);
}

pub(super) fn on_stop(
    activate: On<Activate>,
    buttons: Query<&StopButton>,
    mut models: ResMut<TestsModels>,
    mut asks: ResMut<Asks>,
) {
    let Ok(StopButton(pr)) = buttons.get(activate.entity) else {
        return;
    };
    asks.send(Ask::StopTests(pr.clone()));
    models
        .0
        .entry(pr.clone())
        .or_default()
        .sent(TestsAskKind::Stop);
}

pub(super) fn on_edit_recipe(
    activate: On<Activate>,
    buttons: Query<&EditRecipe>,
    tabs: Res<ReviewTabs>,
    mut models: ResMut<TestsModels>,
    mut asks: ResMut<Asks>,
) {
    let Ok(EditRecipe(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let Some(worktree) = tabs
        .0
        .get(pr)
        .and_then(|t| t.ready())
        .and_then(|r| r.view.worktree.as_deref())
    else {
        return;
    };
    asks.send(Ask::OpenInEditor {
        path: format!("{worktree}/{RECIPE_FILE}"),
        line: None,
    });
    let model = models.0.entry(pr.clone()).or_default();
    if !model.reread_on_focus {
        model.reread_on_focus = true;
        model.touch();
    }
}

pub(super) fn on_check_again(
    activate: On<Activate>,
    buttons: Query<&CheckAgain>,
    mut asks: ResMut<Asks>,
) {
    let Ok(CheckAgain(pr)) = buttons.get(activate.entity) else {
        return;
    };
    asks.send(Ask::CheckTestEngine { pr: pr.clone() });
}

fn on_sub_tab(
    activate: On<Activate>,
    buttons: Query<&SubTabButton>,
    mut models: ResMut<TestsModels>,
) {
    let Ok(SubTabButton { pr, sub }) = buttons.get(activate.entity) else {
        return;
    };
    let model = models.0.entry(pr.clone()).or_default();
    if model.sub != *sub {
        model.sub = *sub;
        model.touch();
    }
}

/// Owns the `TestsBody` under every `SectionBody`: draws it while the tab is ready and on its
/// Tests section, and removes only its own root when the section moves away.
pub(super) fn fill_tests(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    models: Res<TestsModels>,
    model: Res<Model>,
    fonts: Res<UiFonts>,
    bodies: Query<(Entity, &SectionBody, Option<&Children>)>,
    roots: Query<(Entity, &Built), With<TestsBody>>,
) {
    for (entity, body, children) in &bodies {
        let Some(tab) = tabs.0.get(&body.pr) else {
            continue;
        };
        let root = children
            .into_iter()
            .flatten()
            .find_map(|c| roots.get(*c).ok());
        let ready = tab
            .ready()
            .filter(|_| tab.ui.section == ReviewSection::Tests);
        let Some(ready) = ready else {
            if let Some((root, _)) = root {
                commands.entity(root).try_despawn();
            }
            continue;
        };
        let agent = agent_name::of_tab(&model.snapshot, tab);
        let shape = body_view(&Inputs {
            model: models.0.get(&body.pr),
            agent: &agent,
            read_only: read_only_reason(&tabs, &model, &body.pr),
            can_edit: ready.view.worktree.is_some(),
        });
        if root.is_some_and(|(_, built)| built.0 == shape) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        let pr = body.pr.clone();
        commands.entity(entity).with_children(|p| {
            p.spawn((
                Node {
                    flex_grow: 1.0,
                    width: percent(100),
                    min_height: px(0),
                    flex_direction: FlexDirection::Column,
                    ..default()
                },
                TestsBody(pr.clone()),
                Built(shape.clone()),
            ))
            .with_children(|root| draw(root, &fonts, &pr, &shape));
        });
    }
}

type Spawner<'a> = ChildSpawnerCommands<'a>;

fn draw(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, shape: &Shape) {
    match shape {
        Shape::Loading => centered(p, |c| {
            c.spawn(text(fonts, "Loading the tests…", Type::MUTED));
        }),
        Shape::Empty {
            agent,
            engine,
            off,
            starting,
        } => centered(p, |c| {
            steps_card(
                c,
                fonts,
                "Test this change",
                agent,
                engine.as_deref(),
                [Light::Idle; 3],
                |row| {
                    if *starting {
                        row.spawn(disabled_button(fonts, "Starting…"));
                    } else if off.is_some() {
                        row.spawn(disabled_button(fonts, "Generate tests"));
                    } else {
                        generate_button(row, fonts, pr, "Generate tests");
                    }
                    let meta = match off {
                        Some(reason) => (*reason).to_string(),
                        None => format!("Uses one {agent} turn on your account"),
                    };
                    row.spawn(text(fonts, meta, Type::MUTED));
                },
            );
        }),
        Shape::Generating {
            agent,
            step,
            stopping,
        } => centered(p, |c| {
            let lights = if *step == 2 {
                [Light::Done, Light::Lit, Light::Waiting]
            } else {
                [Light::Lit, Light::Waiting, Light::Waiting]
            };
            steps_card(c, fonts, "Generating tests", agent, None, lights, |row| {
                if *stopping {
                    row.spawn(disabled_button(fonts, "Stopping…"));
                } else {
                    row.spawn((
                        button(fonts, "Stop", Variant::Secondary),
                        StopButton(pr.clone()),
                        observe(on_stop),
                    ));
                }
            });
        }),
        Shape::Failed { message, actions } => centered(p, |c| {
            outcome_card(
                c,
                fonts,
                "The tests could not be prepared",
                |card| {
                    card.spawn(panel(
                        Node {
                            padding: px(12).all(),
                            ..default()
                        },
                        Swatch::Chrome,
                    ))
                    .with_child(wrapped_text(
                        fonts,
                        message.clone(),
                        Type::MONO,
                    ));
                },
                |row| {
                    again_buttons(row, fonts, pr, actions, Variant::Primary);
                    edit_button(row, fonts, pr, actions);
                },
            );
        }),
        Shape::Refused { reason, actions } => centered(p, |c| {
            outcome_card(
                c,
                fonts,
                "Clúsia won't run these tests",
                |card| {
                    card.spawn(wrapped_text(fonts, reason.clone(), Type::BODY));
                },
                |row| {
                    edit_button(row, fonts, pr, actions);
                    row.spawn((
                        button(fonts, "Check again", Variant::Secondary),
                        CheckAgain(Some(pr.clone())),
                        observe(on_check_again),
                    ));
                    again_buttons(row, fonts, pr, actions, Variant::Secondary);
                },
            );
        }),
        Shape::Auto { sub, agent } => auto(p, fonts, pr, *sub, agent.as_deref()),
    }
}

/// The body's content, centered in both directions.
fn centered(p: &mut Spawner, build: impl FnOnce(&mut Spawner)) {
    p.spawn(Node {
        flex_grow: 1.0,
        min_height: px(0),
        padding: px(25).all(),
        flex_direction: FlexDirection::Column,
        justify_content: JustifyContent::Center,
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(build);
}

fn card_node() -> Node {
    Node {
        width: px(600),
        max_width: percent(100),
        padding: px(32).all(),
        flex_direction: FlexDirection::Column,
        row_gap: px(14),
        ..default()
    }
}

fn button_row() -> Node {
    Node {
        align_items: AlignItems::Center,
        column_gap: px(12),
        flex_wrap: FlexWrap::Wrap,
        ..default()
    }
}

fn generate_button(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, label: &str) {
    p.spawn((
        button(fonts, label, Variant::Primary),
        GenerateButton(pr.clone()),
        observe(on_generate),
    ));
}

/// **Generate tests again**, or its disabled form while a start is in flight or nothing can be
/// sent.
fn again_buttons(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, actions: &Actions, v: Variant) {
    if actions.starting {
        p.spawn(disabled_button(fonts, "Starting…"));
    } else if actions.off.is_some() {
        p.spawn(disabled_button(fonts, "Generate tests again"));
    } else if v == Variant::Primary {
        generate_button(p, fonts, pr, "Generate tests again");
    } else {
        p.spawn((
            button(fonts, "Generate tests again", v),
            GenerateButton(pr.clone()),
            observe(on_generate),
        ));
    }
}

fn edit_button(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, actions: &Actions) {
    if actions.can_edit {
        p.spawn((
            button(fonts, "Edit recipe", Variant::Secondary),
            EditRecipe(pr.clone()),
            observe(on_edit_recipe),
        ));
    }
}

fn outcome_card(
    p: &mut Spawner,
    fonts: &UiFonts,
    title: &str,
    body: impl FnOnce(&mut Spawner),
    buttons: impl FnOnce(&mut Spawner),
) {
    p.spawn(card(card_node())).with_children(|c| {
        c.spawn(text(fonts, title, Type::HEADING.size(18.0)));
        body(c);
        c.spawn(button_row()).with_children(buttons);
    });
}

/// The card of the empty and generating states: a title, the three steps and `footer` (the
/// buttons) in a row at the bottom.
fn steps_card(
    p: &mut Spawner,
    fonts: &UiFonts,
    title: &str,
    agent: &str,
    engine: Option<&str>,
    lights: [Light; 3],
    footer: impl FnOnce(&mut Spawner),
) {
    let found = match engine {
        Some(engine) => {
            format!("Finds Docker (here: {engine}) and what is already built for this repository.")
        }
        None => "Finds a container engine and what is already built for this repository.".into(),
    };
    let steps = [
        ("Check the environment".to_string(), found),
        (
            format!("{agent} writes the recipe"),
            "How to set up and run the tests. It only reads the code.".into(),
        ),
        (
            "You approve the setup".to_string(),
            "You see every command before it runs. Tests run with no network.".into(),
        ),
    ];
    p.spawn(card(card_node())).with_children(|c| {
        c.spawn(text(fonts, title, Type::HEADING.size(18.0)));
        c.spawn(wrapped_text(
            fonts,
            "Run the repository's tests in isolation. Nothing has run yet for this pull request.",
            Type::MUTED,
        ));
        for (i, (head, line)) in steps.into_iter().enumerate() {
            c.spawn(Node {
                column_gap: px(12),
                align_items: AlignItems::FlexStart,
                ..default()
            })
            .with_children(|row| {
                step_dot(row, fonts, i as u8 + 1, lights[i]);
                row.spawn(Node {
                    flex_direction: FlexDirection::Column,
                    flex_shrink: 1.0,
                    min_width: px(0),
                    row_gap: px(2),
                    ..default()
                })
                .with_children(|col| {
                    col.spawn(text(fonts, head, Type::STRONG));
                    col.spawn(wrapped_text(fonts, line, Type::MUTED));
                });
            });
        }
        c.spawn(Node {
            margin: UiRect::top(px(6)),
            ..button_row()
        })
        .with_children(footer);
    });
}

fn step_dot(p: &mut Spawner, fonts: &UiFonts, n: u8, light: Light) {
    let (fill, ink, label) = match light {
        Light::Idle => (Swatch::GreenSoft, Swatch::Green, n.to_string()),
        Light::Done => (Swatch::GreenSoft, Swatch::Green, "✓".to_string()),
        Light::Lit => (Swatch::Green, Swatch::OnGreen, n.to_string()),
        Light::Waiting => (Swatch::Chrome, Swatch::Muted, n.to_string()),
    };
    p.spawn((
        panel(
            Node {
                width: px(24),
                height: px(24),
                flex_shrink: 0.0,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(12)),
                ..default()
            },
            fill,
        ),
        StepDot { n, light },
    ))
    .with_child(text(fonts, label, Type::STRONG.size(12.0).ink(ink)));
}

fn auto(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, sub: SubTab, agent: Option<&str>) {
    p.spawn(Node {
        padding: UiRect::new(px(25), px(25), px(14), px(0)),
        ..default()
    })
    .with_children(|row| {
        row.spawn(segments()).with_children(|tabs| {
            for (which, label) in [
                (SubTab::Auto, "Auto Tests"),
                (SubTab::Functions, "Run Function tests"),
            ] {
                tabs.spawn((
                    segment(fonts, label, sub == which),
                    SubTabButton {
                        pr: pr.clone(),
                        sub: which,
                    },
                    observe(on_sub_tab),
                ));
            }
        });
    });
    if sub == SubTab::Functions {
        let agent = agent.unwrap_or_default();
        return centered(p, |c| {
            c.spawn(Node {
                max_width: px(460),
                ..default()
            })
            .with_child((
                text(
                    fonts,
                    format!(
                        "Run Function tests comes next: each changed function, run alone with cases {agent} writes and values you choose."
                    ),
                    Type::MUTED,
                ),
                TextLayout::new(
                    bevy::text::Justify::Center,
                    bevy::text::LineBreak::WordOrCharacter,
                ),
            ));
        });
    }
    p.spawn((
        Node {
            flex_grow: 1.0,
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(14),
            padding: UiRect::new(px(25), px(25), px(12), px(14)),
            overflow: Overflow::scroll_y(),
            ..default()
        },
        ScrollArea,
        ScrollPosition::default(),
    ))
    .with_children(|col| {
        col.spawn((region(), TopRegion(pr.clone())));
        col.spawn(Node {
            column_gap: px(16),
            min_height: px(340),
            ..default()
        })
        .with_children(|row| {
            row.spawn((half(), ListRegion(pr.clone())));
            row.spawn((half(), DetailRegion(pr.clone())));
        });
        col.spawn((region(), ChecksRegion(pr.clone())));
    });
}

fn region() -> Node {
    Node {
        width: percent(100),
        flex_direction: FlexDirection::Column,
        ..default()
    }
}

fn half() -> Node {
    Node {
        flex_grow: 1.0,
        flex_basis: px(0),
        min_width: px(0),
        flex_direction: FlexDirection::Column,
        ..default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Connection, Tell};
    use crate::fixture;
    use crate::review_state::ReviewSection;
    use crate::testing::{self, NOW};
    use clusia_core::tests::{
        ACTIVITY_PROBING, ACTIVITY_WRITING, RECIPE_FILE, SetupPlan, TestsPhase,
    };

    /// A demo app with the review open on its Tests section.
    fn tests_app() -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        (app, pr)
    }

    fn open(app: &mut App, pr: &PrRef, phase: TestsPhase) -> TestsView {
        let view = testing::tests_view(app, pr, phase);
        testing::open_tests(app, pr, view.clone());
        testing::recorded(app);
        view
    }

    fn open_view(app: &mut App, pr: &PrRef, view: TestsView) {
        testing::open_tests(app, pr, view);
        testing::recorded(app);
    }

    fn set_worktree(app: &mut App, pr: &PrRef) {
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(pr)
            .and_then(|t| t.ready_mut())
            .expect("a ready tab")
            .view
            .worktree = Some("/work/rzorzal-clusia-123".into());
        testing::settle(app);
    }

    fn model(app: &App, pr: &PrRef) -> TestsModel {
        let models = app.world().resource::<TestsModels>();
        let m = &models.0[pr];
        TestsModel {
            view: m.view.clone(),
            pending: m.pending,
            sub: m.sub,
            reread_on_focus: m.reread_on_focus,
            ..TestsModel::default()
        }
    }

    fn asked(app: &mut App) -> Vec<Ask> {
        testing::recorded(app)
    }

    fn lights(app: &mut App) -> Vec<(u8, Light)> {
        let mut q = app.world_mut().query::<&StepDot>();
        let mut dots: Vec<(u8, Light)> = q.iter(app.world()).map(|d| (d.n, d.light)).collect();
        dots.sort_by_key(|d| d.0);
        dots
    }

    #[test]
    fn the_empty_tab_offers_generate_tests_and_sends_nothing_by_itself() {
        let (mut app, pr) = tests_app();
        open(&mut app, &pr, TestsPhase::NotGenerated);
        testing::settle(&mut app);
        for needle in [
            "Test this change",
            "Run the repository's tests in isolation. Nothing has run yet for this pull request.",
            "Check the environment",
            "Finds a container engine and what is already built for this repository.",
            "Claude Code writes the recipe",
            "How to set up and run the tests. It only reads the code.",
            "You approve the setup",
            "You see every command before it runs. Tests run with no network.",
            "Generate tests",
            "Uses one Claude Code turn on your account",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert_eq!(testing::count::<SubTabButton>(&mut app), 0, "no sub-tabs");
        assert_eq!(
            lights(&mut app),
            [(1, Light::Idle), (2, Light::Idle), (3, Light::Idle)]
        );
        let sent = asked(&mut app);
        assert!(
            !sent.iter().any(|a| matches!(
                a,
                Ask::GenerateTests(_)
                    | Ask::PrepareTests { .. }
                    | Ask::RunTests { .. }
                    | Ask::ListTests(_)
            )),
            "{sent:?}"
        );
    }

    #[test]
    fn the_first_step_names_the_engine_only_when_one_is_known() {
        let (mut app, pr) = tests_app();
        let mut view = testing::tests_view(&app, &pr, TestsPhase::NotGenerated);
        view.env = Some(testing::tests_env(true));
        open_view(&mut app, &pr, view.clone());
        assert!(testing::shows(
            &mut app,
            "Finds Docker (here: Colima, running) and what is already built for this repository."
        ));
        view.env = Some(testing::tests_env(false));
        open_view(&mut app, &pr, view.clone());
        assert!(testing::shows(
            &mut app,
            "Finds Docker (here: Colima, stopped)"
        ));
        view.env.as_mut().expect("an env").engine = None;
        open_view(&mut app, &pr, view);
        assert!(testing::shows(
            &mut app,
            "Finds a container engine and what is already built for this repository."
        ));
    }

    #[test]
    fn generate_tests_sends_one_ask_and_waits_for_the_daemon() {
        let (mut app, pr) = tests_app();
        open(&mut app, &pr, TestsPhase::NotGenerated);
        let button = testing::find::<GenerateButton>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, button);
        testing::settle(&mut app);
        assert_eq!(
            asked(&mut app)
                .iter()
                .filter(|a| **a == Ask::GenerateTests(pr.clone()))
                .count(),
            1
        );
        assert_eq!(model(&app, &pr).pending, Some(TestsAskKind::Generate));
        assert_eq!(
            testing::count::<GenerateButton>(&mut app),
            0,
            "a second click has nothing to land on"
        );
        assert!(testing::shows(&mut app, "Starting…"));
    }

    #[test]
    fn generate_tests_is_disabled_with_the_reason_when_offline() {
        let (mut app, pr) = tests_app();
        open(&mut app, &pr, TestsPhase::NotGenerated);
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::settle(&mut app);
        assert_ne!(
            app.world().resource::<crate::bridge::Model>().connection,
            Connection::Live
        );
        assert_eq!(testing::count::<GenerateButton>(&mut app), 0);
        assert!(testing::shows(&mut app, "Not connected to clusiad"));
        assert!(!testing::shows(&mut app, "Uses one Claude Code turn"));
    }

    #[test]
    fn stop_shows_while_generating_and_sends_stop_tests() {
        let (mut app, pr) = tests_app();
        open(
            &mut app,
            &pr,
            TestsPhase::Generating {
                activity: Some(ACTIVITY_PROBING.into()),
            },
        );
        assert!(testing::shows(&mut app, "Generating tests"));
        assert_eq!(
            lights(&mut app),
            [(1, Light::Lit), (2, Light::Waiting), (3, Light::Waiting)]
        );
        let stop = testing::find::<StopButton>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, stop);
        testing::settle(&mut app);
        assert_eq!(asked(&mut app), [Ask::StopTests(pr.clone())]);
        assert_eq!(model(&app, &pr).pending, Some(TestsAskKind::Stop));
        assert_eq!(testing::count::<StopButton>(&mut app), 0);
        assert!(testing::shows(&mut app, "Stopping…"));
    }

    #[test]
    fn stop_is_sent_even_when_the_connection_is_gone() {
        let (mut app, pr) = tests_app();
        open(&mut app, &pr, TestsPhase::Generating { activity: None });
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::settle(&mut app);
        testing::recorded(&mut app);
        let stop = testing::find::<StopButton>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, stop);
        assert_eq!(
            asked(&mut app),
            [Ask::StopTests(pr)],
            "the bridge answers offline"
        );
    }

    #[test]
    fn the_writing_activity_lights_the_second_step() {
        let (mut app, pr) = tests_app();
        open(
            &mut app,
            &pr,
            TestsPhase::Generating {
                activity: Some(ACTIVITY_WRITING.into()),
            },
        );
        assert_eq!(
            lights(&mut app),
            [(1, Light::Done), (2, Light::Lit), (3, Light::Waiting)]
        );
        open(
            &mut app,
            &pr,
            TestsPhase::Generating {
                activity: Some("Thinking".into()),
            },
        );
        assert_eq!(
            lights(&mut app),
            [(1, Light::Lit), (2, Light::Waiting), (3, Light::Waiting)],
            "any other activity lights step 1"
        );
    }

    #[test]
    fn a_failed_tab_shows_the_message_as_plain_text_and_offers_the_way_out() {
        let (mut app, pr) = tests_app();
        set_worktree(&mut app, &pr);
        open(
            &mut app,
            &pr,
            TestsPhase::Failed {
                message: "boom [x](https://evil.example)\n\u{1b}[31mred\u{202e}".into(),
            },
        );
        assert!(testing::shows(&mut app, "The tests could not be prepared"));
        assert!(testing::shows(
            &mut app,
            "boom [x](https://evil.example)\n\\u{1b}[31mred\\u{202e}"
        ));
        assert!(testing::shows(&mut app, "Generate tests again"));
        let generate = testing::find::<GenerateButton>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, generate);
        assert!(asked(&mut app).contains(&Ask::GenerateTests(pr.clone())));
        let edit = testing::find::<EditRecipe>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, edit);
        assert_eq!(
            asked(&mut app),
            [Ask::OpenInEditor {
                path: format!("/work/rzorzal-clusia-123/{RECIPE_FILE}"),
                line: None,
            }]
        );
        assert!(model(&app, &pr).reread_on_focus);
    }

    #[test]
    fn edit_recipe_is_hidden_when_the_worktree_is_unknown() {
        let (mut app, pr) = tests_app();
        open(
            &mut app,
            &pr,
            TestsPhase::Failed {
                message: "no".into(),
            },
        );
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .and_then(|t| t.ready_mut())
            .expect("a ready tab")
            .view
            .worktree = None;
        testing::settle(&mut app);
        assert_eq!(testing::count::<EditRecipe>(&mut app), 0);
    }

    fn refused(reason: &str) -> SetupPlan {
        SetupPlan {
            recipe_hash: "h1".into(),
            steps: Vec::new(),
            commands: Vec::new(),
            commands_approved: false,
            ready: false,
            blocked: Some(reason.into()),
        }
    }

    #[test]
    fn a_refused_tab_gives_the_reason_and_check_again_asks_the_daemon() {
        let (mut app, pr) = tests_app();
        let mut view = testing::tests_view(&app, &pr, TestsPhase::NeedsSetup);
        view.plan = Some(refused("The repository ships its own .clusia folder"));
        open_view(&mut app, &pr, view);
        assert!(testing::shows(&mut app, "Clúsia won't run these tests"));
        assert!(testing::shows(
            &mut app,
            "The repository ships its own .clusia folder"
        ));
        let again = testing::find::<CheckAgain>(&mut app, |b| b.0.as_ref() == Some(&pr));
        testing::activate(&mut app, again);
        assert_eq!(
            asked(&mut app),
            [Ask::CheckTestEngine {
                pr: Some(pr.clone())
            }]
        );
        assert_eq!(testing::count::<SubTabButton>(&mut app), 0);
    }

    #[test]
    fn auto_tests_draws_its_four_regions_and_the_sub_tabs_swap_the_body() {
        let (mut app, pr) = tests_app();
        open(&mut app, &pr, TestsPhase::Ready);
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Auto Tests"));
        assert!(testing::shows(&mut app, "Run Function tests"));
        assert_eq!(testing::count::<TopRegion>(&mut app), 1);
        assert_eq!(testing::count::<ListRegion>(&mut app), 1);
        assert_eq!(testing::count::<DetailRegion>(&mut app), 1);
        assert_eq!(testing::count::<ChecksRegion>(&mut app), 1);
        let functions = testing::find::<SubTabButton>(&mut app, |b| b.sub == SubTab::Functions);
        testing::activate(&mut app, functions);
        testing::settle(&mut app);
        assert!(testing::shows(
            &mut app,
            "Run Function tests comes next: each changed function, run alone with cases Claude Code writes and values you choose."
        ));
        assert_eq!(testing::count::<TopRegion>(&mut app), 0);
        assert_eq!(testing::count::<SubTabButton>(&mut app), 2, "the tabs stay");
        let auto = testing::find::<SubTabButton>(&mut app, |b| b.sub == SubTab::Auto);
        testing::activate(&mut app, auto);
        testing::settle(&mut app);
        assert_eq!(testing::count::<TopRegion>(&mut app), 1);
        assert_eq!(model(&app, &pr).sub, SubTab::Auto);
    }

    #[test]
    fn every_auto_state_shares_one_body() {
        let (mut app, pr) = tests_app();
        open(&mut app, &pr, TestsPhase::Ready);
        testing::settle(&mut app);
        let body = testing::find::<TestsBody>(&mut app, |b| b.0 == pr);
        for phase in [
            TestsPhase::NeedsSetup,
            TestsPhase::Waiting,
            TestsPhase::Preparing {
                step: "image".into(),
            },
            TestsPhase::Running { run: "r1".into() },
            TestsPhase::Ready,
        ] {
            open(&mut app, &pr, phase);
            testing::settle(&mut app);
            assert_eq!(
                testing::find::<TestsBody>(&mut app, |b| b.0 == pr),
                body,
                "the body is not rebuilt for a run state"
            );
        }
    }

    #[test]
    fn a_settled_tab_rebuilds_nothing() {
        let (mut app, pr) = tests_app();
        open(&mut app, &pr, TestsPhase::NotGenerated);
        testing::settle(&mut app);
        let body = testing::find::<TestsBody>(&mut app, |b| b.0 == pr);
        let signature = testing::tree_signature(&app, body);
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(testing::find::<TestsBody>(&mut app, |b| b.0 == pr), body);
        assert_eq!(testing::tree_signature(&app, body), signature);
    }

    #[test]
    fn leaving_the_section_drops_the_body_and_coming_back_builds_it_again() {
        let (mut app, pr) = tests_app();
        open(&mut app, &pr, TestsPhase::NotGenerated);
        assert_eq!(testing::count::<TestsBody>(&mut app), 1);
        testing::set_section(&mut app, &pr, ReviewSection::Diff);
        assert_eq!(testing::count::<TestsBody>(&mut app), 0);
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        assert_eq!(testing::count::<TestsBody>(&mut app), 1);
        assert!(testing::shows(&mut app, "Test this change"));
    }

    #[test]
    fn a_tab_with_no_answer_yet_says_it_is_loading_and_an_offline_one_says_why() {
        fn inputs<'a>(
            model: Option<&'a TestsModel>,
            read_only: Option<&'static str>,
        ) -> Inputs<'a> {
            Inputs {
                model,
                agent: "Claude Code",
                read_only,
                can_edit: false,
            }
        }
        assert_eq!(body_view(&inputs(None, None)), Shape::Loading);
        assert_eq!(
            body_view(&inputs(None, Some("Not connected to clusiad — not sent"))),
            Shape::Empty {
                agent: "Claude Code".into(),
                engine: None,
                off: Some("Not connected to clusiad — not sent"),
                starting: false,
            }
        );
    }
}
