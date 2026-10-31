//! The setup plan modal of the Tests tab: what the agent's recipe wants to run, each step with
//! its command, and the two ways to approve it.
//!
//! The modal is open exactly while the model holds a plan and the tab shows Tests. Approval
//! reads the plan the modal last **drew**, never the model, so what is sent is what was seen.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::printable::{printable, printable_lines};
use clusia_core::tests::{
    Approve, PlanStep, STEP_ENGINE, STEP_STACK, STEP_TESTS, StepPlace, StepState,
};

use super::body::{EditRecipe, on_edit_recipe};
use super::{TabState, TestsAskKind, TestsModel, TestsModels};
use crate::agent_name;
use crate::bridge::{Ask, Asks, Model, Toasts};
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen};
use crate::review_state::{Modal, ReviewSection, ReviewTabs};
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::screens::review::shell::ModalFor;
use crate::theme::Swatch;
use crate::ui::kit::{
    Fill, Stroke, Tone, Type, Variant, badge, button, checkbox, disabled_button, text, wrapped_text,
};
use crate::ui::modal::{BackdropClicked, escape_pressed, modal_card, modal_root};

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PlanStepBox {
    pub pr: PrRef,
    pub id: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ApproveSelected(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ApproveAll(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CancelPlan(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PlanEditRecipe(pub PrRef);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RowKind {
    Approvable,
    Running,
    Done,
    Skipped(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StepRow {
    pub id: String,
    /// `printable`.
    pub title: String,
    /// `printable_lines`.
    pub command: String,
    pub place: StepPlace,
    pub network: bool,
    /// `printable`.
    pub hint: Option<String>,
    pub kind: RowKind,
    pub checked: bool,
}

/// What the modal draws. The approve buttons read the one it last drew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlanView {
    pub number: u64,
    pub agent: String,
    pub by_hand: bool,
    pub recipe_hash: String,
    /// `printable_lines`.
    pub blocked: Option<String>,
    pub steps: Vec<StepRow>,
    /// (label, command) in the order All, One test, Some tests, List; commands `printable_lines`.
    pub commands: Vec<(&'static str, String)>,
    /// `KEY=value  KEY=value`, each pair `printable`; `None` without variables.
    pub variables: Option<String>,
    pub stack_note: Option<String>,
    pub commands_approved: bool,
    pub changed_note: bool,
    pub sending: bool,
    pub can_edit: bool,
}

impl PlanView {
    fn approvable(&self) -> impl Iterator<Item = &StepRow> {
        self.steps.iter().filter(|s| s.kind == RowKind::Approvable)
    }

    fn offers_selected(&self) -> bool {
        !self.sending
            && self.blocked.is_none()
            && (!self.commands_approved || self.approvable().any(|s| s.checked))
    }

    fn offers_all(&self) -> bool {
        !self.sending && self.blocked.is_none()
    }
}

/// The view the modal root was last drawn from, and its card.
#[derive(Component)]
pub(super) struct PlanBuilt {
    pub view: PlanView,
    pub card: Entity,
    slots: Slots,
}

/// The three parts of the card. They outlive a redraw, which replaces only what is inside
/// them, so the body keeps its scroll offset.
#[derive(Debug, Clone, Copy)]
struct Slots {
    head: Entity,
    body: Entity,
    foot: Entity,
}

/// The scrolling middle of the card.
#[derive(Component, Debug, Clone, Copy)]
pub(super) struct PlanBody;

/// On a freshly drawn checkbox: the one that held keyboard focus before the redraw.
#[derive(Component)]
pub(super) struct Refocus;

/// What the tab and the model ask of the modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Work {
    Nothing,
    Open,
    OpenWanted,
    Close,
}

const COMMAND_KEYS: [(&str, &str); 4] = [
    ("all", "All"),
    ("one", "One test"),
    ("some", "Some tests"),
    ("list", "List"),
];

pub(super) fn plan_view(
    pr: &PrRef,
    model: &TestsModel,
    agent: &str,
    can_edit: bool,
) -> Option<PlanView> {
    let view = model.view.as_ref()?;
    let plan = view.plan.as_ref()?;
    let modal = model.plan.as_ref()?;
    let steps = plan
        .steps
        .iter()
        .map(|s| StepRow {
            id: s.id.clone(),
            title: printable(&s.title),
            command: printable_lines(&s.command),
            place: s.place,
            network: s.network,
            hint: s.hint.as_deref().map(printable),
            kind: row_kind(s),
            checked: modal.checked.contains(&s.id),
        })
        .collect();
    let commands = COMMAND_KEYS
        .iter()
        .filter_map(|(key, label)| {
            let c = plan.commands.iter().find(|c| c.key == *key)?;
            Some((*label, printable_lines(&c.command)))
        })
        .collect();
    let recipe = view.recipe.as_ref();
    let variables = recipe.filter(|r| !r.env.is_empty()).map(|r| {
        r.env
            .iter()
            .map(|(k, v)| format!("{}={}", printable(k), printable(v)))
            .collect::<Vec<_>>()
            .join("  ")
    });
    let stack_note = view
        .env
        .as_ref()
        .filter(|e| !e.existing.reviewer_services.is_empty())
        .map(|_| {
            format!(
                "Your running {} stack is up and will not be used",
                printable(&pr.repo)
            )
        });
    Some(PlanView {
        number: pr.number,
        agent: agent.to_string(),
        by_hand: recipe.is_some_and(|r| r.by_hand),
        recipe_hash: plan.recipe_hash.clone(),
        blocked: plan.blocked.as_deref().map(printable_lines),
        steps,
        commands,
        variables,
        stack_note,
        commands_approved: plan.commands_approved,
        changed_note: modal.changed_note,
        sending: modal.sending,
        can_edit,
    })
}

fn row_kind(step: &PlanStep) -> RowKind {
    match &step.state {
        StepState::Missing | StepState::Failed { .. } => RowKind::Approvable,
        StepState::Running => RowKind::Running,
        StepState::Done => RowKind::Done,
        StepState::Skipped { why } => RowKind::Skipped(printable(why)),
    }
}

/// The steps an approval names: the approvable rows that were drawn (and, unless `all`, drawn
/// checked), then the test commands. A step the modal never drew is not in the list, and
/// there is no way to ask for `Approve::All`.
pub(super) fn approval(view: &PlanView, all: bool) -> Approve {
    let mut ids: Vec<String> = view
        .approvable()
        .filter(|s| all || s.checked)
        .map(|s| s.id.clone())
        .collect();
    ids.push(STEP_TESTS.to_string());
    Approve::Steps(ids)
}

pub(super) fn plan_action(
    active_on_tests: bool,
    modal: Option<Modal>,
    model: Option<&TestsModel>,
) -> Work {
    let state = model.and_then(TestsModel::state);
    let has_plan = model.is_some_and(|m| m.plan.is_some());
    match modal {
        Some(Modal::TestSetup) => {
            if has_plan && matches!(state, Some(TabState::NeedsSetup | TabState::Refused { .. })) {
                Work::Nothing
            } else {
                Work::Close
            }
        }
        Some(_) => Work::Nothing,
        None => {
            let Some(model) = model else {
                return Work::Nothing;
            };
            if state != Some(TabState::NeedsSetup) {
                return if has_plan || model.plan_wanted {
                    Work::Close
                } else {
                    Work::Nothing
                };
            }
            if !active_on_tests {
                Work::Nothing
            } else if has_plan {
                Work::Open
            } else if model.plan_wanted {
                Work::OpenWanted
            } else {
                Work::Nothing
            }
        }
    }
}

fn active_on_tests(nav: &Nav, pr: &PrRef, tab: &crate::review_state::Tab) -> bool {
    nav.screen == Screen::Review(pr.clone())
        && tab.ready().is_some()
        && tab.ui.section == ReviewSection::Tests
}

/// Only reads, so a window with nothing to open or close never wakes `open_plans`.
pub(super) fn plan_work(nav: Res<Nav>, tabs: Res<ReviewTabs>, models: Res<TestsModels>) -> bool {
    (nav.is_changed() || tabs.is_changed() || models.is_changed())
        && tabs.0.iter().any(|(pr, tab)| {
            plan_action(
                active_on_tests(&nav, pr, tab),
                tab.ui.modal,
                models.0.get(pr),
            ) != Work::Nothing
        })
}

pub(super) fn open_plans(
    nav: Res<Nav>,
    mut tabs: ResMut<ReviewTabs>,
    mut models: ResMut<TestsModels>,
) {
    let work: Vec<(PrRef, Work)> = tabs
        .0
        .iter()
        .map(|(pr, tab)| {
            let action = plan_action(
                active_on_tests(&nav, pr, tab),
                tab.ui.modal,
                models.0.get(pr),
            );
            (pr.clone(), action)
        })
        .filter(|(_, w)| *w != Work::Nothing)
        .collect();
    for (pr, work) in work {
        match work {
            Work::Open => set_modal(&mut tabs, &pr),
            Work::OpenWanted => {
                if let Some(model) = models.0.get_mut(&pr) {
                    model.open_plan();
                    if model.plan.is_some() {
                        set_modal(&mut tabs, &pr);
                    }
                }
            }
            Work::Close => close(&pr, &mut tabs, &mut models),
            Work::Nothing => {}
        }
    }
}

fn set_modal(tabs: &mut ReviewTabs, pr: &PrRef) {
    if let Some(tab) = tabs.0.get_mut(pr) {
        tab.ui.modal = Some(Modal::TestSetup);
    }
}

/// Forgets the plan and its wish to open, and takes the modal off the tab.
fn close(pr: &PrRef, tabs: &mut ReviewTabs, models: &mut TestsModels) {
    if let Some(model) = models.0.get_mut(pr) {
        model.plan = None;
        model.plan_wanted = false;
        model.touch();
    }
    if let Some(tab) = tabs.0.get_mut(pr)
        && tab.ui.modal == Some(Modal::TestSetup)
    {
        tab.ui.modal = None;
    }
}

pub(super) fn fill_plan(
    mut commands: Commands,
    nav: Res<Nav>,
    tabs: Res<ReviewTabs>,
    models: Res<TestsModels>,
    app_model: Res<Model>,
    fonts: Res<UiFonts>,
    roots: Query<(Entity, &ModalFor, Option<&PlanBuilt>)>,
    focus: Res<InputFocus>,
    boxes: Query<&PlanStepBox>,
) {
    let Screen::Review(pr) = &nav.screen else {
        return;
    };
    let Some(tab) = tabs.0.get(pr) else { return };
    let Some(ready) = tab.ready() else { return };
    if tab.ui.modal != Some(Modal::TestSetup) {
        return;
    }
    let Some(model) = models.0.get(pr) else {
        return;
    };
    let agent = agent_name::of_tab(&app_model.snapshot, tab);
    let Some(view) = plan_view(pr, model, &agent, ready.view.worktree.is_some()) else {
        return;
    };
    let root = roots
        .iter()
        .find(|(_, m, _)| &m.pr == pr && m.modal == Modal::TestSetup);
    match root {
        None => {
            let mut card = Entity::PLACEHOLDER;
            let mut slots = None;
            let root = commands
                .spawn((
                    modal_root(),
                    ModalFor {
                        pr: pr.clone(),
                        modal: Modal::TestSetup,
                    },
                ))
                .with_children(|r| {
                    let mut c = r.spawn(modal_card(820.0));
                    card = c.id();
                    c.with_children(|c| slots = Some(skeleton(c)));
                })
                .id();
            let slots = slots.expect("the card was spawned");
            fill_slots(&mut commands, &fonts, pr, &view, slots, None);
            commands
                .entity(root)
                .insert(PlanBuilt { view, card, slots });
        }
        Some((_, _, Some(built))) if built.view == view => {}
        Some((root, _, Some(built))) => {
            let (card, slots) = (built.card, built.slots);
            let focused = focus
                .get()
                .and_then(|e| boxes.get(e).ok())
                .map(|b| b.id.clone());
            for slot in [slots.head, slots.body, slots.foot] {
                commands.entity(slot).despawn_related::<Children>();
            }
            fill_slots(&mut commands, &fonts, pr, &view, slots, focused);
            commands
                .entity(root)
                .insert(PlanBuilt { view, card, slots });
        }
        // Spawned this frame: its content is queued already.
        Some((_, _, None)) => {}
    }
}

/// Gives keyboard focus back to a checkbox drawn again.
pub(super) fn refocus(
    add: On<Add, Refocus>,
    mut focus: ResMut<InputFocus>,
    mut commands: Commands,
) {
    focus.set(add.entity, FocusCause::Navigated);
    commands.entity(add.entity).remove::<Refocus>();
}

type Spawner<'a> = ChildSpawnerCommands<'a>;

const SECTION: Type = Type {
    size: 11.0,
    weight: 600,
    ink: Swatch::Muted,
    mono: false,
};

fn skeleton(card: &mut Spawner) -> Slots {
    let head = card
        .spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            padding: UiRect {
                left: px(24),
                right: px(24),
                top: px(22),
                bottom: px(10),
            },
            ..default()
        })
        .id();
    let body = card
        .spawn((
            Node {
                flex_direction: FlexDirection::Column,
                flex_grow: 1.0,
                min_height: px(0),
                padding: UiRect::axes(px(24), px(4)),
                row_gap: px(6),
                overflow: Overflow::scroll_y(),
                ..default()
            },
            ScrollArea,
            ScrollPosition::default(),
            PlanBody,
        ))
        .id();
    let foot = card
        .spawn(Node {
            flex_direction: FlexDirection::Column,
            ..default()
        })
        .id();
    Slots { head, body, foot }
}

fn fill_slots(
    commands: &mut Commands,
    fonts: &UiFonts,
    pr: &PrRef,
    view: &PlanView,
    slots: Slots,
    focused: Option<String>,
) {
    commands.entity(slots.head).with_children(|head| {
        head.spawn(text(
            fonts,
            format!("Set up tests for #{}", view.number),
            Type::HEADING.size(17.0),
        ));
        let who = if view.by_hand {
            "You edited this recipe.".to_string()
        } else {
            format!("{} read the repository and wrote this recipe.", view.agent)
        };
        head.spawn(wrapped_text(
            fonts,
            format!(
                "{who} Nothing runs until you approve it. Steps run once; the results are reused next time."
            ),
            Type::MUTED,
        ));
    });
    commands
        .entity(slots.body)
        .with_children(|body| draw_body(body, fonts, pr, view, focused.as_deref()));
    commands.entity(slots.foot).with_children(|foot| {
        if view.changed_note || view.stack_note.is_some() {
            foot.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: px(4),
                padding: UiRect::axes(px(24), px(8)),
                ..default()
            })
            .with_children(|notes| {
                if view.changed_note {
                    notes.spawn(wrapped_text(
                        fonts,
                        "The recipe changed. Look at the plan again before you approve.",
                        Type::BODY.ink(Swatch::Orange),
                    ));
                }
                if let Some(note) = &view.stack_note {
                    notes.spawn(wrapped_text(fonts, note.clone(), Type::META));
                }
            });
        }
        draw_buttons(foot, fonts, pr, view);
    });
}

fn draw_body(
    body: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    view: &PlanView,
    focused: Option<&str>,
) {
    if let Some(reason) = &view.blocked {
        body.spawn(wrapped_text(
            fonts,
            reason.clone(),
            Type::BODY.ink(Swatch::Orange),
        ));
    } else {
        let n = view.approvable().count();
        let steps = if n == 1 { "STEP" } else { "STEPS" };
        body.spawn(text(fonts, format!("SETUP · {n} {steps}"), SECTION));
        for row in &view.steps {
            draw_step(body, fonts, pr, row, focused == Some(row.id.as_str()));
        }
    }
    body.spawn(Node {
        margin: UiRect::top(px(10)),
        ..default()
    })
    .with_child(text(fonts, "TEST COMMANDS · NO NETWORK", SECTION));
    for (label, command) in &view.commands {
        labelled(body, fonts, label, |c| {
            c.spawn(wrapped_text(
                fonts,
                command.clone(),
                Type::MONO.ink(Swatch::Fg),
            ));
        });
    }
    if let Some(vars) = &view.variables {
        labelled(body, fonts, "Variables", |c| {
            c.spawn(wrapped_text(
                fonts,
                vars.clone(),
                Type::MONO.ink(Swatch::Fg),
            ));
            c.spawn(text(fonts, "(fake · edit in the recipe)", Type::META));
        });
    }
}

fn labelled(body: &mut Spawner, fonts: &UiFonts, label: &str, content: impl FnOnce(&mut Spawner)) {
    body.spawn(Node {
        width: percent(100),
        column_gap: px(16),
        padding: UiRect::axes(px(0), px(4)),
        align_items: AlignItems::FlexStart,
        ..default()
    })
    .with_children(|row| {
        row.spawn(Node {
            width: px(90),
            flex_shrink: 0.0,
            ..default()
        })
        .with_child(text(fonts, label.to_string(), Type::MUTED));
        row.spawn(Node {
            flex_direction: FlexDirection::Column,
            flex_grow: 1.0,
            flex_basis: px(0),
            min_width: px(0),
            row_gap: px(3),
            ..default()
        })
        .with_children(content);
    });
}

fn draw_step(body: &mut Spawner, fonts: &UiFonts, pr: &PrRef, row: &StepRow, focused: bool) {
    let approvable = row.kind == RowKind::Approvable;
    let host = approvable && row.place == StepPlace::Host;
    let title_ink = if approvable {
        Swatch::Fg
    } else {
        Swatch::Muted
    };
    let mut e = body.spawn((
        Node {
            width: percent(100),
            column_gap: px(16),
            padding: UiRect::axes(px(0), px(9)),
            border: UiRect::bottom(px(1)),
            align_items: AlignItems::FlexStart,
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
    ));
    if host {
        e.insert((BackgroundColor::default(), Fill(Swatch::HostRow)));
    }
    e.with_children(|r| {
        if approvable {
            let mut b = r.spawn((
                checkbox(row.checked),
                PlanStepBox {
                    pr: pr.clone(),
                    id: row.id.clone(),
                },
                observe(on_toggle),
            ));
            if focused {
                b.insert(Refocus);
            }
        } else {
            r.spawn(Node {
                width: px(18),
                flex_shrink: 0.0,
                ..default()
            });
        }
        r.spawn(Node {
            flex_direction: FlexDirection::Column,
            flex_basis: percent(34),
            flex_shrink: 0.0,
            row_gap: px(2),
            ..default()
        })
        .with_children(|t| {
            t.spawn(wrapped_text(
                fonts,
                row.title.clone(),
                Type::BODY.ink(title_ink),
            ));
            if host {
                t.spawn(wrapped_text(
                    fonts,
                    host_line(&row.id),
                    Type::META.ink(Swatch::HostInk),
                ));
            }
            if let RowKind::Skipped(why) = &row.kind {
                t.spawn(wrapped_text(fonts, format!("skipped · {why}"), Type::META));
            }
        });
        let skipped = matches!(row.kind, RowKind::Skipped(_));
        r.spawn(Node {
            flex_direction: FlexDirection::Column,
            flex_grow: 1.0,
            flex_basis: px(0),
            min_width: px(0),
            row_gap: px(3),
            ..default()
        })
        .with_children(|c| {
            if skipped {
                return;
            }
            let ink = if approvable {
                Swatch::Fg
            } else {
                Swatch::Muted
            };
            c.spawn(wrapped_text(
                fonts,
                row.command.clone(),
                Type::MONO.ink(ink),
            ));
            if let Some(hint) = &row.hint {
                c.spawn(wrapped_text(fonts, hint.clone(), Type::META));
            }
        });
        r.spawn(Node {
            column_gap: px(6),
            flex_shrink: 0.0,
            ..default()
        })
        .with_children(|tags| match &row.kind {
            RowKind::Approvable => {
                let (label, tone) = match row.place {
                    StepPlace::Container => ("container", Tone::Neutral),
                    StepPlace::LocalSandbox => ("sandbox", Tone::Neutral),
                    StepPlace::Host => ("host", Tone::Amber),
                };
                tags.spawn(badge(fonts, label, tone));
                if row.network {
                    tags.spawn(badge(fonts, "network", Tone::Blue));
                }
            }
            RowKind::Running => {
                tags.spawn(badge(fonts, "running", Tone::Neutral));
            }
            RowKind::Done => {
                tags.spawn(badge(fonts, "done", Tone::Neutral));
            }
            RowKind::Skipped(_) => {}
        });
    });
}

fn host_line(id: &str) -> &'static str {
    match id {
        STEP_STACK => "Runs on your Mac: no host folders, no open ports, internal network",
        STEP_ENGINE => "Runs on your Mac: starts your container engine",
        _ => "Runs on your Mac",
    }
}

fn draw_buttons(card: &mut Spawner, fonts: &UiFonts, pr: &PrRef, view: &PlanView) {
    card.spawn(Node {
        width: percent(100),
        column_gap: px(10),
        padding: UiRect::axes(px(24), px(14)),
        align_items: AlignItems::Center,
        border: UiRect::top(px(1)),
        ..default()
    })
    .insert((BorderColor::default(), Stroke(Swatch::Line)))
    .with_children(|row| {
        if view.can_edit {
            row.spawn((
                button(fonts, "Edit recipe", Variant::Link),
                PlanEditRecipe(pr.clone()),
                EditRecipe(pr.clone()),
                observe(on_edit_recipe),
                observe(on_plan_edit_recipe),
            ));
        }
        row.spawn(Node {
            flex_grow: 1.0,
            ..default()
        });
        row.spawn((
            button(fonts, "Cancel", Variant::Secondary),
            CancelPlan(pr.clone()),
            observe(on_cancel),
        ));
        if view.offers_selected() {
            row.spawn((
                button(fonts, "Approve selected", Variant::Secondary),
                ApproveSelected(pr.clone()),
                observe(on_approve_selected),
            ));
        } else {
            row.spawn(disabled_button(fonts, "Approve selected"));
        }
        if view.offers_all() {
            row.spawn((
                button(fonts, "Approve all and set up", Variant::Primary),
                ApproveAll(pr.clone()),
                observe(on_approve_all),
            ));
        } else {
            row.spawn(disabled_button(fonts, "Approve all and set up"));
        }
    });
}

pub(super) fn modal_up(roots: Query<&ModalFor>) -> bool {
    roots.iter().any(|m| m.modal == Modal::TestSetup)
}

pub(super) fn cancel_plan(
    keys: Res<ButtonInput<KeyCode>>,
    mut clicks: MessageReader<BackdropClicked>,
    roots: Query<&ModalFor>,
    nav: Res<Nav>,
    mut tabs: ResMut<ReviewTabs>,
    mut models: ResMut<TestsModels>,
) {
    let outside = clicks
        .read()
        .any(|c| roots.get(c.0).is_ok_and(|m| m.modal == Modal::TestSetup));
    if !(escape_pressed(&keys) || outside) {
        return;
    }
    if let Screen::Review(pr) = &nav.screen {
        close(pr, &mut tabs, &mut models);
    }
}

fn on_toggle(activate: On<Activate>, boxes: Query<&PlanStepBox>, mut models: ResMut<TestsModels>) {
    let Ok(PlanStepBox { pr, id }) = boxes.get(activate.entity) else {
        return;
    };
    if let Some(model) = models.0.get_mut(pr)
        && let Some(modal) = model.plan.as_mut()
    {
        if !modal.checked.remove(id) {
            modal.checked.insert(id.clone());
        }
        model.touch();
    }
}

/// The approval a click asks for, from the plan the modal drew. `None` with a toast when the
/// review is read-only, and `None` when nothing may be sent.
fn approval_ask(
    pr: &PrRef,
    all: bool,
    roots: &Query<(&ModalFor, &PlanBuilt)>,
    tabs: &ReviewTabs,
    app_model: &Model,
    toasts: &mut Toasts,
    time: &Time,
) -> Option<Ask> {
    if let Some(reason) = read_only_reason(tabs, app_model, pr) {
        not_sent(toasts, time, reason);
        return None;
    }
    let (_, built) = roots
        .iter()
        .find(|(m, _)| &m.pr == pr && m.modal == Modal::TestSetup)?;
    let offered = if all {
        built.view.offers_all()
    } else {
        built.view.offers_selected()
    };
    offered.then(|| Ask::PrepareTests {
        pr: pr.clone(),
        recipe_hash: built.view.recipe_hash.clone(),
        approve: approval(&built.view, all),
    })
}

#[allow(clippy::too_many_arguments)]
fn on_approve_selected(
    activate: On<Activate>,
    buttons: Query<&ApproveSelected>,
    roots: Query<(&ModalFor, &PlanBuilt)>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<TestsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(ApproveSelected(pr)) = buttons.get(activate.entity) else {
        return;
    };
    send_approval(
        pr,
        false,
        &roots,
        &tabs,
        &app_model,
        &mut models,
        &mut asks,
        &mut toasts,
        &time,
    );
}

#[allow(clippy::too_many_arguments)]
fn on_approve_all(
    activate: On<Activate>,
    buttons: Query<&ApproveAll>,
    roots: Query<(&ModalFor, &PlanBuilt)>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<TestsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(ApproveAll(pr)) = buttons.get(activate.entity) else {
        return;
    };
    send_approval(
        pr,
        true,
        &roots,
        &tabs,
        &app_model,
        &mut models,
        &mut asks,
        &mut toasts,
        &time,
    );
}

#[allow(clippy::too_many_arguments)]
fn send_approval(
    pr: &PrRef,
    all: bool,
    roots: &Query<(&ModalFor, &PlanBuilt)>,
    tabs: &ReviewTabs,
    app_model: &Model,
    models: &mut TestsModels,
    asks: &mut Asks,
    toasts: &mut Toasts,
    time: &Time,
) {
    // The drawn view still offers the button until the next redraw: a second activation in
    // the same frame must not send again.
    if models
        .0
        .get(pr)
        .is_some_and(|m| m.pending.is_some() || m.plan.as_ref().is_some_and(|p| p.sending))
    {
        return;
    }
    let Some(ask) = approval_ask(pr, all, roots, tabs, app_model, toasts, time) else {
        return;
    };
    asks.send(ask);
    if let Some(model) = models.0.get_mut(pr) {
        model.sent(TestsAskKind::Prepare);
    }
}

fn on_cancel(
    activate: On<Activate>,
    buttons: Query<&CancelPlan>,
    mut tabs: ResMut<ReviewTabs>,
    mut models: ResMut<TestsModels>,
) {
    if let Ok(CancelPlan(pr)) = buttons.get(activate.entity) {
        close(pr, &mut tabs, &mut models);
    }
}

fn on_plan_edit_recipe(
    activate: On<Activate>,
    buttons: Query<&PlanEditRecipe>,
    mut tabs: ResMut<ReviewTabs>,
    mut models: ResMut<TestsModels>,
) {
    if let Ok(PlanEditRecipe(pr)) = buttons.get(activate.entity) {
        close(pr, &mut tabs, &mut models);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Connection, Tell, TestsTell, Toasts};
    use crate::fixture;
    use crate::review_state::{Modal, ReviewSection, ReviewTabs};
    use crate::screens::review::tests_tab::top::SetUpButton;
    use crate::testing::{self, NOW};
    use crate::theme::Swatch;
    use crate::ui::kit::Fill;
    use bevy::input_focus::{FocusCause, InputFocus};
    use bevy::ui_widgets::Activate;
    use clusia_core::tests::{
        PlanCommand, PlanStep, RECIPE_CHANGED, RECIPE_FILE, StepPlace, StepState, TestsPhase,
        TestsView,
    };

    const WORKTREE: &str = "~/.clusia/worktrees/rzorzal/clusia/123";

    fn tests_app() -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        (app, pr)
    }

    fn step(
        id: &str,
        title: &str,
        command: &str,
        place: StepPlace,
        network: bool,
        state: StepState,
    ) -> PlanStep {
        PlanStep {
            id: id.into(),
            title: title.into(),
            command: command.into(),
            place,
            network,
            hint: None,
            state,
            approved: false,
        }
    }

    /// Two container steps to approve, one done, and a host step that starts the repo's own
    /// postgres.
    fn base_steps() -> Vec<PlanStep> {
        vec![
            step(
                "image",
                "Build the image",
                "docker build --target test -t clusia/rzorzal-clusia:4f2a91c .",
                StepPlace::Container,
                true,
                StepState::Missing,
            ),
            step(
                "work:0",
                "Install dependencies",
                "cargo fetch",
                StepPlace::Container,
                true,
                StepState::Missing,
            ),
            step(
                "services",
                "Pull the postgres service",
                "docker compose pull postgres",
                StepPlace::Container,
                true,
                StepState::Done,
            ),
            step(
                "stack",
                "Start Clúsia's own postgres",
                "docker compose -p clusia-rzorzal-clusia-123 up -d postgres",
                StepPlace::Host,
                false,
                StepState::Missing,
            ),
        ]
    }

    fn needs_setup(app: &App, pr: &PrRef, hash: &str, steps: Vec<PlanStep>) -> TestsView {
        let mut view = testing::tests_view(app, pr, TestsPhase::NeedsSetup);
        view.env = Some(testing::tests_env(true));
        let mut recipe = testing::tests_recipe();
        recipe.hash = hash.into();
        recipe.env = [
            ("API_KEY".to_string(), "fake_k3y_9a1".to_string()),
            (
                "DATABASE_URL".to_string(),
                "postgres://test:test@postgres/shop".to_string(),
            ),
        ]
        .into_iter()
        .collect();
        view.recipe = Some(recipe);
        let mut plan = testing::tests_plan(steps, false);
        plan.recipe_hash = hash.into();
        plan.commands = vec![
            PlanCommand {
                key: "all".into(),
                command: "cargo test".into(),
            },
            PlanCommand {
                key: "one".into(),
                command: "cargo test -- --exact {name}".into(),
            },
            PlanCommand {
                key: "list".into(),
                command: "cargo test -- --list".into(),
            },
        ];
        view.plan = Some(plan);
        view
    }

    /// The review on Tests with `edit` applied to the needs-setup view, not yet opened.
    fn setup_app(edit: impl FnOnce(&mut TestsView)) -> (App, PrRef) {
        let (mut app, pr) = tests_app();
        let mut view = needs_setup(&app, &pr, "h1", base_steps());
        edit(&mut view);
        testing::open_tests(&mut app, &pr, view);
        testing::recorded(&mut app);
        (app, pr)
    }

    /// Clicks **Set up…** and runs frames: the modal is open.
    fn open_modal(app: &mut App, pr: &PrRef) {
        let set_up = testing::find::<SetUpButton>(app, |b| &b.0 == pr);
        testing::activate(app, set_up);
        testing::settle(app);
        testing::recorded(app);
    }

    fn modal_app() -> (App, PrRef) {
        let (mut app, pr) = setup_app(|_| {});
        open_modal(&mut app, &pr);
        (app, pr)
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

    fn modals(app: &mut App) -> usize {
        testing::count::<ModalFor>(app)
    }

    fn model<'a>(app: &'a App, pr: &PrRef) -> &'a TestsModel {
        &app.world().resource::<TestsModels>().0[pr]
    }

    fn ui_modal(app: &App, pr: &PrRef) -> Option<Modal> {
        app.world().resource::<ReviewTabs>().0[pr].ui.modal
    }

    fn prepare(pr: &PrRef, hash: &str, ids: &[&str]) -> Ask {
        Ask::PrepareTests {
            pr: pr.clone(),
            recipe_hash: hash.into(),
            approve: Approve::Steps(ids.iter().map(|s| s.to_string()).collect()),
        }
    }

    fn toggle(app: &mut App, pr: &PrRef, id: &str) {
        let box_ = testing::find::<PlanStepBox>(app, |b| &b.pr == pr && b.id == id);
        testing::activate(app, box_);
        testing::settle(app);
    }

    #[test]
    fn set_up_opens_the_modal_with_the_plan() {
        let (mut app, pr) = setup_app(|_| {});
        assert_eq!(modals(&mut app), 0, "nothing opens before a click");
        let set_up = testing::find::<SetUpButton>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, set_up);
        testing::settle(&mut app);
        assert_eq!(modals(&mut app), 1);
        assert_eq!(ui_modal(&app, &pr), Some(Modal::TestSetup));
        assert!(testing::shows(&mut app, "Set up tests for #123"));
        assert!(testing::shows(
            &mut app,
            "Claude Code read the repository and wrote this recipe. Nothing runs until you approve it. Steps run once; the results are reused next time."
        ));
        assert!(testing::shows(&mut app, "SETUP · 3 STEPS"));
        assert!(testing::shows(&mut app, "TEST COMMANDS · NO NETWORK"));
        for label in ["All", "One test", "List"] {
            assert!(testing::shows(&mut app, label), "{label}");
        }
        assert!(
            !testing::shows(&mut app, "Some tests"),
            "the plan has no `some`"
        );
        assert!(testing::shows(&mut app, "cargo test -- --exact {name}"));
        assert!(testing::shows(&mut app, "cargo test -- --list"));
        for label in ["Cancel", "Approve selected", "Approve all and set up"] {
            assert!(testing::shows(&mut app, label), "{label}");
        }
        assert!(
            testing::recorded(&mut app).is_empty(),
            "opening asks nothing"
        );
    }

    #[test]
    fn rows_show_the_places_the_network_and_the_done_steps() {
        let (mut app, pr) = modal_app();
        for title in [
            "Build the image",
            "Install dependencies",
            "Pull the postgres service",
            "Start Clúsia's own postgres",
        ] {
            assert!(testing::shows(&mut app, title), "{title}");
        }
        assert!(testing::shows(
            &mut app,
            "docker compose -p clusia-rzorzal-clusia-123 up -d postgres"
        ));
        assert_eq!(
            testing::count::<PlanStepBox>(&mut app),
            3,
            "the done step has no checkbox"
        );
        for id in ["image", "work:0", "stack"] {
            let _ = testing::find::<PlanStepBox>(&mut app, |b| b.pr == pr && b.id == id);
        }
        assert!(testing::shows(&mut app, "container"));
        assert!(testing::shows(&mut app, "network"));
        assert!(testing::shows(&mut app, "host"));
        assert!(testing::shows(&mut app, "done"));
        assert!(testing::shows(
            &mut app,
            "Runs on your Mac: no host folders, no open ports, internal network"
        ));
        let host_rows = testing::entities::<Fill>(&mut app)
            .into_iter()
            .filter(|e| app.world().get::<Fill>(*e).unwrap().0 == Swatch::HostRow)
            .count();
        assert_eq!(host_rows, 1, "one host row, filled");
        let amber = testing::entities::<Fill>(&mut app)
            .into_iter()
            .filter(|e| app.world().get::<Fill>(*e).unwrap().0 == Swatch::AmberSoft)
            .count();
        let blue = testing::entities::<Fill>(&mut app)
            .into_iter()
            .filter(|e| app.world().get::<Fill>(*e).unwrap().0 == Swatch::NetBlueSoft)
            .count();
        assert_eq!((amber, blue), (1, 2), "one host tag, two network tags");
    }

    #[test]
    fn an_engine_step_and_a_skipped_step_say_what_they_are() {
        let (mut app, pr) = tests_app();
        let mut steps = vec![
            step(
                "engine",
                "Start Colima",
                "colima start",
                StepPlace::Host,
                false,
                StepState::Missing,
            ),
            step(
                "services",
                "Pull the postgres service",
                "docker compose pull postgres",
                StepPlace::Container,
                true,
                StepState::Skipped {
                    why: "the recipe has no services".into(),
                },
            ),
        ];
        steps[0].hint = Some("takes about a minute".into());
        let view = needs_setup(&app, &pr, "h1", steps);
        testing::open_tests(&mut app, &pr, view);
        open_modal(&mut app, &pr);
        assert!(testing::shows(
            &mut app,
            "Runs on your Mac: starts your container engine"
        ));
        assert!(testing::shows(&mut app, "takes about a minute"));
        assert!(testing::shows(
            &mut app,
            "skipped · the recipe has no services"
        ));
        assert!(testing::shows(&mut app, "SETUP · 1 STEP"));
        assert_eq!(testing::count::<PlanStepBox>(&mut app), 1);
    }

    #[test]
    fn variables_show_on_one_line_as_fake_pairs() {
        let (mut app, _) = modal_app();
        assert!(testing::shows(&mut app, "Variables"));
        assert!(testing::shows(
            &mut app,
            "API_KEY=fake_k3y_9a1  DATABASE_URL=postgres://test:test@postgres/shop"
        ));
        assert!(testing::shows(&mut app, "(fake · edit in the recipe)"));
        let (mut app, pr) = tests_app();
        let mut view = needs_setup(&app, &pr, "h1", base_steps());
        view.recipe.as_mut().unwrap().env.clear();
        testing::open_tests(&mut app, &pr, view);
        open_modal(&mut app, &pr);
        assert!(!testing::shows(&mut app, "Variables"));
    }

    #[test]
    fn the_footer_names_a_running_stack_and_there_is_no_size() {
        let (mut app, pr) = tests_app();
        let mut view = needs_setup(&app, &pr, "h1", base_steps());
        view.env.as_mut().unwrap().existing.reviewer_services = vec!["clusia-db".into()];
        testing::open_tests(&mut app, &pr, view);
        open_modal(&mut app, &pr);
        assert!(testing::shows(
            &mut app,
            "Your running clusia stack is up and will not be used"
        ));
        assert!(!testing::shows(&mut app, "to download"));
    }

    #[test]
    fn a_hand_edited_recipe_says_so() {
        let (mut app, pr) = tests_app();
        let mut view = needs_setup(&app, &pr, "h1", base_steps());
        view.recipe.as_mut().unwrap().by_hand = true;
        testing::open_tests(&mut app, &pr, view);
        open_modal(&mut app, &pr);
        assert!(testing::shows(
            &mut app,
            "You edited this recipe. Nothing runs until you approve it. Steps run once; the results are reused next time."
        ));
        assert!(!testing::shows(
            &mut app,
            "read the repository and wrote this recipe"
        ));
    }

    #[test]
    fn a_plan_that_gets_blocked_shows_its_reason_and_draws_no_approve_button() {
        let (mut app, pr) = modal_app();
        let mut view = needs_setup(&app, &pr, "h1", base_steps());
        view.plan.as_mut().unwrap().blocked =
            Some("Services need a container engine and none is running".into());
        testing::open_tests(&mut app, &pr, view);
        assert_eq!(modals(&mut app), 1, "a refused state keeps the modal");
        assert!(testing::shows(
            &mut app,
            "Services need a container engine and none is running"
        ));
        assert!(!testing::shows(&mut app, "SETUP ·"));
        assert_eq!(testing::count::<PlanStepBox>(&mut app), 0);
        assert_eq!(testing::count::<ApproveSelected>(&mut app), 0);
        assert_eq!(testing::count::<ApproveAll>(&mut app), 0);
        assert!(
            testing::shows(&mut app, "Approve all and set up"),
            "drawn, disabled"
        );
        assert_eq!(testing::count::<CancelPlan>(&mut app), 1);
    }

    #[test]
    fn approve_selected_sends_the_shown_hash_and_only_checked_steps() {
        let (mut app, pr) = modal_app();
        toggle(&mut app, &pr, "work:0");
        let approve = testing::find::<ApproveSelected>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, approve);
        assert_eq!(
            testing::recorded(&mut app),
            [prepare(&pr, "h1", &["image", "stack", "tests"])]
        );
        let m = model(&app, &pr);
        assert_eq!(m.pending, Some(TestsAskKind::Prepare));
        assert!(m.prepared_here);
        assert!(m.plan.as_ref().is_some_and(|p| p.sending));
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<ApproveSelected>(&mut app),
            0,
            "once, not twice"
        );
        assert_eq!(testing::count::<ApproveAll>(&mut app), 0);
    }

    #[test]
    fn approve_all_sends_the_drawn_steps_never_all() {
        let (mut app, pr) = modal_app();
        toggle(&mut app, &pr, "work:0");
        let all = testing::find::<ApproveAll>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, all);
        let asked = testing::recorded(&mut app);
        assert_eq!(
            asked,
            [prepare(&pr, "h1", &["image", "work:0", "stack", "tests"])],
            "every drawn step to approve, checked or not; the done one is not asked again"
        );
        assert!(!asked.iter().any(|a| matches!(
            a,
            Ask::PrepareTests {
                approve: Approve::All,
                ..
            }
        )));
    }

    #[test]
    fn a_step_that_appears_later_is_not_approved() {
        let (mut app, pr) = modal_app();
        // The engine stops meanwhile: the plan gains a step the modal has not drawn yet.
        let mut steps = base_steps();
        steps.insert(
            0,
            step(
                "engine",
                "Start Colima",
                "colima start",
                StepPlace::Host,
                false,
                StepState::Missing,
            ),
        );
        let view = needs_setup(&app, &pr, "h1", steps);
        app.world_mut()
            .resource_mut::<TestsModels>()
            .apply(&TestsTell::Loaded {
                pr: pr.clone(),
                view: Box::new(view),
            });
        // The click comes before the modal is drawn again.
        let all = testing::find::<ApproveAll>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, all);
        assert_eq!(
            testing::recorded(&mut app),
            [prepare(&pr, "h1", &["image", "work:0", "stack", "tests"])],
            "the engine was never shown, so it is not approved"
        );
    }

    #[test]
    fn a_step_that_comes_in_is_drawn_unchecked_and_left_out_of_approve_selected() {
        let (mut app, pr) = modal_app();
        let mut steps = base_steps();
        steps.insert(
            0,
            step(
                "engine",
                "Start Colima",
                "colima start",
                StepPlace::Host,
                false,
                StepState::Missing,
            ),
        );
        let view = needs_setup(&app, &pr, "h1", steps);
        testing::open_tests(&mut app, &pr, view);
        assert!(testing::shows(&mut app, "Start Colima"), "drawn now");
        assert_eq!(testing::count::<PlanStepBox>(&mut app), 4);
        let approve = testing::find::<ApproveSelected>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, approve);
        assert_eq!(
            testing::recorded(&mut app),
            [prepare(&pr, "h1", &["image", "work:0", "stack", "tests"])],
            "what the reviewer checked, not the step that came in"
        );
    }

    #[test]
    fn the_commands_are_approved_even_when_no_step_is() {
        let (mut app, pr) = modal_app();
        for id in ["image", "work:0", "stack"] {
            toggle(&mut app, &pr, id);
        }
        let approve = testing::find::<ApproveSelected>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, approve);
        assert_eq!(
            testing::recorded(&mut app),
            [prepare(&pr, "h1", &["tests"])]
        );
    }

    #[test]
    fn approve_selected_is_not_a_button_when_nothing_is_left_to_approve() {
        let (mut app, pr) = tests_app();
        let mut view = needs_setup(&app, &pr, "h1", base_steps());
        view.plan.as_mut().unwrap().commands_approved = true;
        testing::open_tests(&mut app, &pr, view);
        open_modal(&mut app, &pr);
        assert_eq!(testing::count::<ApproveSelected>(&mut app), 1);
        for id in ["image", "work:0", "stack"] {
            toggle(&mut app, &pr, id);
        }
        assert_eq!(testing::count::<ApproveSelected>(&mut app), 0);
        assert!(
            testing::shows(&mut app, "Approve selected"),
            "drawn, disabled"
        );
        assert_eq!(testing::count::<ApproveAll>(&mut app), 1);
    }

    #[test]
    fn a_click_without_a_connection_sends_nothing() {
        let (mut app, pr) = modal_app();
        app.world_mut()
            .resource_mut::<crate::bridge::Model>()
            .connection = Connection::Lost("gone".into());
        let all = testing::find::<ApproveAll>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, all);
        assert!(testing::recorded(&mut app).is_empty());
        let m = model(&app, &pr);
        assert_eq!(m.pending, None);
        assert!(m.plan.as_ref().is_some_and(|p| !p.sending));
        assert!(
            app.world()
                .resource::<Toasts>()
                .0
                .iter()
                .any(|t| t.warning && t.text.contains("Not connected"))
        );
    }

    #[test]
    fn a_refused_hash_shows_the_new_plan_and_keeps_the_modal() {
        let (mut app, pr) = modal_app();
        let all = testing::find::<ApproveAll>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, all);
        testing::recorded(&mut app);
        testing::tell(
            &mut app,
            Tell::Tests(TestsTell::Refused {
                pr: pr.clone(),
                ask: TestsAskKind::Prepare,
                message: RECIPE_CHANGED.into(),
            }),
        );
        let mut steps = base_steps();
        steps.push(step(
            "extra",
            "Seed the database",
            "cargo run --bin seed",
            StepPlace::Container,
            false,
            StepState::Missing,
        ));
        let view = needs_setup(&app, &pr, "h2", steps);
        testing::open_tests(&mut app, &pr, view);
        assert_eq!(modals(&mut app), 1, "the modal stays on the new plan");
        assert!(testing::shows(
            &mut app,
            "The recipe changed. Look at the plan again before you approve."
        ));
        assert!(testing::shows(&mut app, "Seed the database"));
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::GetTests(pr.clone())],
            "only the model's read of the plan; nothing is re-approved"
        );
        let all = testing::find::<ApproveAll>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, all);
        assert_eq!(
            testing::recorded(&mut app),
            [prepare(
                &pr,
                "h2",
                &["image", "work:0", "stack", "extra", "tests"]
            )],
            "the new hash, after the new plan was drawn"
        );
    }

    #[test]
    fn cancel_escape_and_the_backdrop_close_it_without_asking() {
        for how in ["cancel", "escape", "backdrop"] {
            let (mut app, pr) = modal_app();
            match how {
                "cancel" => {
                    let cancel = testing::find::<CancelPlan>(&mut app, |b| b.0 == pr);
                    testing::activate(&mut app, cancel);
                }
                "escape" => {
                    app.world_mut()
                        .resource_mut::<bevy::input::ButtonInput<KeyCode>>()
                        .press(KeyCode::Escape);
                    app.update();
                    app.world_mut()
                        .resource_mut::<bevy::input::ButtonInput<KeyCode>>()
                        .reset_all();
                }
                _ => {
                    let root = testing::find::<ModalFor>(&mut app, |m| m.modal == Modal::TestSetup);
                    testing::click(&mut app, root);
                }
            }
            testing::settle(&mut app);
            assert_eq!(ui_modal(&app, &pr), None, "{how}");
            assert!(model(&app, &pr).plan.is_none(), "{how}");
            assert_eq!(modals(&mut app), 0, "{how}");
            assert!(testing::recorded(&mut app).is_empty(), "{how}");
        }
    }

    #[test]
    fn a_click_inside_the_card_does_not_close_it() {
        let (mut app, pr) = modal_app();
        let card = testing::find::<crate::ui::modal::ModalCard>(&mut app, |_| true);
        testing::click(&mut app, card);
        testing::settle(&mut app);
        assert_eq!(ui_modal(&app, &pr), Some(Modal::TestSetup));
    }

    #[test]
    fn edit_recipe_opens_the_recipe_and_closes_the_modal() {
        let (mut app, pr) = setup_app(|_| {});
        set_worktree(&mut app, &pr);
        open_modal(&mut app, &pr);
        let edit = testing::find::<PlanEditRecipe>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, edit);
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::OpenInEditor {
                path: format!("{WORKTREE}/{RECIPE_FILE}"),
                line: None
            }]
        );
        assert!(model(&app, &pr).reread_on_focus);
        assert_eq!(modals(&mut app), 0);
        assert!(model(&app, &pr).plan.is_none());
    }

    #[test]
    fn the_modal_closes_when_the_setup_moves_on() {
        for phase in [
            TestsPhase::Preparing {
                step: "image".into(),
            },
            TestsPhase::Ready,
        ] {
            let (mut app, pr) = modal_app();
            let mut view = needs_setup(&app, &pr, "h1", base_steps());
            view.phase = phase;
            testing::open_tests(&mut app, &pr, view);
            assert_eq!(modals(&mut app), 0);
            assert_eq!(ui_modal(&app, &pr), None);
            assert!(model(&app, &pr).plan.is_none());
        }
    }

    fn generating(app: &mut App, pr: &PrRef) {
        let view = testing::tests_view(
            app,
            pr,
            TestsPhase::Generating {
                activity: Some("Writing the recipe".into()),
            },
        );
        testing::open_tests(app, pr, view);
        testing::recorded(app);
    }

    #[test]
    fn the_modal_opens_on_its_own_once_per_recipe_hash() {
        let (mut app, pr) = tests_app();
        generating(&mut app, &pr);
        let view = needs_setup(&app, &pr, "h1", base_steps());
        testing::open_tests(&mut app, &pr, view);
        assert_eq!(modals(&mut app), 1, "the plan arrived: the modal opens");
        assert!(model(&app, &pr).auto_opened.contains("h1"));
        assert!(!model(&app, &pr).plan_wanted);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "opening sends nothing"
        );
        let cancel = testing::find::<CancelPlan>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, cancel);
        testing::settle(&mut app);
        // Generated again with the same recipe: it does not open by itself twice.
        generating(&mut app, &pr);
        let view = needs_setup(&app, &pr, "h1", base_steps());
        testing::open_tests(&mut app, &pr, view);
        assert_eq!(modals(&mut app), 0);
        // A new recipe does.
        generating(&mut app, &pr);
        let view = needs_setup(&app, &pr, "h2", base_steps());
        testing::open_tests(&mut app, &pr, view);
        assert_eq!(modals(&mut app), 1);
        // And Set up… opens it whenever the reviewer asks.
        let cancel = testing::find::<CancelPlan>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, cancel);
        testing::settle(&mut app);
        let set_up = testing::find::<SetUpButton>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, set_up);
        testing::settle(&mut app);
        assert_eq!(modals(&mut app), 1);
    }

    #[test]
    fn the_modal_waits_for_the_tests_section() {
        let (mut app, pr) = tests_app();
        testing::set_section(&mut app, &pr, ReviewSection::Diff);
        generating(&mut app, &pr);
        let view = needs_setup(&app, &pr, "h1", base_steps());
        testing::open_tests(&mut app, &pr, view);
        assert_eq!(modals(&mut app), 0, "the reviewer is in Diff");
        assert!(model(&app, &pr).plan_wanted, "it still waits");
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        assert_eq!(modals(&mut app), 1, "the next entry into Tests opens it");
        assert!(!model(&app, &pr).plan_wanted);
    }

    #[test]
    fn another_modal_holds_the_plan_back() {
        let (mut app, pr) = tests_app();
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .modal = Some(Modal::Finalize);
        generating(&mut app, &pr);
        let view = needs_setup(&app, &pr, "h1", base_steps());
        testing::open_tests(&mut app, &pr, view);
        assert_eq!(ui_modal(&app, &pr), Some(Modal::Finalize), "not taken over");
        assert!(model(&app, &pr).plan_wanted);
    }

    #[test]
    fn what_opens_and_closes_the_modal_is_decided_by_the_model_and_the_tab() {
        use crate::review_state::Modal::{Finalize, TestSetup};
        use crate::screens::review::tests_tab::PlanModal;
        let view = |phase: TestsPhase, blocked: Option<&str>| {
            let mut v = TestsView::new("abc");
            v.phase = phase;
            let mut plan = clusia_core::tests::SetupPlan {
                recipe_hash: "h1".into(),
                steps: Vec::new(),
                commands: Vec::new(),
                commands_approved: false,
                ready: false,
                blocked: blocked.map(Into::into),
            };
            plan.steps = base_steps();
            v.plan = Some(plan);
            v
        };
        let model =
            |phase: TestsPhase, plan: bool, wanted: bool, blocked: Option<&str>| TestsModel {
                view: Some(view(phase, blocked)),
                plan: plan.then(PlanModal::default),
                plan_wanted: wanted,
                ..TestsModel::default()
            };
        let needs = || model(TestsPhase::NeedsSetup, false, false, None);
        // Opening.
        let asked = model(TestsPhase::NeedsSetup, true, false, None);
        assert_eq!(plan_action(true, None, Some(&asked)), Work::Open);
        let wanted = model(TestsPhase::NeedsSetup, false, true, None);
        assert_eq!(plan_action(true, None, Some(&wanted)), Work::OpenWanted);
        assert_eq!(plan_action(false, None, Some(&wanted)), Work::Nothing);
        assert_eq!(plan_action(true, None, Some(&needs())), Work::Nothing);
        assert_eq!(
            plan_action(true, Some(Finalize), Some(&wanted)),
            Work::Nothing
        );
        // Staying open, and closing.
        assert_eq!(
            plan_action(true, Some(TestSetup), Some(&asked)),
            Work::Nothing
        );
        let blocked = model(TestsPhase::NeedsSetup, true, false, Some("no engine"));
        assert_eq!(
            plan_action(true, Some(TestSetup), Some(&blocked)),
            Work::Nothing,
            "a blocked plan shows its reason"
        );
        assert_eq!(
            plan_action(true, Some(TestSetup), Some(&needs())),
            Work::Close
        );
        assert_eq!(plan_action(true, Some(TestSetup), None), Work::Close);
        let moved = model(TestsPhase::Ready, true, false, None);
        assert_eq!(
            plan_action(true, Some(TestSetup), Some(&moved)),
            Work::Close
        );
        let stale = model(TestsPhase::Ready, true, true, None);
        assert_eq!(
            plan_action(true, None, Some(&stale)),
            Work::Close,
            "a plan the setup has left is forgotten"
        );
    }

    #[test]
    fn step_titles_with_breaks_show_escaped() {
        let (mut app, pr) = tests_app();
        let mut steps = base_steps();
        steps[0].title = "Build\nthe\timage\u{202e}".into();
        steps[0].hint = Some("see [docs](https://evil.example)\u{1b}[31m".into());
        let view = needs_setup(&app, &pr, "h1", steps);
        testing::open_tests(&mut app, &pr, view);
        open_modal(&mut app, &pr);
        assert!(testing::shows(&mut app, "Build\\nthe\\timage\\u{202e}"));
        assert!(testing::shows(
            &mut app,
            "see [docs](https://evil.example)\\u{1b}[31m"
        ));
    }

    #[test]
    fn commands_are_shown_whole_and_as_plain_text() {
        let (mut app, pr) = tests_app();
        let mut steps = base_steps();
        let long = format!("docker build {}", "--build-arg X=1 ".repeat(30));
        steps[0].command = long.clone();
        steps[1].command = "npm ci\n\tnpm run build \u{1b}[2K [x](https://evil.example)".into();
        let mut view = needs_setup(&app, &pr, "h1", steps);
        view.plan.as_mut().unwrap().commands[0].command = "make test # \u{202e}hidden".into();
        view.recipe.as_mut().unwrap().env.insert(
            "EVIL".into(),
            "[a](https://evil.example)\n\u{1b}[31m".into(),
        );
        testing::open_tests(&mut app, &pr, view);
        open_modal(&mut app, &pr);
        assert!(testing::shows(&mut app, &long), "no `…` cut");
        assert!(testing::shows(
            &mut app,
            "npm ci\n\tnpm run build \\u{1b}[2K [x](https://evil.example)"
        ));
        assert!(testing::shows(&mut app, "make test # \\u{202e}hidden"));
        assert!(testing::shows(
            &mut app,
            "EVIL=[a](https://evil.example)\\n\\u{1b}[31m"
        ));
        assert_eq!(testing::count::<crate::ui::markdown::MdBody>(&mut app), 0);
        assert_eq!(testing::count::<crate::ui::markdown::MdLink>(&mut app), 0);
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
        assert!(
            testing::recorded(&mut app).is_empty(),
            "nothing acts on a click"
        );
        assert!(
            app.world()
                .get_resource::<crate::platform_open::OpenUrls>()
                .is_none_or(|u| u.0.is_empty())
        );
    }

    #[test]
    fn a_checkbox_click_redraws_inside_the_same_card() {
        let (mut app, pr) = modal_app();
        testing::run_tweens(&mut app);
        let card = testing::find::<crate::ui::modal::ModalCard>(&mut app, |_| true);
        toggle(&mut app, &pr, "image");
        assert_eq!(
            testing::find::<crate::ui::modal::ModalCard>(&mut app, |_| true),
            card,
            "the card is not respawned, so it does not grow in again"
        );
        assert_eq!(testing::count::<crate::ui::tween::Tween>(&mut app), 0);
        let plan = model(&app, &pr).plan.as_ref().unwrap();
        assert!(!plan.checked.contains("image"));
        assert!(plan.checked.contains("work:0"));
    }

    #[test]
    fn a_settled_modal_rebuilds_nothing() {
        let (mut app, _pr) = modal_app();
        testing::settle(&mut app);
        let card = testing::find::<crate::ui::modal::ModalCard>(&mut app, |_| true);
        let kids: Vec<Entity> = app.world().get::<Children>(card).unwrap().to_vec();
        for _ in 0..12 {
            app.update();
        }
        let after: Vec<Entity> = app.world().get::<Children>(card).unwrap().to_vec();
        assert_eq!(kids, after);
    }

    fn many_steps(n: usize) -> Vec<PlanStep> {
        (0..n)
            .map(|i| {
                step(
                    &format!("work:{i}"),
                    &format!("Step {i}"),
                    "cargo fetch",
                    StepPlace::Container,
                    false,
                    StepState::Missing,
                )
            })
            .collect()
    }

    fn body_scroll(app: &mut App) -> (Entity, f32) {
        let body = testing::entities::<PlanBody>(app)[0];
        let y = app.world().get::<ScrollPosition>(body).unwrap().0.y;
        (body, y)
    }

    #[test]
    fn a_redraw_keeps_the_scroll_and_the_focus() {
        let (mut app, pr) = tests_app();
        let view = needs_setup(&app, &pr, "h1", many_steps(40));
        testing::open_tests(&mut app, &pr, view);
        open_modal(&mut app, &pr);
        let (body, _) = body_scroll(&mut app);
        app.world_mut().get_mut::<ScrollPosition>(body).unwrap().0.y = 120.0;
        let focused = testing::find::<PlanStepBox>(&mut app, |b| b.id == "work:30");
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(focused, FocusCause::Navigated);
        toggle(&mut app, &pr, "work:30");
        testing::settle(&mut app);
        assert_eq!(
            body_scroll(&mut app),
            (body, 120.0),
            "a tick keeps the scroll"
        );
        let now = app
            .world()
            .resource::<InputFocus>()
            .get()
            .expect("focus kept");
        assert_eq!(app.world().get::<PlanStepBox>(now).unwrap().id, "work:30");
        assert!(
            !model(&app, &pr)
                .plan
                .as_ref()
                .unwrap()
                .checked
                .contains("work:30")
        );
        // A plan that changes while the reviewer reads keeps it too.
        let mut steps = many_steps(40);
        steps.push(step(
            "extra",
            "Seed the database",
            "cargo run --bin seed",
            StepPlace::Container,
            false,
            StepState::Missing,
        ));
        let view = needs_setup(&app, &pr, "h2", steps);
        testing::open_tests(&mut app, &pr, view);
        assert!(testing::shows(&mut app, "Seed the database"));
        assert_eq!(
            body_scroll(&mut app),
            (body, 120.0),
            "a new plan keeps the scroll"
        );
    }

    #[test]
    fn two_activations_in_one_frame_send_once() {
        let (mut app, pr) = modal_app();
        let all = testing::find::<ApproveAll>(&mut app, |b| b.0 == pr);
        app.world_mut().trigger(Activate { entity: all });
        app.world_mut().trigger(Activate { entity: all });
        app.update();
        assert_eq!(testing::recorded(&mut app).len(), 1);
    }
}
