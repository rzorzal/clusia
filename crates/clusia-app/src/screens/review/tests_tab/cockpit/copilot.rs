//! The QA copilot under the comparison: its thread, the suggestions it made with **Write it**,
//! the two links that ask it something, and the one-line input. Every ask here goes out on a
//! click or on Enter in the focused input, never on a blur. The thread and the input are drawn
//! apart, so an answer redraws the thread and leaves what the reviewer is typing alone.
//!
//! **Suggest as a test in a comment** lives here too: it puts a scenario, its command and its
//! scripts into the review's general comment as the reviewer's own draft.

use bevy::input::common_conditions::input_just_pressed;
use bevy::input_focus::InputFocus;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui_widgets::{Activate, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::cockpit::{
    BenchAsk, CockpitView, CopilotAsk, CopilotFrom, CopilotMessage, ItemKind, ScenarioAsk,
    Suggestion, copilot_ask_ok, describe_ok, scenario_comment,
};

use super::center::{SuggestAsComment, text_link};
use super::frame::CockpitCopilot;
use super::views::copilot_view;
use super::{CockpitModel, CockpitModels};
use crate::agent_name;
use crate::bridge::{Ask, Asks, CockpitAskKind, Model, Toasts};
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::review_state::{EditTarget, Editor, ReviewSection, ReviewTabs};
use crate::screens::review::diff::unsent;
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::screens::review::tests_tab::functions::FunctionsModels;
use crate::screens::review::tests_tab::plan::{Refocus, Spawner};
use crate::theme::Swatch;
use crate::ui::composer::ComposerMode;
use crate::ui::composer::popover::Placeholder;
use crate::ui::kit::{
    Fill, Stroke, Tone, Type, Variant, badge, button, disabled_button, text, text_field,
    wrapped_text,
};
use crate::ui::spinner::spinner;
use crate::ui::text_area::set_text;

/// What the input says when its text cannot be a question.
pub const TOO_LONG: &str = "At most 2000 characters on one line";

const UNSENT_COMMENT: &str = "Finish or cancel your open comment first";

/// The input's width: the right column's, less its padding and the ↑.
const INPUT_W: f32 = 262.0;

/// The single-line field of the copilot.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CopilotInput(pub PrRef);

/// The green ↑ next to the input.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CopilotSend(pub PrRef);

/// **Write it** under the suggestion `index` of the message `message`.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct WriteIt {
    pub pr: PrRef,
    pub message: u32,
    pub index: usize,
}

/// **Explain this result**.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ExplainThis(pub PrRef);

/// **What should I try next?**
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct TryNext(pub PrRef);

/// The region was given its header, thread and input row.
#[derive(Component)]
pub struct CopilotShell;

/// The scrolling thread of the region.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CopilotThread(pub PrRef);

/// The row of the input and the ↑.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CopilotInputRow(pub PrRef);

/// What a **Write it** asks for: a scenario from its description, or a benchmark of a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteAsk {
    /// The text to describe the scenario with.
    Scenario(String),
    /// The slug of the function to benchmark.
    Bench(String),
}

/// What **Write it** asks for a suggestion, or `None` when nothing fits: a scenario's
/// "<title>: <why>" that is no description, or a benchmark of a function that is neither a
/// listed benchmark nor a changed function. `functions` are the review's `(slug, qualified)`.
pub fn write_ask(
    view: &CockpitView,
    functions: &[(String, String)],
    s: &Suggestion,
) -> Option<WriteAsk> {
    match s.kind {
        ItemKind::Scenario => {
            let text = format!("{}: {}", s.title, s.why);
            describe_ok(&text).then_some(WriteAsk::Scenario(text))
        }
        ItemKind::Bench => {
            let f = s.function.as_deref()?;
            view.benches
                .iter()
                .find(|b| b.slug == f || b.qualified == f)
                .map(|b| b.slug.clone())
                .or_else(|| {
                    functions
                        .iter()
                        .find(|(slug, qualified)| slug == f || qualified == f)
                        .map(|(slug, _)| slug.clone())
                })
                .map(WriteAsk::Bench)
        }
        ItemKind::Load => None,
    }
}

/// What the thread draws.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadDrawn {
    pub messages: Vec<CopilotMessage>,
    /// The (message id, suggestion index) pairs that get **Write it**.
    pub writable: Vec<(u32, usize)>,
    pub busy_line: Option<String>,
    pub failed: Option<String>,
    pub explain: bool,
    pub next: bool,
    pub intro: Option<String>,
}

#[derive(Component)]
pub struct ThreadBuilt(ThreadDrawn);

/// What the input row draws; the text in the field is the reviewer's and is kept.
#[derive(Debug, Clone, PartialEq)]
pub struct InputDrawn {
    pub placeholder: String,
    pub disabled: bool,
    pub error: Option<String>,
}

#[derive(Component)]
pub struct InputBuilt(InputDrawn);

/// The `copilot_sent` the input last saw: when it moves, the question was taken.
#[derive(Component)]
pub struct CopilotSeen(u64);

fn functions_of(functions: &FunctionsModels, pr: &PrRef) -> Vec<(String, String)> {
    functions
        .0
        .get(pr)
        .and_then(|m| m.view.as_ref())
        .map(|v| {
            v.functions
                .iter()
                .map(|f| (f.slug.clone(), f.qualified.clone()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn thread_drawn(
    model: &CockpitModel,
    functions: &[(String, String)],
    agent: &str,
    now: i64,
) -> ThreadDrawn {
    let view = copilot_view(model, agent, now);
    let writable = model
        .view
        .as_ref()
        .map(|v| {
            v.copilot
                .messages
                .iter()
                .flat_map(|m| {
                    m.suggestions
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| write_ask(v, functions, s).is_some())
                        .map(move |(i, _)| (m.id, i))
                })
                .collect()
        })
        .unwrap_or_default();
    ThreadDrawn {
        messages: view.messages,
        writable,
        busy_line: view.busy_line,
        failed: view.failed,
        explain: view.explain,
        next: view.next,
        intro: view.intro,
    }
}

pub fn input_drawn(model: &CockpitModel, agent: &str, now: i64) -> InputDrawn {
    let view = copilot_view(model, agent, now);
    InputDrawn {
        placeholder: view.placeholder,
        disabled: view.disabled,
        error: view.error,
    }
}

/// Whether the copilot's region has anything to compare.
pub fn copilot_changed(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    cockpit: Res<CockpitModels>,
    functions: Res<FunctionsModels>,
) -> bool {
    tabs.is_changed() || model.is_changed() || cockpit.is_changed() || functions.is_changed()
}

const HEADING: Type = Type {
    weight: 700,
    ..Type::STRONG
};

/// Gives a fresh region its header, its thread and its input row; they fill in the same frame.
pub fn shell_copilot(
    mut commands: Commands,
    fonts: Res<UiFonts>,
    regions: Query<(Entity, &CockpitCopilot), Without<CopilotShell>>,
) {
    for (entity, region) in &regions {
        let pr = region.0.clone();
        commands
            .entity(entity)
            .insert(CopilotShell)
            .with_children(|p| {
                p.spawn(Node {
                    padding: UiRect::new(px(16), px(16), px(8), px(8)),
                    flex_shrink: 0.0,
                    ..default()
                })
                .with_child(text(&fonts, "QA copilot", HEADING));
                p.spawn((
                    Node {
                        width: percent(100),
                        flex_grow: 1.0,
                        min_height: px(0),
                        flex_direction: FlexDirection::Column,
                        row_gap: px(10),
                        padding: UiRect::horizontal(px(16)),
                        overflow: Overflow::scroll_y(),
                        ..default()
                    },
                    ScrollArea,
                    ScrollPosition::default(),
                    CopilotThread(pr.clone()),
                ));
                p.spawn((
                    Node {
                        width: percent(100),
                        flex_shrink: 0.0,
                        flex_direction: FlexDirection::Column,
                        row_gap: px(4),
                        padding: UiRect::all(px(16)),
                        ..default()
                    },
                    CopilotInputRow(pr),
                ));
            });
    }
}

/// Redraws the thread when what it shows changed, and keeps it at its newest message.
#[allow(clippy::too_many_arguments)]
pub fn fill_thread(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    clock: Res<Clock>,
    models: Res<CockpitModels>,
    functions: Res<FunctionsModels>,
    fonts: Res<UiFonts>,
    mut threads: Query<(
        Entity,
        &CopilotThread,
        Option<&ThreadBuilt>,
        &mut ScrollPosition,
    )>,
) {
    let now = clock.now();
    for (entity, thread, built, mut scroll) in &mut threads {
        let pr = &thread.0;
        let Some(tab) = tabs.0.get(pr).filter(|t| t.ready().is_some()) else {
            continue;
        };
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let default = CockpitModel::default();
        let model = models.0.get(pr).unwrap_or(&default);
        let drawn = thread_drawn(model, &functions_of(&functions, pr), &agent, now);
        if built.is_some_and(|b| b.0 == drawn) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| draw_thread(p, &fonts, pr, &drawn));
        commands.entity(entity).insert(ThreadBuilt(drawn));
        scroll.y = f32::MAX;
    }
}

fn draw_thread(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, drawn: &ThreadDrawn) {
    if let Some(intro) = &drawn.intro {
        p.spawn(wrapped_text(
            fonts,
            intro.clone(),
            Type::MUTED.ink(Swatch::Faint),
        ));
    }
    for m in &drawn.messages {
        match m.from {
            CopilotFrom::Reviewer => {
                p.spawn(Node {
                    width: percent(100),
                    justify_content: JustifyContent::FlexEnd,
                    padding: UiRect::left(px(40)),
                    flex_shrink: 0.0,
                    ..default()
                })
                .with_child((
                    Node {
                        padding: UiRect::axes(px(12), px(8)),
                        border_radius: BorderRadius::all(px(8)),
                        min_width: px(0),
                        ..default()
                    },
                    BackgroundColor::default(),
                    Fill(Swatch::Chrome),
                    children![wrapped_text(fonts, m.text.clone(), Type::BODY)],
                ));
            }
            CopilotFrom::Agent => {
                p.spawn((
                    Node {
                        width: percent(100),
                        padding: UiRect::axes(px(12), px(10)),
                        border: px(1).all(),
                        border_radius: BorderRadius::all(px(8)),
                        flex_shrink: 0.0,
                        ..default()
                    },
                    BorderColor::default(),
                    Stroke(Swatch::Line),
                ))
                .with_child(wrapped_text(fonts, m.text.clone(), Type::BODY));
            }
        }
        for (i, s) in m.suggestions.iter().enumerate() {
            suggestion_row(
                p,
                fonts,
                pr,
                s,
                drawn.writable.contains(&(m.id, i)).then_some((m.id, i)),
            );
        }
    }
    if let Some(line) = &drawn.busy_line {
        p.spawn(Node {
            align_items: AlignItems::Center,
            column_gap: px(8),
            flex_shrink: 0.0,
            ..default()
        })
        .with_children(|row| {
            row.spawn(spinner());
            row.spawn(text(fonts, line.clone(), Type::MUTED));
        });
    }
    if let Some(failed) = &drawn.failed {
        p.spawn(wrapped_text(
            fonts,
            failed.clone(),
            Type::BODY.ink(Swatch::Orange),
        ));
    }
    if drawn.explain || drawn.next {
        p.spawn(Node {
            flex_wrap: FlexWrap::Wrap,
            column_gap: px(14),
            row_gap: px(4),
            flex_shrink: 0.0,
            ..default()
        })
        .with_children(|links| {
            if drawn.explain {
                links.spawn((
                    text_link(fonts, "Explain this result"),
                    ExplainThis(pr.clone()),
                    observe(on_explain),
                ));
            }
            if drawn.next {
                links.spawn((
                    text_link(fonts, "What should I try next?"),
                    TryNext(pr.clone()),
                    observe(on_next),
                ));
            }
        });
    }
}

fn suggestion_row(
    p: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    s: &Suggestion,
    write: Option<(u32, usize)>,
) {
    let (pill, tone) = match s.kind {
        ItemKind::Bench => ("benchmark", Tone::Green),
        _ => ("scenario", Tone::Blue),
    };
    p.spawn((
        Node {
            width: percent(100),
            align_items: AlignItems::FlexStart,
            column_gap: px(8),
            padding: UiRect::axes(px(10), px(8)),
            border: px(1).all(),
            border_radius: BorderRadius::all(px(6)),
            flex_shrink: 0.0,
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|row| {
        row.spawn(badge(fonts, pill, tone));
        row.spawn(Node {
            flex_grow: 1.0,
            flex_basis: px(0),
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            ..default()
        })
        .with_children(|c| {
            c.spawn(wrapped_text(fonts, s.title.clone(), Type::BODY));
            if !s.why.is_empty() {
                c.spawn(wrapped_text(fonts, s.why.clone(), Type::META));
            }
        });
        if let Some((message, index)) = write {
            row.spawn((
                text_link(fonts, "Write it"),
                WriteIt {
                    pr: pr.clone(),
                    message,
                    index,
                },
                observe(on_write_it),
            ));
        }
    });
}

/// Redraws the input row when its placeholder, state or error changed, keeping the reviewer's
/// text and focus; empties the field once a question was taken; takes the focus when asked to.
#[allow(clippy::too_many_arguments)]
pub fn fill_input(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    clock: Res<Clock>,
    mut models: ResMut<CockpitModels>,
    fonts: Res<UiFonts>,
    focus: Res<InputFocus>,
    rows: Query<(
        Entity,
        &CopilotInputRow,
        Option<&InputBuilt>,
        Option<&CopilotSeen>,
    )>,
    mut inputs: Query<(Entity, &CopilotInput, &mut EditableText)>,
) {
    let now = clock.now();
    for (entity, row, built, seen) in &rows {
        let pr = &row.0;
        let Some(tab) = tabs.0.get(pr).filter(|t| t.ready().is_some()) else {
            continue;
        };
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let default = CockpitModel::default();
        let model = models.0.get(pr).unwrap_or(&default);
        let drawn = input_drawn(model, &agent, now);
        let sent = model.copilot_sent;
        let wants_focus = model.copilot_focus;
        let clear = seen.is_some_and(|s| s.0 != sent);
        let field = inputs.iter_mut().find(|(_, i, _)| i.0 == *pr);
        if built.is_some_and(|b| b.0 == drawn) {
            if let Some((input, _, mut editable)) = field {
                if clear {
                    set_text(&mut editable, "");
                }
                if wants_focus {
                    commands.entity(input).insert(Refocus);
                }
            }
        } else {
            let (typed, focused) = match field {
                Some((input, _, editable)) => (
                    if clear {
                        String::new()
                    } else {
                        editable.value().to_string()
                    },
                    focus.get() == Some(input),
                ),
                None => (String::new(), false),
            };
            commands.entity(entity).despawn_related::<Children>();
            commands.entity(entity).with_children(|p| {
                draw_input(p, &fonts, pr, &drawn, &typed, focused || wants_focus);
            });
            commands.entity(entity).insert(InputBuilt(drawn));
        }
        commands.entity(entity).insert(CopilotSeen(sent));
        if wants_focus && let Some(m) = models.bypass_change_detection().0.get_mut(pr) {
            m.copilot_focus = false;
        }
    }
}

fn draw_input(
    p: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    drawn: &InputDrawn,
    typed: &str,
    focused: bool,
) {
    p.spawn(Node {
        width: percent(100),
        align_items: AlignItems::Center,
        column_gap: px(8),
        ..default()
    })
    .with_children(|row| {
        row.spawn(Node {
            flex_grow: 1.0,
            min_width: px(0),
            ..default()
        })
        .with_children(|wrap| {
            let mut field = wrap.spawn((
                text_field(fonts, typed, INPUT_W, false),
                CopilotInput(pr.clone()),
            ));
            if focused {
                field.insert(Refocus);
            }
            let field = field.id();
            wrap.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: px(11),
                    top: px(7),
                    display: if typed.is_empty() {
                        Display::Flex
                    } else {
                        Display::None
                    },
                    ..default()
                },
                Pickable::IGNORE,
                Placeholder(field),
                text(
                    fonts,
                    drawn.placeholder.clone(),
                    Type::MUTED.ink(Swatch::Faint),
                ),
            ));
        });
        if drawn.disabled {
            row.spawn(disabled_button(fonts, "↑"));
        } else {
            row.spawn((
                button(fonts, "↑", Variant::Primary),
                CopilotSend(pr.clone()),
                observe(on_send),
            ));
        }
    });
    if let Some(error) = &drawn.error {
        p.spawn(wrapped_text(
            fonts,
            error.clone(),
            Type::META.ink(Swatch::Orange),
        ));
    }
}

/// Whether the review may send now; a toast says why not.
fn may_send(
    tabs: &ReviewTabs,
    app_model: &Model,
    pr: &PrRef,
    toasts: &mut Toasts,
    time: &Time,
) -> bool {
    match read_only_reason(tabs, app_model, pr) {
        Some(reason) => {
            not_sent(toasts, time, reason);
            false
        }
        None => true,
    }
}

/// Whether the copilot can take a question now.
fn copilot_free(model: &CockpitModel) -> bool {
    model.pending != Some(CockpitAskKind::Copilot)
        && !model.view.as_ref().is_some_and(|v| v.copilot.busy)
}

/// Sends the input's text as a question about the selected item, or says why it cannot be one.
fn send_free(model: &mut CockpitModel, pr: &PrRef, text: &str, asks: &mut Asks) {
    if !copilot_free(model) || text.trim().is_empty() {
        return;
    }
    if !copilot_ask_ok(text) {
        if model.copilot_error.as_deref() != Some(TOO_LONG) {
            model.copilot_error = Some(TOO_LONG.into());
            model.touch();
        }
        return;
    }
    model.copilot_error = None;
    asks.send(Ask::AskCopilot {
        pr: pr.clone(),
        item: model.selected.clone(),
        ask: CopilotAsk::Free {
            text: text.to_string(),
        },
    });
    model.sent(CockpitAskKind::Copilot);
}

#[allow(clippy::too_many_arguments)]
fn on_send(
    activate: On<Activate>,
    buttons: Query<&CopilotSend>,
    inputs: Query<(&CopilotInput, &EditableText)>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(CopilotSend(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let Some(text) = inputs
        .iter()
        .find(|(i, _)| i.0 == *pr)
        .map(|(_, e)| e.value().to_string())
    else {
        return;
    };
    if !may_send(&tabs, &app_model, pr, &mut toasts, &time) {
        return;
    }
    if let Some(model) = models.0.get_mut(pr) {
        send_free(model, pr, &text, &mut asks);
    }
}

/// Enter in the focused input sends its text, as the ↑ does.
#[allow(clippy::too_many_arguments)]
pub fn send_on_enter(
    focus: Res<InputFocus>,
    inputs: Query<(&CopilotInput, &EditableText)>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Some((CopilotInput(pr), editable)) = focus.get().and_then(|e| inputs.get(e).ok()) else {
        return;
    };
    let text = editable.value().to_string();
    if !may_send(&tabs, &app_model, pr, &mut toasts, &time) {
        return;
    }
    if let Some(model) = models.0.get_mut(pr) {
        send_free(model, pr, &text, &mut asks);
    }
}

#[allow(clippy::too_many_arguments)]
fn on_explain(
    activate: On<Activate>,
    buttons: Query<&ExplainThis>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(ExplainThis(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if !may_send(&tabs, &app_model, pr, &mut toasts, &time) {
        return;
    }
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if !copilot_free(model) || model.selected.is_none() {
        return;
    }
    asks.send(Ask::AskCopilot {
        pr: pr.clone(),
        item: model.selected.clone(),
        ask: CopilotAsk::Explain,
    });
    model.sent(CockpitAskKind::Copilot);
}

#[allow(clippy::too_many_arguments)]
fn on_next(
    activate: On<Activate>,
    buttons: Query<&TryNext>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(TryNext(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if !may_send(&tabs, &app_model, pr, &mut toasts, &time) {
        return;
    }
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if !copilot_free(model) {
        return;
    }
    asks.send(Ask::AskCopilot {
        pr: pr.clone(),
        item: None,
        ask: CopilotAsk::Next,
    });
    model.sent(CockpitAskKind::Copilot);
}

#[allow(clippy::too_many_arguments)]
fn on_write_it(
    activate: On<Activate>,
    buttons: Query<&WriteIt>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    functions: Res<FunctionsModels>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(WriteIt { pr, message, index }) = buttons.get(activate.entity) else {
        return;
    };
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    // One write at a time: a second click before the answer sends nothing.
    if model.pending.is_some() || model.busy() || model.tests_busy {
        return;
    }
    if !may_send(&tabs, &app_model, pr, &mut toasts, &time) {
        return;
    }
    let Some(view) = model.view.as_ref() else {
        return;
    };
    let Some(s) = view
        .copilot
        .messages
        .iter()
        .find(|m| m.id == *message)
        .and_then(|m| m.suggestions.get(*index))
    else {
        return;
    };
    let ask = match write_ask(view, &functions_of(&functions, pr), s) {
        Some(WriteAsk::Scenario(text)) => Ask::WriteScenarios {
            pr: pr.clone(),
            ask: ScenarioAsk::Describe { text },
        },
        Some(WriteAsk::Bench(function)) => Ask::WriteBench {
            pr: pr.clone(),
            function,
            ask: BenchAsk::Fresh,
        },
        None => return,
    };
    asks.send(ask);
    model.sent(CockpitAskKind::Write);
}

/// **Suggest as a test in a comment**: goes on at once with the scenario's script in hand, or
/// asks for it and goes on when it lands (`suggest_when_script`).
#[allow(clippy::too_many_arguments)]
pub fn on_suggest(
    activate: On<Activate>,
    buttons: Query<&SuggestAsComment>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(SuggestAsComment { pr, scenario }) = buttons.get(activate.entity) else {
        return;
    };
    if !may_send(&tabs, &app_model, pr, &mut toasts, &time) {
        return;
    }
    if tabs.0.get(pr).is_some_and(|t| unsent(t.ui.editor.as_ref())) {
        return not_sent(&mut toasts, &time, UNSENT_COMMENT);
    }
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if model
        .selected_scenario()
        .is_none_or(|s| s.slug != *scenario)
    {
        return;
    }
    // A click is the reviewer's own retry of a script that could not be read.
    if model.script_error.is_some() {
        model.script_asked = None;
        model.script_error = None;
    }
    model.suggest_after_script = true;
    if let Some(ask) = model.want_script(pr) {
        asks.send(ask);
    }
    model.touch();
}

/// Fills the review's general comment with the selected scenario once its script for the current
/// approval is held, and switches to the Comments section; a script that cannot be read says so.
pub fn suggest_when_script(
    mut models: ResMut<CockpitModels>,
    mut tabs: ResMut<ReviewTabs>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let mut filled = false;
    for (pr, model) in &mut models.bypass_change_detection().0 {
        if !model.suggest_after_script {
            continue;
        }
        if let Some(error) = model.script_error.clone() {
            model.suggest_after_script = false;
            not_sent(&mut toasts, &time, &error);
            continue;
        }
        let Some(s) = model.selected_scenario() else {
            model.suggest_after_script = false;
            continue;
        };
        let Some(script) = model
            .script
            .as_ref()
            .filter(|sc| sc.scenario == s.slug && sc.approval_hash == s.approval_hash)
        else {
            continue;
        };
        let text = scenario_comment(s, script);
        model.suggest_after_script = false;
        let Some(tab) = tabs.bypass_change_detection().0.get_mut(pr) else {
            continue;
        };
        if unsent(tab.ui.editor.as_ref()) {
            not_sent(&mut toasts, &time, UNSENT_COMMENT);
            continue;
        }
        tab.ui.editor = Some(Editor {
            target: EditTarget::General,
            text,
            error: None,
            ticket: None,
            mode: ComposerMode::Write,
        });
        tab.ui.section = ReviewSection::Comments;
        filled = true;
    }
    if filled {
        tabs.set_changed();
    }
}

/// The copilot's systems and the observer of **Suggest as a test in a comment**.
pub fn register(app: &mut App) {
    app.add_observer(on_suggest).add_systems(
        Update,
        (
            shell_copilot.run_if(super::frame::regions_changed),
            fill_thread.run_if(copilot_changed),
            fill_input.run_if(copilot_changed),
            send_on_enter.run_if(input_just_pressed(KeyCode::Enter)),
            suggest_when_script.run_if(resource_changed::<CockpitModels>),
        )
            .chain()
            .after(super::modals::cancel_cockpit_modal),
    );
}

#[cfg(test)]
mod tests {
    use bevy::input::ButtonInput;
    use bevy::input::keyboard::KeyCode;
    use clusia_core::cockpit::{COPILOT_BUSY, COPILOT_TEXT_MAX, ItemRef, ScenarioScript};

    use super::super::center::support::{self, REFRESH, scenario_item};
    use super::super::frame::CockpitCompare;
    use super::*;
    use crate::bridge::{CockpitTell, Tell};
    use crate::fixture::{self, DemoCockpit, demo_cockpit};
    use crate::screens::review::editor::EditorArea;
    use crate::testing::{self, NOW};
    use crate::ui::spinner::Spinner;

    fn scenario() -> CockpitView {
        demo_cockpit(NOW, DemoCockpit::Scenario)
    }

    /// The texts under the one `C`, in the order they are drawn.
    fn texts_under<C: Component>(app: &mut App) -> Vec<String> {
        fn walk(world: &World, e: Entity, out: &mut Vec<String>) {
            if let Some(t) = world.get::<Text>(e) {
                out.push(t.0.clone());
            }
            if let Some(children) = world.get::<Children>(e) {
                for c in children.iter() {
                    walk(world, c, out);
                }
            }
        }
        let root = testing::find::<C>(app, |_| true);
        let mut out = Vec::new();
        walk(app.world(), root, &mut out);
        out
    }

    fn thread_texts(app: &mut App) -> Vec<String> {
        texts_under::<CopilotThread>(app)
    }

    fn input(app: &mut App) -> Entity {
        testing::find::<CopilotInput>(app, |_| true)
    }

    fn input_text(app: &mut App) -> String {
        let e = input(app);
        app.world()
            .get::<EditableText>(e)
            .expect("a field")
            .value()
            .to_string()
    }

    fn type_text(app: &mut App, text: &str) {
        let e = input(app);
        testing::type_into(app, e, text);
        testing::settle(app);
    }

    fn press_enter(app: &mut App) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Enter);
        testing::settle(app);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .release(KeyCode::Enter);
        testing::settle(app);
    }

    fn toasts(app: &App) -> Vec<String> {
        app.world()
            .resource::<Toasts>()
            .0
            .iter()
            .map(|t| t.text.clone())
            .collect()
    }

    fn free(pr: &PrRef, item: Option<ItemRef>, text: &str) -> Ask {
        Ask::AskCopilot {
            pr: pr.clone(),
            item,
            ask: CopilotAsk::Free { text: text.into() },
        }
    }

    #[test]
    fn the_thread_draws_reviewer_bubbles_and_agent_cards_in_order() {
        let (mut app, _pr) = support::app_with(scenario());
        let all = thread_texts(&mut app);
        let first = all
            .iter()
            .position(|t| t == "Why does memory grow?")
            .expect("the question");
        let second = all
            .iter()
            .position(|t| t.starts_with("Memory grows for the whole run"))
            .expect("the answer");
        assert!(first < second);
        assert!(all.iter().any(|t| t == "Waiters that time out are dropped"));
        assert!(all.iter().any(|t| t == "scenario"));
        assert!(all.iter().any(|t| t == "Write it"));
        assert!(
            texts_under::<CockpitCopilot>(&mut app)
                .iter()
                .any(|t| t == "QA copilot")
        );
    }

    #[test]
    fn hostile_copilot_text_is_plain_printable_and_bounded() {
        let mut view = scenario();
        view.copilot.messages[0].text = "[x](http://evil) **bold** \u{202e}".into();
        view.copilot.messages[1].text = "r".repeat(10_000);
        view.copilot.messages[1].suggestions[0].title = "**t** \u{202e}".into();
        let (mut app, _pr) = support::app_with(view);
        let all = thread_texts(&mut app);
        assert!(
            all.iter().any(|t| t.contains("[x](http://evil) **bold**")),
            "{all:?}"
        );
        for t in &all {
            assert!(!t.contains('\u{202e}'), "{t}");
            assert!(t.chars().count() <= COPILOT_TEXT_MAX);
        }
        assert_eq!(testing::count::<crate::ui::markdown::MdLink>(&mut app), 0);
        assert_eq!(testing::count::<crate::ui::markdown::MdImage>(&mut app), 0);
    }

    #[test]
    fn write_it_for_a_scenario_suggestion_sends_describe_with_title_and_why() {
        let (mut app, pr) = support::app_with(scenario());
        support::click::<WriteIt>(&mut app, |w| w.pr == pr);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::WriteScenarios {
                pr: pr.clone(),
                ask: ScenarioAsk::Describe {
                    text: "Waiters that time out are dropped: A caller that gives up must leave \
                           the refresh queue"
                        .into()
                }
            }]
        );
        // A second click before the answer sends nothing.
        support::click::<WriteIt>(&mut app, |w| w.pr == pr);
        assert_eq!(testing::recorded(&mut app), []);
    }

    #[test]
    fn write_it_is_not_drawn_when_describe_ok_fails() {
        let mut long = scenario();
        long.copilot.messages[1].suggestions[0].why = "w".repeat(700);
        let (mut app, _pr) = support::app_with(long);
        assert_eq!(testing::count::<WriteIt>(&mut app), 0);
        let mut lines = scenario();
        lines.copilot.messages[1].suggestions[0].why = "one\ntwo".into();
        let (mut app, _pr) = support::app_with(lines);
        assert_eq!(testing::count::<WriteIt>(&mut app), 0);
    }

    fn bench_suggestion(function: Option<&str>) -> Suggestion {
        Suggestion {
            kind: ItemKind::Bench,
            title: "Backoff at the cap".into(),
            why: "The new cap applies from attempt 7".into(),
            function: function.map(str::to_string),
        }
    }

    #[test]
    fn write_it_for_a_benchmark_matches_a_listed_bench_or_a_changed_function() {
        let view = scenario();
        let slug = view.benches[0].slug.clone();
        let functions = vec![("store-x".to_string(), "Store::x".to_string())];
        for f in [slug.as_str(), "backoff_delay"] {
            assert_eq!(
                write_ask(&view, &functions, &bench_suggestion(Some(f))),
                Some(WriteAsk::Bench(slug.clone()))
            );
        }
        for f in ["store-x", "Store::x"] {
            assert_eq!(
                write_ask(&view, &functions, &bench_suggestion(Some(f))),
                Some(WriteAsk::Bench("store-x".into()))
            );
        }
        assert_eq!(
            write_ask(&view, &functions, &bench_suggestion(Some("nope"))),
            None
        );
        assert_eq!(write_ask(&view, &functions, &bench_suggestion(None)), None);

        let mut with_bench = scenario();
        with_bench.copilot.messages[1].suggestions = vec![bench_suggestion(Some("backoff_delay"))];
        let (mut app, pr) = support::app_with(with_bench);
        assert!(thread_texts(&mut app).iter().any(|t| t == "benchmark"));
        support::click::<WriteIt>(&mut app, |w| w.pr == pr);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::WriteBench {
                pr: pr.clone(),
                function: slug,
                ask: BenchAsk::Fresh
            }]
        );
        let mut unmatched = scenario();
        unmatched.copilot.messages[1].suggestions = vec![bench_suggestion(Some("nope"))];
        let (mut app, _pr) = support::app_with(unmatched);
        assert_eq!(testing::count::<WriteIt>(&mut app), 0);
    }

    fn busy() -> CockpitView {
        let mut view = scenario();
        view.copilot.busy = true;
        view
    }

    #[test]
    fn busy_shows_the_answering_line_and_disables_the_input() {
        let (mut app, _pr) = support::app_with(busy());
        let all = thread_texts(&mut app);
        assert!(all.iter().any(|t| t.ends_with(" is answering…")), "{all:?}");
        assert!(testing::count::<Spinner>(&mut app) >= 1);
        assert!(
            texts_under::<CopilotInputRow>(&mut app)
                .iter()
                .any(|t| t == COPILOT_BUSY)
        );
        assert_eq!(testing::count::<CopilotSend>(&mut app), 0);
        type_text(&mut app, "why?");
        let field = input(&mut app);
        support::focus(&mut app, field);
        press_enter(&mut app);
        assert_eq!(testing::recorded(&mut app), []);
    }

    #[test]
    fn failed_shows_an_orange_line() {
        let mut view = scenario();
        view.copilot.failed = Some("Claude Code stopped".into());
        let (mut app, _pr) = support::app_with(view);
        let thread = testing::find::<CopilotThread>(&mut app, |_| true);
        let ink = support::descendants(&app, thread)
            .into_iter()
            .find(|e| {
                app.world()
                    .get::<Text>(*e)
                    .is_some_and(|t| t.0 == "Claude Code stopped")
            })
            .and_then(|e| app.world().get::<crate::ui::kit::Ink>(e).copied());
        assert_eq!(ink.map(|i| i.0), Some(Swatch::Orange));
    }

    #[test]
    fn the_input_sends_free_on_enter_or_the_arrow_never_on_blur() {
        let (mut app, pr) = support::app_with(scenario());
        let selected = support::model(&app, &pr).selected.clone();
        let field = input(&mut app);
        type_text(&mut app, "why is it slower?");
        // A blur commits the field; that is no question.
        app.world_mut()
            .write_message(crate::ui::kit::FieldCommitted {
                entity: field,
                value: "why is it slower?".into(),
            });
        app.world_mut().resource_mut::<InputFocus>().clear();
        testing::settle(&mut app);
        assert_eq!(testing::recorded(&mut app), []);
        support::click::<CopilotSend>(&mut app, |b| b.0 == pr);
        assert_eq!(
            testing::recorded(&mut app),
            [free(&pr, selected.clone(), "why is it slower?")]
        );
        support::tell(
            &mut app,
            CockpitTell::Acked {
                pr: pr.clone(),
                kind: CockpitAskKind::Copilot,
            },
        );
        type_text(&mut app, "and on main?");
        let field = input(&mut app);
        support::focus(&mut app, field);
        press_enter(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [free(&pr, selected, "and on main?")]
        );
    }

    #[test]
    fn text_that_fails_copilot_ask_ok_shows_the_orange_error_and_sends_nothing() {
        let (mut app, pr) = support::app_with(scenario());
        let long = "a".repeat(2001);
        type_text(&mut app, &long);
        support::click::<CopilotSend>(&mut app, |b| b.0 == pr);
        assert_eq!(testing::recorded(&mut app), []);
        assert!(
            texts_under::<CopilotInputRow>(&mut app)
                .iter()
                .any(|t| t == TOO_LONG)
        );
        assert_eq!(input_text(&mut app), long, "the text stays");
        // The next good question clears it.
        let e = input(&mut app);
        if let Some(mut t) = app.world_mut().get_mut::<EditableText>(e) {
            set_text(&mut t, "");
        }
        testing::settle(&mut app);
        type_text(&mut app, "ok?");
        support::click::<CopilotSend>(&mut app, |b| b.0 == pr);
        assert_eq!(testing::recorded(&mut app).len(), 1);
        assert!(
            !texts_under::<CopilotInputRow>(&mut app)
                .iter()
                .any(|t| t == TOO_LONG)
        );
    }

    #[test]
    fn the_field_clears_when_the_ack_lands_and_keeps_its_text_on_a_refusal() {
        let (mut app, pr) = support::app_with(scenario());
        type_text(&mut app, "why?");
        support::click::<CopilotSend>(&mut app, |b| b.0 == pr);
        assert_eq!(testing::recorded(&mut app).len(), 1);
        support::tell(
            &mut app,
            CockpitTell::Refused {
                pr: pr.clone(),
                kind: CockpitAskKind::Copilot,
                message: "busy \u{202e}now".into(),
            },
        );
        assert_eq!(input_text(&mut app), "why?");
        assert!(
            toasts(&app)
                .iter()
                .any(|t| t == &clusia_core::printable::printable("busy \u{202e}now")),
            "{:?}",
            toasts(&app)
        );
        support::click::<CopilotSend>(&mut app, |b| b.0 == pr);
        assert_eq!(testing::recorded(&mut app).len(), 1);
        support::tell(
            &mut app,
            CockpitTell::Acked {
                pr: pr.clone(),
                kind: CockpitAskKind::Copilot,
            },
        );
        assert_eq!(input_text(&mut app), "");
    }

    #[test]
    fn explain_and_next_links_send_their_asks() {
        let (mut app, pr) = support::app_with(scenario());
        // The first scenario's comparison is finished and no message is about it.
        let selected = support::model(&app, &pr).selected.clone();
        assert_eq!(
            selected,
            Some(scenario_item("two-requests-refresh-at-once"))
        );
        support::click::<ExplainThis>(&mut app, |b| b.0 == pr);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AskCopilot {
                pr: pr.clone(),
                item: selected,
                ask: CopilotAsk::Explain
            }]
        );
        support::tell(
            &mut app,
            CockpitTell::Acked {
                pr: pr.clone(),
                kind: CockpitAskKind::Copilot,
            },
        );
        support::click::<TryNext>(&mut app, |b| b.0 == pr);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AskCopilot {
                pr: pr.clone(),
                item: None,
                ask: CopilotAsk::Next
            }]
        );
        // The refresh scenario has a message about it: no Explain link.
        support::select(&mut app, &pr, scenario_item(REFRESH));
        assert_eq!(testing::count::<ExplainThis>(&mut app), 0);
    }

    #[test]
    fn the_empty_view_uses_the_other_placeholder() {
        let (mut app, _pr) = support::app_with(demo_cockpit(NOW, DemoCockpit::Empty));
        assert!(
            texts_under::<CopilotInputRow>(&mut app)
                .iter()
                .any(|t| t == "Ask for a scenario, or what to try next…")
        );
        let (mut app, _pr) = support::app_with(scenario());
        assert!(
            texts_under::<CopilotInputRow>(&mut app)
                .iter()
                .any(|t| t == "Ask why, or what to try next…")
        );
    }

    #[test]
    fn the_ask_the_copilot_button_focuses_the_input_and_sends_nothing() {
        let (mut app, pr) = support::app_with(scenario());
        support::click::<super::super::list::CockpitAskScenario>(&mut app, |b| b.0 == pr);
        testing::settle(&mut app);
        assert_eq!(
            app.world().resource::<InputFocus>().get(),
            Some(input(&mut app))
        );
        assert!(!support::model(&app, &pr).copilot_focus);
        assert_eq!(testing::recorded(&mut app), []);
    }

    fn thread_children(app: &mut App) -> Vec<Entity> {
        let e = testing::find::<CopilotThread>(app, |_| true);
        app.world()
            .get::<Children>(e)
            .map(|c| c.iter().collect())
            .unwrap_or_default()
    }

    #[test]
    fn a_new_message_sticks_the_thread_to_the_bottom() {
        let (mut app, pr) = support::app_with(scenario());
        let thread = testing::find::<CopilotThread>(&mut app, |_| true);
        app.world_mut()
            .get_mut::<ScrollPosition>(thread)
            .expect("a scroll")
            .y = 0.0;
        let mut view = scenario();
        view.copilot.messages.push(CopilotMessage {
            id: 3,
            at: NOW,
            from: CopilotFrom::Agent,
            item: None,
            text: "Try fewer callers.".into(),
            suggestions: Vec::new(),
        });
        support::changed(&mut app, view);
        let _ = pr;
        assert_eq!(
            app.world()
                .get::<ScrollPosition>(thread)
                .expect("a scroll")
                .y,
            f32::MAX
        );
    }

    #[test]
    fn the_thread_rebuilds_only_when_its_view_changes() {
        let view = demo_cockpit(NOW, DemoCockpit::BenchRunning);
        let item = ItemRef {
            kind: ItemKind::Bench,
            id: view.benches[0].slug.clone(),
        };
        let (mut app, pr) = support::app_with(view);
        support::select(&mut app, &pr, item.clone());
        let thread = thread_children(&mut app);
        let row = texts_under::<CopilotInputRow>(&mut app);
        let field = input(&mut app);
        support::tell(
            &mut app,
            CockpitTell::Samples {
                pr: pr.clone(),
                run: "demo-bench-run".into(),
                item,
                side: clusia_core::cockpit::RunSide::Main,
                round: 3,
                points: vec![clusia_core::cockpit::Sample {
                    t_ms: 0,
                    rss_kb: Some(1),
                    cpu_milli: Some(1),
                }],
            },
        );
        assert_eq!(thread_children(&mut app), thread);
        assert_eq!(input(&mut app), field);
        assert_eq!(texts_under::<CopilotInputRow>(&mut app), row);
        let _ = testing::find::<CockpitCompare>(&mut app, |_| true);
    }

    // Suggest as a test in a comment.

    fn refresh_script() -> ScenarioScript {
        fixture::demo_cockpit_script(REFRESH).expect("a script")
    }

    fn refresh_app() -> (App, PrRef) {
        let (mut app, pr) = support::app_with(scenario());
        support::select(&mut app, &pr, scenario_item(REFRESH));
        (app, pr)
    }

    fn land_script(app: &mut App, pr: &PrRef, script: ScenarioScript) {
        support::tell(
            app,
            CockpitTell::Script {
                pr: pr.clone(),
                script: Box::new(script),
            },
        );
    }

    fn refresh_view(app: &App, pr: &PrRef) -> clusia_core::cockpit::ScenarioView {
        support::model(app, pr)
            .selected_scenario()
            .expect("refresh")
            .clone()
    }

    fn click_suggest(app: &mut App, pr: &PrRef) {
        support::click::<SuggestAsComment>(app, |b| b.pr == *pr);
        testing::settle(app);
    }

    #[test]
    fn suggest_with_the_current_script_fills_the_general_comment() {
        let (mut app, pr) = refresh_app();
        land_script(&mut app, &pr, refresh_script());
        let view = refresh_view(&app, &pr);
        click_suggest(&mut app, &pr);
        let tab = testing::tab(&app, &pr);
        let editor = tab.ui.editor.expect("an editor");
        assert_eq!(editor.target, EditTarget::General);
        assert_eq!(editor.text, scenario_comment(&view, &refresh_script()));
        assert_eq!(editor.mode, ComposerMode::Write);
        assert_eq!(tab.ui.section, ReviewSection::Comments);
        testing::settle(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |a| a.0 == pr);
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(area));
        // The Comments section loads its own pictures; the click asked the daemon nothing.
        let asked: Vec<Ask> = testing::recorded(&mut app)
            .into_iter()
            .filter(|a| !matches!(a, Ask::FetchMedia(_)))
            .collect();
        assert_eq!(asked, []);
    }

    #[test]
    fn suggest_without_the_script_asks_for_it_then_goes_on() {
        let (mut app, pr) = refresh_app();
        // The read the selection started could not be done.
        support::tell(
            &mut app,
            CockpitTell::Refused {
                pr: pr.clone(),
                kind: CockpitAskKind::Script,
                message: "busy".into(),
            },
        );
        testing::recorded(&mut app);
        click_suggest(&mut app, &pr);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::GetScenarioScript {
                pr: pr.clone(),
                scenario: REFRESH.into()
            }]
        );
        assert!(testing::tab(&app, &pr).ui.editor.is_none());
        land_script(&mut app, &pr, refresh_script());
        let view = refresh_view(&app, &pr);
        let editor = testing::tab(&app, &pr).ui.editor.expect("an editor");
        assert_eq!(editor.text, scenario_comment(&view, &refresh_script()));
        assert!(!support::model(&app, &pr).suggest_after_script);
    }

    #[test]
    fn suggest_while_the_script_is_on_its_way_waits_for_it() {
        let (mut app, pr) = refresh_app();
        click_suggest(&mut app, &pr);
        assert_eq!(testing::recorded(&mut app), [], "asked already");
        assert!(testing::tab(&app, &pr).ui.editor.is_none());
        land_script(&mut app, &pr, refresh_script());
        assert!(testing::tab(&app, &pr).ui.editor.is_some());
    }

    #[test]
    fn suggest_with_a_stale_script_asks_again() {
        let (mut app, pr) = refresh_app();
        land_script(&mut app, &pr, refresh_script());
        let mut view = scenario();
        view.scenarios[1].approval_hash = "changed".into();
        support::changed(&mut app, view);
        click_suggest(&mut app, &pr);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::GetScenarioScript {
                pr: pr.clone(),
                scenario: REFRESH.into()
            }]
        );
        assert!(
            testing::tab(&app, &pr).ui.editor.is_none(),
            "not the old script"
        );
        let mut fresh = refresh_script();
        fresh.approval_hash = "changed".into();
        land_script(&mut app, &pr, fresh.clone());
        let view = refresh_view(&app, &pr);
        let editor = testing::tab(&app, &pr).ui.editor.expect("an editor");
        assert_eq!(editor.text, scenario_comment(&view, &fresh));
    }

    #[test]
    fn suggest_with_unsent_text_in_the_editor_toasts_and_does_nothing() {
        let (mut app, pr) = refresh_app();
        land_script(&mut app, &pr, refresh_script());
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .expect("a tab")
            .ui
            .editor = Some(Editor {
            target: EditTarget::General,
            text: "half a thought".into(),
            error: None,
            ticket: None,
            mode: ComposerMode::Write,
        });
        click_suggest(&mut app, &pr);
        assert!(toasts(&app).iter().any(|t| t == UNSENT_COMMENT));
        let tab = testing::tab(&app, &pr);
        assert_eq!(tab.ui.editor.expect("the editor").text, "half a thought");
        assert_eq!(tab.ui.section, ReviewSection::Tests);
        assert_eq!(testing::recorded(&mut app), []);
        assert!(!support::model(&app, &pr).suggest_after_script);
    }

    #[test]
    fn the_suggested_comment_text_is_the_reviewers_draft_not_markdown_rendered_by_the_cockpit() {
        let mut view = scenario();
        view.scenarios[1].title = "[x](http://evil) **b**".into();
        let (mut app, pr) = support::app_with(view);
        support::select(&mut app, &pr, scenario_item(REFRESH));
        land_script(&mut app, &pr, refresh_script());
        assert_eq!(testing::count::<crate::ui::markdown::MdLink>(&mut app), 0);
        let view = refresh_view(&app, &pr);
        click_suggest(&mut app, &pr);
        let editor = testing::tab(&app, &pr).ui.editor.expect("an editor");
        assert_eq!(editor.mode, ComposerMode::Write);
        assert_eq!(editor.text, scenario_comment(&view, &refresh_script()));
        assert!(
            editor
                .text
                .starts_with("Suggested test: [x](http://evil) **b**")
        );
        let _ = Tell::Lost(String::new());
    }
}
