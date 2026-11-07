//! The cockpit's three modals: the approval of a scenario's command and script, main's setup
//! plan, and the confirmation to remove a scenario. What a modal sends is read from the view it
//! drew, never from the model, so a reviewer approves what they saw.

use std::collections::BTreeSet;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::input_focus::InputFocus;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::cockpit::{
    CockpitScope, InputKind, ItemKind, ItemRef, MainPhase, ScenarioApproval, ScenarioView,
};
use clusia_core::printable::{printable, printable_lines};
use clusia_core::tests::{Approve, RunnerKind, STEP_TESTS};

use super::center::tab_strip;
use super::views::{PURPOSE_SHOWN, TITLE_SHOWN, VALUE_SHOWN, shown};
use super::{CockpitModal, CockpitModel, CockpitModels};
use crate::agent_name;
use crate::bridge::{Ask, Asks, CockpitAskKind, Model, Toasts};
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen};
use crate::review_state::{Modal, ReviewSection, ReviewTabs};
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::screens::review::shell::ModalFor;
use crate::screens::review::tests_tab::TestsModels;
use crate::screens::review::tests_tab::body::cockpit_open;
use crate::screens::review::tests_tab::plan::{
    RowKind, SECTION, StepRow, command_rows, draw_step, labelled, step_rows,
};
use crate::theme::Swatch;
use crate::ui::kit::{
    Stroke, Type, Variant, button, disabled_button, divider, panel, text, wrapped_text,
};
use crate::ui::modal::{BackdropClicked, escape_pressed, modal_card, modal_root};

/// A step's checkbox in the Prepare main modal.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PrepareMainStepBox {
    pub pr: PrRef,
    pub id: String,
}

/// What a script card of the approval modal draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptDrawn {
    /// "SCRIPT · <NAMES>".
    pub header: String,
    /// "<name> · <size>" per file.
    pub tabs: Vec<String>,
    pub selected: usize,
    pub text: String,
    /// The hash this script is approved by.
    pub approval_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproveView {
    pub scenario: String,
    pub title: String,
    pub runner: String,
    pub command: String,
    pub script: Option<ScriptDrawn>,
    /// The script held is for an older version of the scenario than the view has now.
    pub stale: bool,
    /// (label, value, kind) per input.
    pub inputs: Vec<(String, String, &'static str)>,
    pub agent: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::screens::review::tests_tab) struct PrepareView {
    pub meta: String,
    pub note: String,
    pub steps: Vec<StepRow>,
    /// `printable_lines`: why the plan cannot be approved.
    pub blocked: Option<String>,
    pub commands: Vec<(&'static str, String)>,
    pub uses: Vec<(&'static str, String)>,
    pub recipe_hash: String,
}

/// "412 B" or "1.9 KB", as the mockup writes a script's size.
fn size_text(bytes: u32) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", f64::from(bytes) / 1024.0)
    }
}

fn kind_text(kind: InputKind) -> &'static str {
    match kind {
        InputKind::String => "text",
        InputKind::Integer => "integer",
        InputKind::Boolean => "boolean",
        InputKind::Duration => "duration",
        InputKind::OneOf => "one of",
    }
}

impl ApproveView {
    pub fn of(model: &CockpitModel, agent: &str) -> Option<ApproveView> {
        let Some(CockpitModal::Approve { scenario }) = &model.modal else {
            return None;
        };
        let s = scenario_of(model, scenario)?;
        let held = model.script.as_ref().filter(|sc| sc.scenario == s.slug);
        let current = held.filter(|sc| sc.approval_hash == s.approval_hash);
        let script = current.map(|sc| {
            let names: Vec<String> = sc
                .files
                .iter()
                .map(|(n, _)| shown(n, TITLE_SHOWN))
                .collect();
            let tabs = sc
                .files
                .iter()
                .map(|(name, _)| {
                    let bytes = s.scripts.iter().find(|m| m.name == *name).map(|m| m.bytes);
                    match bytes {
                        Some(b) => format!("{} · {}", shown(name, TITLE_SHOWN), size_text(b)),
                        None => shown(name, TITLE_SHOWN),
                    }
                })
                .collect();
            let selected = model.script_tab.min(sc.files.len().saturating_sub(1));
            ScriptDrawn {
                header: format!("SCRIPT · {}", names.join(", ").to_uppercase()),
                tabs,
                selected,
                text: sc
                    .files
                    .get(selected)
                    .map(|(_, text)| printable_lines(text))
                    .unwrap_or_default(),
                approval_hash: sc.approval_hash.clone(),
            }
        });
        let runner = match model.view.as_ref().and_then(|v| v.runner) {
            Some(RunnerKind::Container) => "Runs in the container · no network",
            Some(RunnerKind::Local) => "Runs in the macOS sandbox · no network",
            None => "Runs with no network",
        };
        Some(ApproveView {
            scenario: s.slug.clone(),
            title: shown(&s.title, TITLE_SHOWN),
            runner: runner.into(),
            command: shown(s.command.as_deref().unwrap_or(&s.run), PURPOSE_SHOWN),
            stale: held.is_some() && current.is_none(),
            script,
            inputs: s
                .inputs
                .iter()
                .map(|i| {
                    (
                        shown(&i.input.label, TITLE_SHOWN),
                        shown(&i.value, VALUE_SHOWN),
                        kind_text(i.input.kind),
                    )
                })
                .collect(),
            agent: printable(agent),
        })
    }
}

impl PrepareView {
    pub(in crate::screens::review::tests_tab) fn of(
        model: &CockpitModel,
        _agent: &str,
    ) -> Option<PrepareView> {
        let Some(CockpitModal::PrepareMain { checked }) = &model.modal else {
            return None;
        };
        let view = model.view.as_ref()?;
        let plan = view.main.plan.as_ref()?;
        let short = |text: &str, n: usize| printable(&text.chars().take(n).collect::<String>());
        let at = view.main.at.as_deref().map(|a| short(a, 7));
        let image = view.main.image.as_deref().map(|i| shown(i, TITLE_SHOWN));
        let mut meta = vec![match &at {
            Some(at) => format!("main · {at}"),
            None => "main".to_string(),
        }];
        if let Some(image) = &image {
            meta.push(format!("this pull request's image {image}"));
        }
        meta.push(format!("recipe {}", short(&plan.recipe_hash, 7)));
        let hash = printable(&plan.recipe_hash);
        let sha = if hash.chars().count() > 7 {
            format!(
                "{}…{}",
                hash.chars().take(4).collect::<String>(),
                hash.chars()
                    .skip(hash.chars().count() - 3)
                    .collect::<String>()
            )
        } else {
            hash
        };
        let mut uses = vec![(
            "Recipe",
            format!("this PR's recipe (main has none) · sha {sha}"),
        )];
        if let Some(image) = image {
            uses.push(("Image", image));
        }
        uses.push(("Caches", "main's own · clusia-main-* volumes".into()));
        uses.push(("Removed", "with the review, and by Prune".into()));
        Some(PrepareView {
            meta: meta.join(" · "),
            note: "Main builds in its own worktree with its own caches; its first run builds from scratch."
                .into(),
            steps: step_rows(plan, |id| checked.contains(id)),
            blocked: plan.blocked.as_deref().map(printable_lines),
            commands: command_rows(plan),
            uses,
            recipe_hash: plan.recipe_hash.clone(),
        })
    }
}

/// The hash an approval may carry: the script's, when the modal drew it and it is current.
pub fn approve_target(view: &ApproveView) -> Option<String> {
    if view.stale {
        return None;
    }
    view.script.as_ref().map(|s| s.approval_hash.clone())
}

/// The plan's hash and the steps to approve: the checked approvable rows the modal drew, then the
/// test commands once. `None` when the plan cannot be approved.
pub(in crate::screens::review::tests_tab) fn main_approval(
    view: &PrepareView,
    checked: &BTreeSet<String>,
) -> Option<(String, Approve)> {
    if view.blocked.is_some() {
        return None;
    }
    let mut ids: Vec<String> = view
        .steps
        .iter()
        .filter(|s| s.kind == RowKind::Approvable && s.id != STEP_TESTS && checked.contains(&s.id))
        .map(|s| s.id.clone())
        .collect();
    ids.push(STEP_TESTS.to_string());
    Some((view.recipe_hash.clone(), Approve::Steps(ids)))
}

pub fn on_toggle_step(
    activate: On<Activate>,
    boxes: Query<&PrepareMainStepBox>,
    mut models: ResMut<CockpitModels>,
) {
    let Ok(PrepareMainStepBox { pr, id }) = boxes.get(activate.entity) else {
        return;
    };
    if let Some(model) = models.0.get_mut(pr)
        && let Some(CockpitModal::PrepareMain { checked }) = model.modal.as_mut()
    {
        if !checked.remove(id) {
            checked.insert(id.clone());
        }
        model.touch();
    }
}

fn scenario_of<'a>(model: &'a CockpitModel, slug: &str) -> Option<&'a ScenarioView> {
    model
        .view
        .as_ref()?
        .scenarios
        .iter()
        .find(|s| s.slug == slug)
}

type Spawner<'a> = ChildSpawnerCommands<'a>;

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ApproveButton(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ApproveAndRunButton(pub PrRef);

/// **Cancel** of any of the three modals.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CancelModal(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PrepareMainApprove(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RemoveConfirm(pub PrRef);

/// The approval modal's root, with what it last drew and its card.
#[derive(Component)]
pub struct ApproveBuilt {
    pub view: ApproveView,
    card: Entity,
}

#[derive(Component)]
pub(in crate::screens::review::tests_tab) struct MainBuilt {
    view: PrepareView,
    card: Entity,
}

#[derive(Component)]
pub struct RemoveBuilt {
    view: RemoveView,
    card: Entity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveView {
    pub scenario: String,
    pub title: String,
}

impl RemoveView {
    pub fn of(model: &CockpitModel) -> Option<RemoveView> {
        let Some(CockpitModal::Remove { scenario }) = &model.modal else {
            return None;
        };
        let s = scenario_of(model, scenario)?;
        Some(RemoveView {
            scenario: s.slug.clone(),
            title: shown(&s.title, TITLE_SHOWN),
        })
    }
}

fn is_ours(modal: Modal) -> bool {
    matches!(
        modal,
        Modal::CockpitApprove | Modal::CockpitMain | Modal::CockpitRemove
    )
}

fn kind_of(modal: &CockpitModal) -> Modal {
    match modal {
        CockpitModal::Approve { .. } => Modal::CockpitApprove,
        CockpitModal::PrepareMain { .. } => Modal::CockpitMain,
        CockpitModal::Remove { .. } => Modal::CockpitRemove,
    }
}

/// Opens the modal the model holds and closes the one it no longer holds. A modal of another
/// kind that is up holds this one back, and a cockpit that is not on screen has none.
pub fn open_cockpit_modals(
    nav: Res<Nav>,
    mut tabs: ResMut<ReviewTabs>,
    mut models: ResMut<CockpitModels>,
    tests: Res<TestsModels>,
) {
    let mut drop_model: Vec<PrRef> = Vec::new();
    let mut set_tab: Vec<(PrRef, Option<Modal>)> = Vec::new();
    for (pr, tab) in &tabs.0 {
        let want = models.0.get(pr).and_then(|m| m.modal.as_ref()).map(kind_of);
        let up = tab.ui.modal;
        let on_screen = tab.ready().is_some()
            && tab.ui.section == ReviewSection::Tests
            && cockpit_open(tests.0.get(pr));
        match (want, up) {
            (Some(_), _) if !on_screen => {
                drop_model.push(pr.clone());
                if up.is_some_and(is_ours) {
                    set_tab.push((pr.clone(), None));
                }
            }
            (Some(w), None) if nav.screen == Screen::Review(pr.clone()) => {
                set_tab.push((pr.clone(), Some(w)));
            }
            (Some(w), Some(c)) if c != w && is_ours(c) => set_tab.push((pr.clone(), Some(w))),
            (None, Some(c)) if is_ours(c) => set_tab.push((pr.clone(), None)),
            _ => {}
        }
    }
    for pr in drop_model {
        if let Some(model) = models.0.get_mut(&pr) {
            model.modal = None;
            model.touch();
        }
    }
    for (pr, modal) in set_tab {
        if let Some(tab) = tabs.0.get_mut(&pr) {
            tab.ui.modal = modal;
        }
    }
}

fn close_modal(pr: &PrRef, tabs: &mut ReviewTabs, models: &mut CockpitModels) {
    if let Some(model) = models.0.get_mut(pr)
        && model.modal.is_some()
    {
        model.modal = None;
        model.touch();
    }
    if let Some(tab) = tabs.0.get_mut(pr)
        && tab.ui.modal.is_some_and(is_ours)
    {
        tab.ui.modal = None;
    }
}

/// Whether any cockpit modal is up: Escape then closes it and nothing else.
pub fn cockpit_modal_up(roots: Query<&ModalFor>) -> bool {
    roots.iter().any(|m| is_ours(m.modal))
}

/// Esc and a click outside the card close the modal.
pub fn cancel_cockpit_modal(
    keys: Res<ButtonInput<KeyCode>>,
    mut clicks: MessageReader<BackdropClicked>,
    roots: Query<&ModalFor>,
    nav: Res<Nav>,
    mut tabs: ResMut<ReviewTabs>,
    mut models: ResMut<CockpitModels>,
) {
    let outside = clicks
        .read()
        .any(|c| roots.get(c.0).is_ok_and(|m| is_ours(m.modal)));
    if !(escape_pressed(&keys) || outside) {
        return;
    }
    if let Screen::Review(pr) = &nav.screen {
        close_modal(pr, &mut tabs, &mut models);
    }
}

fn on_cancel(
    activate: On<Activate>,
    buttons: Query<&CancelModal>,
    mut tabs: ResMut<ReviewTabs>,
    mut models: ResMut<CockpitModels>,
) {
    if let Ok(CancelModal(pr)) = buttons.get(activate.entity) {
        close_modal(pr, &mut tabs, &mut models);
    }
}

const HEAD_PAD: UiRect = UiRect {
    left: Val::Px(24.0),
    right: Val::Px(24.0),
    top: Val::Px(22.0),
    bottom: Val::Px(10.0),
};

fn head(card: &mut Spawner, build: impl FnOnce(&mut Spawner)) {
    card.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(4),
        padding: HEAD_PAD,
        ..default()
    })
    .with_children(build);
}

fn body(card: &mut Spawner, build: impl FnOnce(&mut Spawner)) {
    card.spawn(Node {
        flex_direction: FlexDirection::Column,
        flex_grow: 1.0,
        flex_shrink: 1.0,
        min_height: px(0),
        padding: UiRect::axes(px(24), px(4)),
        row_gap: px(6),
        overflow: Overflow::scroll_y(),
        ..default()
    })
    .insert((ScrollArea, ScrollPosition::default()))
    .with_children(build);
}

fn foot(card: &mut Spawner, build: impl FnOnce(&mut Spawner)) {
    card.spawn(Node {
        width: percent(100),
        column_gap: px(10),
        padding: UiRect::axes(px(24), px(16)),
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(build);
}

fn code_pane(p: &mut Spawner, fonts: &UiFonts, content: String, max_height: Option<f32>) {
    let mut node = Node {
        width: percent(100),
        padding: UiRect::all(px(12)),
        border_radius: BorderRadius::all(px(6)),
        flex_direction: FlexDirection::Column,
        flex_shrink: 0.0,
        ..default()
    };
    if let Some(h) = max_height {
        node.max_height = px(h);
        node.overflow = Overflow::scroll_y();
    }
    let mut pane = p.spawn(panel(node, Swatch::Chrome));
    if max_height.is_some() {
        pane.insert((ScrollArea, ScrollPosition::default()));
    }
    pane.with_child(wrapped_text(fonts, content, Type::MONO.ink(Swatch::Fg)));
}

fn spacer(p: &mut Spawner) {
    p.spawn(Node {
        flex_grow: 1.0,
        ..default()
    });
}

#[allow(clippy::too_many_arguments)]
pub fn fill_approve(
    mut commands: Commands,
    nav: Res<Nav>,
    tabs: Res<ReviewTabs>,
    models: Res<CockpitModels>,
    app_model: Res<Model>,
    fonts: Res<UiFonts>,
    roots: Query<(Entity, &ModalFor, Option<&ApproveBuilt>)>,
) {
    for (pr, tab) in &tabs.0 {
        if tab.ui.modal != Some(Modal::CockpitApprove) || nav.screen != Screen::Review(pr.clone()) {
            continue;
        }
        let Some(model) = models.0.get(pr) else {
            continue;
        };
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let Some(view) = ApproveView::of(model, &agent) else {
            continue;
        };
        let root = roots
            .iter()
            .find(|(_, m, _)| &m.pr == pr && m.modal == Modal::CockpitApprove);
        match root {
            None => {
                let mut card = Entity::PLACEHOLDER;
                let root = commands
                    .spawn((
                        modal_root(),
                        ModalFor {
                            pr: pr.clone(),
                            modal: Modal::CockpitApprove,
                        },
                    ))
                    .with_children(|r| {
                        let mut c = r.spawn(modal_card(840.0));
                        card = c.id();
                        c.with_children(|c| draw_approve(c, &fonts, pr, &view));
                    })
                    .id();
                commands.entity(root).insert(ApproveBuilt { view, card });
            }
            Some((_, _, Some(built))) if built.view == view => {}
            Some((root, _, Some(built))) => {
                let card = built.card;
                commands.entity(card).despawn_related::<Children>();
                commands
                    .entity(card)
                    .with_children(|c| draw_approve(c, &fonts, pr, &view));
                commands.entity(root).insert(ApproveBuilt { view, card });
            }
            Some((_, _, None)) => {}
        }
    }
}

fn draw_approve(card: &mut Spawner, fonts: &UiFonts, pr: &PrRef, view: &ApproveView) {
    head(card, |h| {
        h.spawn(text(fonts, "Run this scenario?", Type::HEADING.size(17.0)));
        h.spawn(wrapped_text(fonts, view.title.clone(), Type::STRONG));
        h.spawn(text(fonts, view.runner.clone(), Type::MUTED));
    });
    body(card, |b| {
        b.spawn(text(fonts, "COMMAND", SECTION));
        code_pane(b, fonts, view.command.clone(), None);
        b.spawn(wrapped_text(
            fonts,
            "Values reach the script only as environment variables (CLUSIA_ARG_<NAME>).",
            Type::META.ink(Swatch::Faint),
        ));
        match &view.script {
            Some(script) => {
                b.spawn(Node {
                    margin: UiRect::top(px(8)),
                    ..default()
                })
                .with_child(text(fonts, script.header.clone(), SECTION));
                b.spawn(panel(
                    Node {
                        width: percent(100),
                        flex_direction: FlexDirection::Column,
                        flex_shrink: 0.0,
                        border: px(1).all(),
                        border_radius: BorderRadius::all(px(6)),
                        overflow: Overflow::clip(),
                        ..default()
                    },
                    Swatch::Chrome,
                ))
                .insert((BorderColor::default(), Stroke(Swatch::Line)))
                .with_children(|pane| {
                    pane.spawn(Node {
                        width: percent(100),
                        ..default()
                    })
                    .with_children(|strip| {
                        tab_strip(strip, fonts, pr, &script.tabs, script.selected)
                    });
                    pane.spawn(divider());
                    pane.spawn(Node {
                        width: percent(100),
                        max_height: px(SCRIPT_MAX_HEIGHT),
                        padding: UiRect::all(px(12)),
                        overflow: Overflow::scroll_y(),
                        ..default()
                    })
                    .insert((ScrollArea, ScrollPosition::default()))
                    .with_child(wrapped_text(
                        fonts,
                        script.text.clone(),
                        Type::MONO.ink(Swatch::Fg),
                    ));
                });
            }
            None => {
                b.spawn(Node {
                    margin: UiRect::top(px(8)),
                    ..default()
                })
                .with_child(text(fonts, "SCRIPT", SECTION));
                let (line, ink) = if view.stale {
                    (
                        "The script changed since you opened this; read it again.",
                        Swatch::Amber,
                    )
                } else {
                    ("Loading the script…", Swatch::Muted)
                };
                b.spawn(wrapped_text(fonts, line, Type::BODY.ink(ink)));
            }
        }
        if !view.inputs.is_empty() {
            b.spawn(Node {
                margin: UiRect::top(px(8)),
                ..default()
            })
            .with_child(text(fonts, "INPUTS", SECTION));
            b.spawn(Node {
                width: percent(100),
                flex_wrap: FlexWrap::Wrap,
                column_gap: px(16),
                row_gap: px(4),
                ..default()
            })
            .with_children(|grid| {
                for (label, value, kind) in &view.inputs {
                    grid.spawn(Node {
                        width: percent(48),
                        column_gap: px(10),
                        align_items: AlignItems::Baseline,
                        ..default()
                    })
                    .with_children(|row| {
                        row.spawn(Node {
                            width: px(90),
                            flex_shrink: 0.0,
                            ..default()
                        })
                        .with_child(wrapped_text(
                            fonts,
                            label.clone(),
                            Type::MUTED,
                        ));
                        row.spawn(text(fonts, value.clone(), Type::MONO.ink(Swatch::Fg)));
                        row.spawn(text(
                            fonts,
                            format!("({kind})"),
                            Type::META.ink(Swatch::Faint),
                        ));
                    });
                }
            });
        }
    });
    let target = approve_target(view);
    foot(card, |f| {
        f.spawn(wrapped_text(
            fonts,
            "Main runs the same files from a read-only copy.",
            Type::MUTED,
        ));
        spacer(f);
        f.spawn((
            button(fonts, "Cancel", Variant::Secondary),
            CancelModal(pr.clone()),
            observe(on_cancel),
        ));
        if target.is_some() {
            f.spawn((
                button(fonts, "Approve", Variant::Secondary),
                ApproveButton(pr.clone()),
                observe(on_approve),
            ));
            f.spawn((
                button(fonts, "Approve and run", Variant::Primary),
                ApproveAndRunButton(pr.clone()),
                observe(on_approve_and_run),
            ));
        } else {
            f.spawn(disabled_button(fonts, "Approve"));
            f.spawn(disabled_button(fonts, "Approve and run"));
        }
    });
}

/// The tallest the script grows before it scrolls.
const SCRIPT_MAX_HEIGHT: f32 = 236.0;

/// Sends the approval of the script the modal drew; with `run`, the scenario runs once its
/// approval is in.
#[allow(clippy::too_many_arguments)]
fn approve(
    pr: &PrRef,
    run: bool,
    built: &Query<(&ModalFor, &ApproveBuilt)>,
    tabs: &mut ReviewTabs,
    app_model: &Model,
    models: &mut CockpitModels,
    toasts: &mut Toasts,
    time: &Time,
    asks: &mut Asks,
) {
    let Some(view) = built
        .iter()
        .find(|(m, _)| &m.pr == pr && m.modal == Modal::CockpitApprove)
        .map(|(_, b)| b.view.clone())
    else {
        return;
    };
    let Some(hash) = approve_target(&view) else {
        return;
    };
    if let Some(reason) = read_only_reason(tabs, app_model, pr) {
        return not_sent(toasts, time, reason);
    }
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if model.pending == Some(CockpitAskKind::Approve) {
        return;
    }
    asks.send(Ask::ApproveScenario {
        pr: pr.clone(),
        scenario: view.scenario.clone(),
        hash,
    });
    model.sent(CockpitAskKind::Approve);
    if run {
        model.run_after_approve = Some(ItemRef {
            kind: ItemKind::Scenario,
            id: view.scenario,
        });
    }
    close_modal(pr, tabs, models);
}

#[allow(clippy::too_many_arguments)]
fn on_approve(
    activate: On<Activate>,
    buttons: Query<&ApproveButton>,
    built: Query<(&ModalFor, &ApproveBuilt)>,
    mut tabs: ResMut<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    if let Ok(ApproveButton(pr)) = buttons.get(activate.entity) {
        approve(
            pr,
            false,
            &built,
            &mut tabs,
            &app_model,
            &mut models,
            &mut toasts,
            &time,
            &mut asks,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn on_approve_and_run(
    activate: On<Activate>,
    buttons: Query<&ApproveAndRunButton>,
    built: Query<(&ModalFor, &ApproveBuilt)>,
    mut tabs: ResMut<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    if let Ok(ApproveAndRunButton(pr)) = buttons.get(activate.entity) {
        approve(
            pr,
            true,
            &built,
            &mut tabs,
            &app_model,
            &mut models,
            &mut toasts,
            &time,
            &mut asks,
        );
    }
}

/// Continues an **Approve and run** the reviewer clicked, once the approval has landed: runs
/// the scenario with the hash its approved view has. A refused approval or a view that is still
/// not approved ends the wait and runs nothing. Written around change detection and announced
/// only when something was written.
pub fn run_after_approval(mut models: ResMut<CockpitModels>, mut asks: ResMut<Asks>) {
    let mut wrote = false;
    for (pr, model) in &mut models.bypass_change_detection().0 {
        let Some(item) = model.run_after_approve.clone() else {
            continue;
        };
        if model.pending == Some(CockpitAskKind::Approve) {
            continue;
        }
        model.run_after_approve = None;
        wrote = true;
        let Some(s) = scenario_of(model, &item.id) else {
            continue;
        };
        if s.approval != ScenarioApproval::Approved {
            continue;
        }
        let seen = s.hash.clone();
        let on_main = model.on_main;
        if model
            .run_disabled_reason(Some(&item), on_main, model.tests_busy)
            .is_some()
            || (on_main
                && model
                    .view
                    .as_ref()
                    .is_some_and(|v| v.main.phase == MainPhase::NotCreated))
        {
            continue;
        }
        if let Some(ask) = model.click_run(pr, CockpitScope::Item { item }, on_main, seen) {
            asks.send(ask);
        }
    }
    if wrote {
        models.set_changed();
    }
}

/// Whether an approved scenario waits to be run.
pub fn run_after_due(models: Res<CockpitModels>) -> bool {
    models.is_changed() && models.0.values().any(|m| m.run_after_approve.is_some())
}

#[allow(clippy::too_many_arguments)]
pub(in crate::screens::review::tests_tab) fn fill_main(
    mut commands: Commands,
    nav: Res<Nav>,
    tabs: Res<ReviewTabs>,
    models: Res<CockpitModels>,
    app_model: Res<Model>,
    fonts: Res<UiFonts>,
    focus: Res<InputFocus>,
    boxes: Query<&PrepareMainStepBox>,
    roots: Query<(Entity, &ModalFor, Option<&MainBuilt>)>,
) {
    for (pr, tab) in &tabs.0 {
        if tab.ui.modal != Some(Modal::CockpitMain) || nav.screen != Screen::Review(pr.clone()) {
            continue;
        }
        let Some(model) = models.0.get(pr) else {
            continue;
        };
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let Some(view) = PrepareView::of(model, &agent) else {
            continue;
        };
        let root = roots
            .iter()
            .find(|(_, m, _)| &m.pr == pr && m.modal == Modal::CockpitMain);
        match root {
            None => {
                let mut card = Entity::PLACEHOLDER;
                let root = commands
                    .spawn((
                        modal_root(),
                        ModalFor {
                            pr: pr.clone(),
                            modal: Modal::CockpitMain,
                        },
                    ))
                    .with_children(|r| {
                        let mut c = r.spawn(modal_card(820.0));
                        card = c.id();
                        c.with_children(|c| draw_main(c, &fonts, pr, &view, None));
                    })
                    .id();
                commands.entity(root).insert(MainBuilt { view, card });
            }
            Some((_, _, Some(built))) if built.view == view => {}
            Some((root, _, Some(built))) => {
                let card = built.card;
                let focused = focus
                    .get()
                    .and_then(|e| boxes.get(e).ok())
                    .map(|b| b.id.clone());
                commands.entity(card).despawn_related::<Children>();
                commands
                    .entity(card)
                    .with_children(|c| draw_main(c, &fonts, pr, &view, focused.as_deref()));
                commands.entity(root).insert(MainBuilt { view, card });
            }
            Some((_, _, None)) => {}
        }
    }
}

fn checked_of(view: &PrepareView) -> BTreeSet<String> {
    view.steps
        .iter()
        .filter(|s| s.checked)
        .map(|s| s.id.clone())
        .collect()
}

fn draw_main(
    card: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    view: &PrepareView,
    focused: Option<&str>,
) {
    head(card, |h| {
        h.spawn(text(fonts, "Prepare main", Type::HEADING.size(17.0)));
        h.spawn(wrapped_text(
            fonts,
            view.meta.clone(),
            Type::MONO.ink(Swatch::Muted),
        ));
        h.spawn(wrapped_text(fonts, view.note.clone(), Type::MUTED));
    });
    body(card, |b| {
        match &view.blocked {
            Some(reason) => {
                b.spawn(wrapped_text(
                    fonts,
                    reason.clone(),
                    Type::BODY.ink(Swatch::Orange),
                ));
            }
            None => {
                let n = view
                    .steps
                    .iter()
                    .filter(|s| s.kind == RowKind::Approvable)
                    .count();
                let steps = if n == 1 { "STEP" } else { "STEPS" };
                b.spawn(text(fonts, format!("SETUP ON MAIN · {n} {steps}"), SECTION));
                for row in &view.steps {
                    draw_step(b, fonts, pr, row, focused == Some(row.id.as_str()), true);
                }
            }
        }
        b.spawn(Node {
            margin: UiRect::top(px(10)),
            ..default()
        })
        .with_child(text(fonts, "COMMANDS ON MAIN · NO NETWORK", SECTION));
        for (label, command) in &view.commands {
            if view.commands.len() == 1 {
                b.spawn(wrapped_text(
                    fonts,
                    command.clone(),
                    Type::MONO.ink(Swatch::Fg),
                ));
            } else {
                labelled(b, fonts, label, |c| {
                    c.spawn(wrapped_text(
                        fonts,
                        command.clone(),
                        Type::MONO.ink(Swatch::Fg),
                    ));
                });
            }
        }
        b.spawn(Node {
            margin: UiRect::top(px(10)),
            ..default()
        })
        .with_child(text(fonts, "USES", SECTION));
        for (label, value) in &view.uses {
            labelled(b, fonts, label, |c| {
                c.spawn(wrapped_text(fonts, value.clone(), Type::BODY));
            });
        }
    });
    let approvable = main_approval(view, &checked_of(view)).is_some();
    foot(card, |f| {
        spacer(f);
        f.spawn((
            button(fonts, "Cancel", Variant::Secondary),
            CancelModal(pr.clone()),
            observe(on_cancel),
        ));
        if approvable {
            f.spawn((
                button(fonts, "Approve and prepare", Variant::Primary),
                PrepareMainApprove(pr.clone()),
                observe(on_prepare_main),
            ));
        } else {
            f.spawn(disabled_button(fonts, "Approve and prepare"));
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn on_prepare_main(
    activate: On<Activate>,
    buttons: Query<&PrepareMainApprove>,
    built: Query<(&ModalFor, &MainBuilt)>,
    mut tabs: ResMut<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(PrepareMainApprove(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let Some(view) = built
        .iter()
        .find(|(m, _)| &m.pr == pr && m.modal == Modal::CockpitMain)
        .map(|(_, b)| b.view.clone())
    else {
        return;
    };
    let Some((main_hash, approve)) = main_approval(&view, &checked_of(&view)) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &app_model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if model.pending == Some(CockpitAskKind::PrepareMain) {
        return;
    }
    asks.send(Ask::PrepareMain {
        pr: pr.clone(),
        main_hash,
        approve,
    });
    model.sent(CockpitAskKind::PrepareMain);
    close_modal(pr, &mut tabs, &mut models);
}

#[allow(clippy::too_many_arguments)]
pub fn fill_remove(
    mut commands: Commands,
    nav: Res<Nav>,
    tabs: Res<ReviewTabs>,
    models: Res<CockpitModels>,
    fonts: Res<UiFonts>,
    roots: Query<(Entity, &ModalFor, Option<&RemoveBuilt>)>,
) {
    for (pr, tab) in &tabs.0 {
        if tab.ui.modal != Some(Modal::CockpitRemove) || nav.screen != Screen::Review(pr.clone()) {
            continue;
        }
        let Some(view) = models.0.get(pr).and_then(RemoveView::of) else {
            continue;
        };
        let root = roots
            .iter()
            .find(|(_, m, _)| &m.pr == pr && m.modal == Modal::CockpitRemove);
        match root {
            None => {
                let mut card = Entity::PLACEHOLDER;
                let root = commands
                    .spawn((
                        modal_root(),
                        ModalFor {
                            pr: pr.clone(),
                            modal: Modal::CockpitRemove,
                        },
                    ))
                    .with_children(|r| {
                        let mut c = r.spawn(modal_card(440.0));
                        card = c.id();
                        c.with_children(|c| draw_remove(c, &fonts, pr, &view));
                    })
                    .id();
                commands.entity(root).insert(RemoveBuilt { view, card });
            }
            Some((_, _, Some(built))) if built.view == view => {}
            Some((root, _, Some(built))) => {
                let card = built.card;
                commands.entity(card).despawn_related::<Children>();
                commands
                    .entity(card)
                    .with_children(|c| draw_remove(c, &fonts, pr, &view));
                commands.entity(root).insert(RemoveBuilt { view, card });
            }
            Some((_, _, None)) => {}
        }
    }
}

fn draw_remove(card: &mut Spawner, fonts: &UiFonts, pr: &PrRef, view: &RemoveView) {
    head(card, |h| {
        h.spawn(text(
            fonts,
            "Remove this scenario?",
            Type::HEADING.size(17.0),
        ));
        h.spawn(wrapped_text(fonts, view.title.clone(), Type::STRONG));
        h.spawn(wrapped_text(
            fonts,
            "Its approval and results go too.",
            Type::MUTED,
        ));
    });
    foot(card, |f| {
        spacer(f);
        f.spawn((
            button(fonts, "Cancel", Variant::Secondary),
            CancelModal(pr.clone()),
            observe(on_cancel),
        ));
        f.spawn((
            button(fonts, "Remove", Variant::Danger),
            RemoveConfirm(pr.clone()),
            observe(on_remove_confirm),
        ));
    });
}

#[allow(clippy::too_many_arguments)]
fn on_remove_confirm(
    activate: On<Activate>,
    buttons: Query<&RemoveConfirm>,
    built: Query<(&ModalFor, &RemoveBuilt)>,
    mut tabs: ResMut<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(RemoveConfirm(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let Some(scenario) = built
        .iter()
        .find(|(m, _)| &m.pr == pr && m.modal == Modal::CockpitRemove)
        .map(|(_, b)| b.view.scenario.clone())
    else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &app_model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if model.pending == Some(CockpitAskKind::Remove) {
        return;
    }
    asks.send(Ask::RemoveScenario {
        pr: pr.clone(),
        scenario,
    });
    model.sent(CockpitAskKind::Remove);
    close_modal(pr, &mut tabs, &mut models);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::CockpitTell;
    use crate::fixture::{self, DemoCockpit, demo_cockpit};
    use crate::testing::NOW;

    const THIRD: &str = "token-endpoint-is-down";

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn model_with(state: DemoCockpit, modal: CockpitModal) -> CockpitModel {
        let mut m = CockpitModel::default();
        m.apply(&CockpitTell::Loaded {
            pr: pr(),
            view: Box::new(demo_cockpit(NOW, state)),
        });
        m.modal = Some(modal);
        m
    }

    fn approve_model() -> CockpitModel {
        model_with(
            DemoCockpit::Approve,
            CockpitModal::Approve {
                scenario: THIRD.into(),
            },
        )
    }

    fn hold_script(m: &mut CockpitModel) {
        m.apply(&CockpitTell::Script {
            pr: pr(),
            script: Box::new(fixture::demo_scenario_script()),
        });
    }

    #[test]
    fn approve_target_needs_the_current_script() {
        let mut m = approve_model();
        let none = ApproveView::of(&m, "Claude Code").expect("a view");
        assert_eq!(approve_target(&none), None);
        assert!(none.script.is_none() && !none.stale);
        let mut other = fixture::demo_scenario_script();
        other.approval_hash = "an older hash".into();
        m.apply(&CockpitTell::Script {
            pr: pr(),
            script: Box::new(other),
        });
        let stale = ApproveView::of(&m, "Claude Code").expect("a view");
        assert_eq!(approve_target(&stale), None);
        assert!(stale.stale && stale.script.is_none());
        hold_script(&mut m);
        let current = ApproveView::of(&m, "Claude Code").expect("a view");
        let hash = fixture::demo_scenario_script().approval_hash;
        assert_eq!(approve_target(&current), Some(hash));
        assert!(!current.stale);
        assert_eq!(
            current.script.as_ref().map(|s| s.header.as_str()),
            Some("SCRIPT · DOWN.SH, FAKE_OAUTH.PY")
        );
        assert_eq!(current.runner, "Runs in the container · no network");
    }

    #[test]
    fn main_approval_names_the_drawn_plan() {
        let m = model_with(
            DemoCockpit::Main,
            CockpitModal::PrepareMain {
                checked: BTreeSet::new(),
            },
        );
        let plan = m
            .view
            .as_ref()
            .and_then(|v| v.main.plan.clone())
            .expect("a plan");
        let mut view = PrepareView::of(&m, "Claude Code").expect("a view");
        let all: BTreeSet<String> = plan.steps.iter().map(|s| s.id.clone()).collect();
        let (hash, approve) = main_approval(&view, &all).expect("approvable");
        assert_eq!(hash, plan.recipe_hash);
        let mut ids: Vec<String> = plan.steps.iter().map(|s| s.id.clone()).collect();
        ids.retain(|id| id != STEP_TESTS);
        ids.push(STEP_TESTS.into());
        assert_eq!(approve, Approve::Steps(ids));
        let first = plan.steps[0].id.clone();
        let only_tests: BTreeSet<String> = all.iter().filter(|id| **id != first).cloned().collect();
        let (_, approve) = main_approval(&view, &only_tests).expect("approvable");
        let Approve::Steps(ids) = approve else {
            panic!("named steps")
        };
        assert!(!ids.contains(&first));
        assert_eq!(ids.last().map(String::as_str), Some(STEP_TESTS));
        assert_eq!(ids.iter().filter(|i| *i == STEP_TESTS).count(), 1);
        view.blocked = Some("a recipe step is unsafe".into());
        assert_eq!(main_approval(&view, &all), None);
    }
}

#[cfg(test)]
mod app_tests {
    use clusia_core::cockpit::CockpitScope;
    use clusia_core::tests::STEP_STACK;

    use super::super::center::OpenApproval;
    use super::super::center::RemoveScenarioButton;
    use super::super::center::support::*;
    use super::*;
    use crate::bridge::CockpitTell;
    use crate::fixture::{self, DemoCockpit, demo_cockpit};
    use crate::review_state::ReviewTabs;
    use crate::screens::review::tests_tab::{SubTab, TestsModels};
    use crate::testing::{self, NOW};

    fn script_tell() -> CockpitTell {
        CockpitTell::Script {
            pr: pr(),
            script: Box::new(fixture::demo_scenario_script()),
        }
    }

    fn open_approval(app: &mut App, pr: &PrRef) {
        select(app, pr, scenario_item(THIRD));
        click::<OpenApproval>(app, |b| b.pr == *pr);
    }

    fn modals_up(app: &mut App) -> usize {
        testing::count::<ModalFor>(app)
    }

    fn approved_reply() -> CockpitView {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        let s = view
            .scenarios
            .iter_mut()
            .find(|s| s.slug == THIRD)
            .expect("third");
        s.approval = ScenarioApproval::Approved;
        s.support = clusia_core::cockpit::ItemSupport::Ready;
        s.hash = "after-approval".into();
        view
    }

    use clusia_core::cockpit::CockpitView;

    #[test]
    fn opening_the_approval_modal_asks_for_the_script_once() {
        let (mut app, pr) = scenario_app();
        edit_model(&mut app, &pr, |m| m.select(scenario_item(THIRD)));
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::GetScenarioScript {
                pr: pr.clone(),
                scenario: THIRD.into()
            }]
        );
        click::<OpenApproval>(&mut app, |b| b.pr == pr);
        assert_eq!(modals_up(&mut app), 1);
        assert!(testing::recorded(&mut app).is_empty(), "asked already");
        assert!(testing::shows(&mut app, "Run this scenario?"));
        assert!(testing::shows(&mut app, "Loading the script…"));
        assert_eq!(testing::count::<ApproveButton>(&mut app), 0);
        assert_eq!(testing::count::<ApproveAndRunButton>(&mut app), 0);
        tell(&mut app, script_tell());
        for needle in [
            "Token endpoint is down",
            "Runs in the container · no network",
            "COMMAND",
            "sh down.sh \"$CLUSIA_ARG_RETRIES\" \"$CLUSIA_ARG_TIMEOUT\"",
            "SCRIPT · DOWN.SH, FAKE_OAUTH.PY",
            "down.sh · ",
            "INPUTS",
            "(integer)",
            "(duration)",
            "Main runs the same files from a read-only copy.",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert_eq!(testing::count::<ApproveButton>(&mut app), 1);
        assert_eq!(testing::count::<ApproveAndRunButton>(&mut app), 1);
    }

    #[test]
    fn approve_and_run_sends_the_hash_the_modal_drew() {
        let (mut app, pr) = scenario_app();
        open_approval(&mut app, &pr);
        tell(&mut app, script_tell());
        let hash = fixture::demo_scenario_script().approval_hash;
        click::<ApproveAndRunButton>(&mut app, |b| b.0 == pr);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::ApproveScenario {
                pr: pr.clone(),
                scenario: THIRD.into(),
                hash
            }]
        );
        assert_eq!(
            model(&app, &pr).run_after_approve,
            Some(scenario_item(THIRD))
        );
        assert_eq!(modals_up(&mut app), 0, "the modal is closed");
        tell(
            &mut app,
            CockpitTell::Loaded {
                pr: pr.clone(),
                view: Box::new(approved_reply()),
            },
        );
        testing::settle(&mut app);
        let sent = testing::recorded(&mut app);
        assert_eq!(
            sent.iter()
                .filter(|a| matches!(a, Ask::RunCockpit { .. }))
                .collect::<Vec<_>>(),
            [&Ask::RunCockpit {
                pr: pr.clone(),
                scope: CockpitScope::Item {
                    item: scenario_item(THIRD)
                },
                on_main: false,
                seen: "after-approval".into()
            }]
        );
        assert_eq!(model(&app, &pr).run_after_approve, None);
    }

    #[test]
    fn a_refused_approval_runs_nothing_and_clears_the_wait() {
        let (mut app, pr) = scenario_app();
        open_approval(&mut app, &pr);
        tell(&mut app, script_tell());
        click::<ApproveAndRunButton>(&mut app, |b| b.0 == pr);
        testing::recorded(&mut app);
        tell(
            &mut app,
            CockpitTell::Refused {
                pr: pr.clone(),
                kind: CockpitAskKind::Approve,
                message: "The script changed".into(),
            },
        );
        testing::settle(&mut app);
        assert_eq!(model(&app, &pr).run_after_approve, None);
        assert!(
            !testing::recorded(&mut app)
                .iter()
                .any(|a| matches!(a, Ask::RunCockpit { .. }))
        );
    }

    #[test]
    fn a_reply_that_is_still_not_approved_runs_nothing() {
        let (mut app, pr) = scenario_app();
        open_approval(&mut app, &pr);
        tell(&mut app, script_tell());
        click::<ApproveAndRunButton>(&mut app, |b| b.0 == pr);
        testing::recorded(&mut app);
        tell(
            &mut app,
            CockpitTell::Loaded {
                pr: pr.clone(),
                view: Box::new(demo_cockpit(NOW, DemoCockpit::Scenario)),
            },
        );
        testing::settle(&mut app);
        assert_eq!(model(&app, &pr).run_after_approve, None);
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn plain_approve_sends_only_the_approval() {
        let (mut app, pr) = scenario_app();
        open_approval(&mut app, &pr);
        tell(&mut app, script_tell());
        click::<ApproveButton>(&mut app, |b| b.0 == pr);
        assert!(matches!(
            testing::recorded(&mut app).as_slice(),
            [Ask::ApproveScenario { .. }]
        ));
        assert_eq!(model(&app, &pr).run_after_approve, None);
        assert_eq!(model(&app, &pr).pending, Some(CockpitAskKind::Approve));
    }

    #[test]
    fn approve_never_sends_a_hash_it_did_not_draw() {
        let (mut app, pr) = scenario_app();
        open_approval(&mut app, &pr);
        tell(&mut app, script_tell());
        assert_eq!(testing::count::<ApproveButton>(&mut app), 1);
        testing::recorded(&mut app);
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.scenarios
            .iter_mut()
            .find(|s| s.slug == THIRD)
            .expect("third")
            .approval_hash = "a newer hash".into();
        changed(&mut app, view);
        assert_eq!(testing::count::<ApproveButton>(&mut app), 0);
        assert_eq!(testing::count::<ApproveAndRunButton>(&mut app), 0);
        assert!(testing::shows(
            &mut app,
            "The script changed since you opened this; read it again."
        ));
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::GetScenarioScript {
                pr: pr.clone(),
                scenario: THIRD.into()
            }]
        );
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty(), "re-asked once");
    }

    #[test]
    fn the_prepare_main_modal_approves_what_it_drew() {
        let (mut app, pr) = app_with(demo_cockpit(NOW, DemoCockpit::Main));
        edit_model(&mut app, &pr, super::super::list::open_prepare_main);
        assert_eq!(modals_up(&mut app), 1);
        for needle in [
            "Prepare main",
            "SETUP ON MAIN · 2 STEPS",
            "Fetch dependencies",
            "cargo fetch --locked",
            "COMMANDS ON MAIN · NO NETWORK",
            "cargo test --workspace",
            "USES",
            "clusia-main-* volumes",
            "with the review, and by Prune",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        let plan = model(&app, &pr)
            .view
            .as_ref()
            .and_then(|v| v.main.plan.clone())
            .expect("a plan");
        click::<PrepareMainStepBox>(&mut app, |b| b.id == STEP_STACK);
        click::<PrepareMainApprove>(&mut app, |b| b.0 == pr);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::PrepareMain {
                pr: pr.clone(),
                main_hash: plan.recipe_hash,
                approve: Approve::Steps(vec![STEP_TESTS.into()]),
            }]
        );
        assert_eq!(model(&app, &pr).pending, Some(CockpitAskKind::PrepareMain));
        assert_eq!(modals_up(&mut app), 0);
    }

    #[test]
    fn a_blocked_plan_cannot_be_approved() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Main);
        view.main.plan.as_mut().expect("plan").blocked = Some("The recipe is unsafe".into());
        let (mut app, pr) = app_with(view);
        edit_model(&mut app, &pr, super::super::list::open_prepare_main);
        assert!(testing::shows(&mut app, "The recipe is unsafe"));
        assert_eq!(testing::count::<PrepareMainApprove>(&mut app), 0);
    }

    #[test]
    fn escape_closes_only_the_modal() {
        let (mut app, pr) = scenario_app();
        open_approval(&mut app, &pr);
        assert_eq!(modals_up(&mut app), 1);
        testing::press_escape(&mut app);
        assert_eq!(model(&app, &pr).modal, None);
        assert_eq!(modals_up(&mut app), 0);
        let sub = |app: &App| app.world().resource::<TestsModels>().0[&pr].sub;
        assert_eq!(sub(&app), SubTab::Cockpit);
        testing::press_escape(&mut app);
        assert_eq!(
            sub(&app),
            SubTab::Cockpit,
            "a second Escape leaves the screen"
        );
    }

    #[test]
    fn cancel_and_the_backdrop_close_it_without_asking() {
        let (mut app, pr) = scenario_app();
        open_approval(&mut app, &pr);
        testing::recorded(&mut app);
        click::<CancelModal>(&mut app, |b| b.0 == pr);
        assert_eq!(model(&app, &pr).modal, None);
        assert!(testing::recorded(&mut app).is_empty());
        click::<OpenApproval>(&mut app, |b| b.pr == pr);
        let root = testing::find::<ModalFor>(&mut app, |m| m.modal == Modal::CockpitApprove);
        testing::click(&mut app, root);
        testing::settle(&mut app);
        assert_eq!(model(&app, &pr).modal, None);
    }

    #[test]
    fn a_modal_waits_for_another_modal() {
        let (mut app, pr) = scenario_app();
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .expect("tab")
            .ui
            .modal = Some(Modal::Finalize);
        edit_model(&mut app, &pr, |m| {
            m.modal = Some(CockpitModal::Remove {
                scenario: REFRESH.into(),
            });
        });
        testing::settle(&mut app);
        let ours = |app: &mut App| {
            let mut q = app.world_mut().query::<&ModalFor>();
            q.iter(app.world()).filter(|m| is_ours(m.modal)).count()
        };
        assert_eq!(ours(&mut app), 0);
        assert!(model(&app, &pr).modal.is_some(), "held, not dropped");
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .expect("tab")
            .ui
            .modal = None;
        testing::settle(&mut app);
        assert_eq!(ours(&mut app), 1);
    }

    #[test]
    fn remove_confirms_before_it_sends() {
        let (mut app, pr) = scenario_app();
        select(&mut app, &pr, scenario_item(REFRESH));
        click::<RemoveScenarioButton>(&mut app, |b| b.pr == pr);
        assert!(testing::recorded(&mut app).is_empty());
        assert!(testing::shows(&mut app, "Remove this scenario?"));
        assert!(testing::shows(&mut app, "Its approval and results go too."));
        click::<RemoveConfirm>(&mut app, |b| b.0 == pr);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RemoveScenario {
                pr: pr.clone(),
                scenario: REFRESH.into()
            }]
        );
        assert_eq!(model(&app, &pr).pending, Some(CockpitAskKind::Remove));
        assert_eq!(modals_up(&mut app), 0);
        // Cancel sends nothing.
        let (mut app, pr) = scenario_app();
        select(&mut app, &pr, scenario_item(REFRESH));
        click::<RemoveScenarioButton>(&mut app, |b| b.pr == pr);
        click::<CancelModal>(&mut app, |b| b.0 == pr);
        assert!(testing::recorded(&mut app).is_empty());
        assert_eq!(modals_up(&mut app), 0);
    }

    #[test]
    fn a_modal_goes_when_the_cockpit_is_not_on_screen() {
        let (mut app, pr) = scenario_app();
        open_approval(&mut app, &pr);
        assert_eq!(modals_up(&mut app), 1);
        app.world_mut()
            .resource_mut::<TestsModels>()
            .0
            .get_mut(&pr)
            .expect("tests")
            .sub = SubTab::Auto;
        testing::settle(&mut app);
        assert_eq!(modals_up(&mut app), 0);
        assert_eq!(model(&app, &pr).modal, None);
    }
}
