//! The cockpit's center column: the selected item's title, inputs or values, script, errors and
//! run buttons. Every string the views here hold is already printable and bounded; the drawers
//! below put them on screen as plain text.

use std::collections::{BTreeMap, HashMap};

use bevy::ecs::system::EntityCommands;
use bevy::input_focus::InputFocus;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::{EditableText, TextEdit, Underline};
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};
use clusia_core::PrRef;
use clusia_core::cockpit::{
    BENCH_SECS_MAX, BENCH_SECS_MIN, BENCH_WARMUP_MAX, BenchAsk, BenchInput, BenchView,
    CockpitActivity, CockpitScope, ComparisonView, Input, InputKind, ItemKind, ItemRef,
    ItemSupport, MainPhase, ROUNDS_MAX, RunSide, SCENARIO_ROUNDS_MIN, ScenarioApproval,
    ScenarioInput, ScenarioView, SideView, check_input, error_text,
};
use clusia_core::functions::check_value;
use clusia_core::printable::{printable, printable_lines};

use super::frame::{CockpitCenter, CockpitCenterFoot, CockpitErrors, FrameBuilt, failed_line};
use super::views::{CenterView, PURPOSE_SHOWN, TITLE_SHOWN, VALUE_SHOWN, center_view, shown};
use super::{CockpitModal, CockpitModel, CockpitModels, Field, FieldKey};
use crate::agent_name;
use crate::bridge::{Ask, Asks, CockpitAskKind, Model, Toasts};
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::review_state::ReviewTabs;
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::screens::review::tests_tab::TestsModels;
use crate::screens::review::tests_tab::body::cockpit_open;
use crate::screens::review::tests_tab::plan::{Refocus, Spawner};
use crate::theme::Swatch;
use crate::ui::kit::{
    Clickable, Field as Kept, FieldCommitted, Fill, HoverFill, Stroke, Type, Variant, button,
    disabled_button, divider, panel, segment, segments, text, text_field, wrapped_text,
};

/// Choices a segmented control may have, and the longest one.
const SEGMENTS_MAX: usize = 4;
const SEGMENT_CHARS_MAX: usize = 12;

/// Rows of an error list before "+ k more".
pub const ERRORS_SHOWN: usize = 5;
/// Lines of a script before "+ k more lines".
pub const SCRIPT_LINES_SHOWN: usize = 12;
/// Characters of one script line before it is cut with "…".
pub const SCRIPT_LINE_SHOWN: usize = 240;

#[derive(Debug, Clone, PartialEq)]
pub struct InputRow {
    pub name: String,
    pub label: String,
    pub value: String,
    pub edited: bool,
    /// The choices of a segmented control; `None` is a text field.
    pub choices: Option<Vec<String>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NumberRow {
    pub value: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScenarioForm {
    pub inputs: Vec<InputRow>,
    pub rounds: NumberRow,
    /// A run on main alternates the two sides.
    pub alternated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValueRow {
    pub name: String,
    pub label: String,
    pub value: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BenchForm {
    pub function: String,
    pub values: Vec<ValueRow>,
    pub warmup: NumberRow,
    pub secs: NumberRow,
    pub rounds: NumberRow,
    /// "at most 10 rounds × 30 s a side", built from the limits.
    pub limit: String,
    /// The loop file's path.
    pub loop_file: String,
    pub ask_label: String,
    /// Why **Ask … for new values** is off.
    pub ask_off: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaneBody {
    Loading,
    Error(String),
    Text { lines: String, more: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScriptPane {
    /// The command and whether it is approved.
    pub header: String,
    pub tabs: Vec<String>,
    pub selected: usize,
    pub body: PaneBody,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionView {
    Run {
        label: String,
        enabled: bool,
        reason: Option<String>,
        /// The item's hash when the view was built.
        hash: String,
    },
    ApproveAndRun,
    Stop {
        text: String,
    },
    Blocked {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorTone {
    Pr,
    Main,
    More,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErrorRow {
    pub text: String,
    pub tone: ErrorTone,
}

/// `text` as a whole number in `lo..=hi`, or "Between <lo> and <hi>".
pub fn in_range(text: &str, lo: u32, hi: u32) -> Result<u32, String> {
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|n| (lo..=hi).contains(n))
        .ok_or_else(|| format!("Between {lo} and {hi}"))
}

fn scenario_of<'a>(model: &'a CockpitModel, slug: &str) -> Option<&'a ScenarioView> {
    model
        .view
        .as_ref()?
        .scenarios
        .iter()
        .find(|s| s.slug == slug)
}

fn bench_of<'a>(model: &'a CockpitModel, slug: &str) -> Option<&'a BenchView> {
    model.view.as_ref()?.benches.iter().find(|b| b.slug == slug)
}

/// Keeps `message` under the field it is about, and says so to the model's views.
fn fail(model: &mut CockpitModel, key: FieldKey, message: String) -> Result<Option<Ask>, String> {
    model.field_errors.insert(key, message.clone());
    model.touch();
    Err(message)
}

/// Sends the whole of a scenario's overlay with one value, or the rounds, changed: the daemon
/// replaces the overlay, so the command, the rounds, the timeout and the edited script files go
/// again. The script of an edited file must be held, or nothing is sent.
fn save_scenario(
    model: &mut CockpitModel,
    pr: &PrRef,
    slug: &str,
    field: Field,
    values: impl FnOnce(&mut BTreeMap<String, String>),
    rounds: Option<u32>,
) -> Result<Option<Ask>, String> {
    let key = FieldKey {
        item: item_of(ItemKind::Scenario, slug),
        field,
    };
    let Some(s) = scenario_of(model, slug) else {
        return Ok(None);
    };
    let mut all: BTreeMap<String, String> = s
        .inputs
        .iter()
        .map(|i| (i.input.name.clone(), i.value.clone()))
        .collect();
    values(&mut all);
    let files = if s.scripts.iter().any(|f| f.edited) {
        let held = model
            .script
            .as_ref()
            .filter(|sc| sc.scenario == s.slug && sc.approval_hash == s.approval_hash);
        match held {
            Some(script) => script.files.iter().cloned().collect(),
            None => return fail(model, key, "Loading the script…".into()),
        }
    } else {
        BTreeMap::new()
    };
    let input = ScenarioInput {
        values: all,
        files,
        run: Some(s.run.clone()),
        rounds: Some(rounds.unwrap_or(s.rounds)),
        timeout_secs: Some(s.timeout_secs),
    };
    model.field_errors.remove(&key);
    let ask = Ask::SaveScenario {
        pr: pr.clone(),
        scenario: slug.to_string(),
        input,
    };
    Ok(model.send_save(key, ask))
}

/// An input of a scenario committed with `text`: checked, then saved with every other value as
/// it is. A value that fails its kind stays where it was typed, with its message under it.
pub fn input_commit(
    model: &mut CockpitModel,
    pr: &PrRef,
    scenario: &str,
    name: &str,
    text: &str,
) -> Result<Option<Ask>, String> {
    let Some(input) = scenario_of(model, scenario)
        .and_then(|s| s.inputs.iter().find(|i| i.input.name == name))
        .map(|i| i.input.clone())
    else {
        return Ok(None);
    };
    if let Err(message) = check_input(input.kind, &input.choices, text) {
        let key = FieldKey {
            item: item_of(ItemKind::Scenario, scenario),
            field: Field::Value(name.to_string()),
        };
        return fail(model, key, message);
    }
    save_scenario(
        model,
        pr,
        scenario,
        Field::Value(name.to_string()),
        |values| {
            values.insert(name.to_string(), text.to_string());
        },
        None,
    )
}

/// A scenario's rounds a side committed with `text`.
pub fn rounds_commit(
    model: &mut CockpitModel,
    pr: &PrRef,
    scenario: &str,
    text: &str,
) -> Result<Option<Ask>, String> {
    match in_range(text, SCENARIO_ROUNDS_MIN, ROUNDS_MAX) {
        Ok(n) => save_scenario(model, pr, scenario, Field::Rounds, |_| {}, Some(n)),
        Err(message) => {
            let key = FieldKey {
                item: item_of(ItemKind::Scenario, scenario),
                field: Field::Rounds,
            };
            fail(model, key, message)
        }
    }
}

/// A benchmark's value (`name` is a parameter) or loop setting (`warmup`, `secs`, `rounds`)
/// committed with `text`.
pub fn bench_commit(
    model: &mut CockpitModel,
    pr: &PrRef,
    function: &str,
    name: &str,
    text: &str,
) -> Result<Option<Ask>, String> {
    let Some(b) = bench_of(model, function) else {
        return Ok(None);
    };
    let (mut warmup, mut secs, mut rounds) = (b.warmup, b.secs, b.rounds);
    let mut values = b.values.clone();
    let field = match name {
        "warmup" => Field::Warmup,
        "secs" => Field::Secs,
        "rounds" => Field::Rounds,
        other => Field::Value(other.to_string()),
    };
    let checked: Result<(), String> = match name {
        "warmup" => in_range(text, 0, BENCH_WARMUP_MAX).map(|n| warmup = n),
        "secs" => in_range(text, BENCH_SECS_MIN, BENCH_SECS_MAX).map(|n| secs = n),
        "rounds" => in_range(text, 1, ROUNDS_MAX).map(|n| rounds = n),
        _ => match b.params.iter().find(|p| p.name == name) {
            Some(p) => check_value(p.kind, text)
                .map(|()| {
                    values.insert(name.to_string(), text.to_string());
                })
                .map_err(str::to_string),
            None => return Ok(None),
        },
    };
    let key = FieldKey {
        item: item_of(ItemKind::Bench, function),
        field,
    };
    if let Err(message) = checked {
        return fail(model, key, message);
    }
    model.field_errors.remove(&key);
    let ask = Ask::SaveBench {
        pr: pr.clone(),
        function: function.to_string(),
        input: BenchInput {
            values,
            warmup: Some(warmup),
            secs: Some(secs),
            rounds: Some(rounds),
        },
    };
    Ok(model.send_save(key, ask))
}

/// "this PR" or "main", as the error lines and the run text say it.
fn side_word(side: RunSide) -> &'static str {
    match side {
        RunSide::Pr => "this PR",
        RunSide::Main => "main",
    }
}

/// What the action row draws for `item`: Stop while it runs, the reason it cannot run, the
/// approval it needs, or the run button with its state.
pub fn shown_action(model: &CockpitModel, item: &ItemRef) -> ActionView {
    if model.running_item() == Some(item)
        && let Some(CockpitActivity::Running {
            side,
            round,
            rounds,
            ..
        }) = model.view.as_ref().and_then(|v| v.activity.as_ref())
    {
        let place = if *round == 0 {
            "warm-up".to_string()
        } else {
            format!("round {round} of {rounds}")
        };
        return ActionView::Stop {
            text: format!("{}, {place} · finished rounds are kept", side_word(*side)),
        };
    }
    let (support, approval, hash) = match item.kind {
        ItemKind::Scenario => {
            let Some(s) = scenario_of(model, &item.id) else {
                return ActionView::Blocked {
                    reason: String::new(),
                };
            };
            (&s.support, Some(s.approval), s.hash.clone())
        }
        _ => {
            let Some(b) = bench_of(model, &item.id) else {
                return ActionView::Blocked {
                    reason: String::new(),
                };
            };
            (&b.support, None, b.hash.clone())
        }
    };
    if let ItemSupport::NoCommand { reason }
    | ItemSupport::Stale { reason }
    | ItemSupport::Unsupported { reason } = support
    {
        return ActionView::Blocked {
            reason: shown(reason, PURPOSE_SHOWN),
        };
    }
    if approval.is_some_and(|a| a != ScenarioApproval::Approved) {
        return ActionView::ApproveAndRun;
    }
    let reason = model
        .run_disabled_reason(Some(item), model.on_main, model.tests_busy)
        .map(|r| shown(&r, PURPOSE_SHOWN));
    ActionView::Run {
        label: if model.on_main {
            "▶ Run on this PR and main".into()
        } else {
            "▶ Run on this PR".into()
        },
        enabled: reason.is_none(),
        reason,
        hash,
    }
}

/// The errors of the last run or of the comparison, as rows: at most `ERRORS_SHOWN`, then how
/// many more. The PR's lines say how often main hit the same error.
pub(super) fn errors_view(
    last: Option<&SideView>,
    comparison: Option<&ComparisonView>,
) -> Vec<ErrorRow> {
    let mut rows: Vec<ErrorRow> = Vec::new();
    match comparison {
        Some(c) => {
            for e in &c.pr.errors {
                let on_main: u32 = c
                    .main
                    .errors
                    .iter()
                    .filter(|m| m.text == e.text)
                    .map(|m| m.count)
                    .sum();
                rows.push(ErrorRow {
                    text: format!("{} · {on_main} on main", error_text(e, RunSide::Pr)),
                    tone: ErrorTone::Pr,
                });
            }
            for e in c
                .main
                .errors
                .iter()
                .filter(|m| !c.pr.errors.iter().any(|p| p.text == m.text))
            {
                rows.push(ErrorRow {
                    text: error_text(e, RunSide::Main),
                    tone: ErrorTone::Main,
                });
            }
        }
        None => {
            for e in last.map_or(&[][..], |s| &s.errors[..]) {
                rows.push(ErrorRow {
                    text: error_text(e, RunSide::Pr),
                    tone: ErrorTone::Pr,
                });
            }
        }
    }
    if rows.is_empty() {
        return vec![ErrorRow {
            text: "None".into(),
            tone: ErrorTone::None,
        }];
    }
    let more = rows.len().saturating_sub(ERRORS_SHOWN);
    rows.truncate(ERRORS_SHOWN);
    if more > 0 {
        rows.push(ErrorRow {
            text: format!("+ {more} more"),
            tone: ErrorTone::More,
        });
    }
    for row in &mut rows {
        row.text = shown(&row.text, PURPOSE_SHOWN);
    }
    rows
}

/// A scenario's inputs and rounds as the card draws them, with the errors under their fields.
pub(super) fn scenario_form(model: &CockpitModel, s: &ScenarioView) -> ScenarioForm {
    let item = item_of(ItemKind::Scenario, &s.slug);
    let error_of = |field: Field| {
        model
            .field_errors
            .get(&FieldKey {
                item: item.clone(),
                field,
            })
            .map(|e| shown(e, PURPOSE_SHOWN))
    };
    ScenarioForm {
        inputs: s
            .inputs
            .iter()
            .map(|i| InputRow {
                name: i.input.name.clone(),
                label: shown(&i.input.label, TITLE_SHOWN),
                value: shown(&i.value, VALUE_SHOWN),
                edited: i.edited,
                choices: segment_choices(&i.input),
                error: error_of(Field::Value(i.input.name.clone())),
            })
            .collect(),
        rounds: NumberRow {
            value: s.rounds.to_string(),
            error: error_of(Field::Rounds),
        },
        alternated: model.on_main,
    }
}

/// The choices a segmented control draws: at most four, each short and plain. A text field
/// otherwise, which takes any choice typed.
fn segment_choices(input: &Input) -> Option<Vec<String>> {
    match input.kind {
        InputKind::Boolean => Some(vec!["true".into(), "false".into()]),
        InputKind::OneOf
            if !input.choices.is_empty()
                && input.choices.len() <= SEGMENTS_MAX
                && input
                    .choices
                    .iter()
                    .all(|c| c.chars().count() <= SEGMENT_CHARS_MAX && printable(c) == *c) =>
        {
            Some(input.choices.clone())
        }
        _ => None,
    }
}

/// A benchmark's values, loop settings and loop file as the card draws them.
pub(super) fn bench_form(model: &CockpitModel, b: &BenchView, agent: &str) -> BenchForm {
    let item = item_of(ItemKind::Bench, &b.slug);
    let error_of = |field: Field| {
        model
            .field_errors
            .get(&FieldKey {
                item: item.clone(),
                field,
            })
            .map(|e| shown(e, PURPOSE_SHOWN))
    };
    let number = |value: u32, field: Field| NumberRow {
        value: value.to_string(),
        error: error_of(field),
    };
    BenchForm {
        function: b.slug.clone(),
        values: b
            .params
            .iter()
            .map(|p| ValueRow {
                name: p.name.clone(),
                label: shown(&p.label, TITLE_SHOWN),
                value: shown(
                    b.values.get(&p.name).map_or("", String::as_str),
                    VALUE_SHOWN,
                ),
                error: error_of(Field::Value(p.name.clone())),
            })
            .collect(),
        warmup: number(b.warmup, Field::Warmup),
        secs: number(b.secs, Field::Secs),
        rounds: number(b.rounds, Field::Rounds),
        limit: format!("at most {ROUNDS_MAX} rounds × {BENCH_SECS_MAX} s a side"),
        loop_file: shown(&b.path, TITLE_SHOWN),
        ask_label: format!("Ask {agent} for new values"),
        ask_off: if model.tests_busy {
            Some("Tests are running".into())
        } else if model.busy() {
            Some("The QA cockpit is running".into())
        } else {
            None
        },
    }
}

/// `line` cut to `SCRIPT_LINE_SHOWN` characters, with "…" where it was cut.
fn cut_line(line: &str) -> String {
    match line.char_indices().nth(SCRIPT_LINE_SHOWN) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

/// The script's lines as the pane shows them: at most `SCRIPT_LINES_SHOWN`, then how many more.
pub fn script_lines(text: &str) -> (String, Option<String>) {
    let plain = printable_lines(text);
    let cut: Vec<String> = plain.lines().map(cut_line).collect();
    let lines: Vec<&str> = cut.iter().map(String::as_str).collect();
    let more = lines
        .len()
        .checked_sub(SCRIPT_LINES_SHOWN)
        .filter(|k| *k > 0)
        .map(|k| format!("+ {k} more lines"));
    (
        lines
            .iter()
            .take(SCRIPT_LINES_SHOWN)
            .copied()
            .collect::<Vec<_>>()
            .join("\n"),
        more,
    )
}

/// The script card of a scenario: its command, its file tabs and the selected file's text.
pub(super) fn script_pane(model: &CockpitModel, s: &ScenarioView) -> ScriptPane {
    let approval = match s.approval {
        ScenarioApproval::Approved => "approved",
        ScenarioApproval::NotApproved => "not approved",
        ScenarioApproval::Changed => "approve again",
    };
    let header = match &s.command {
        Some(command) => format!("{} · {approval}", shown(command, PURPOSE_SHOWN)),
        None => approval.to_string(),
    };
    let tabs: Vec<String> = s
        .scripts
        .iter()
        .map(|f| shown(&f.name, TITLE_SHOWN))
        .collect();
    let selected = model.script_tab.min(tabs.len().saturating_sub(1));
    let held = model
        .script
        .as_ref()
        .filter(|sc| sc.scenario == s.slug && sc.approval_hash == s.approval_hash);
    let body = match held {
        Some(script) => {
            let text = s
                .scripts
                .get(selected)
                .and_then(|meta| script.files.iter().find(|(name, _)| *name == meta.name))
                .or_else(|| script.files.get(selected));
            match text {
                Some((_, text)) => {
                    let (lines, more) = script_lines(text);
                    PaneBody::Text { lines, more }
                }
                None => PaneBody::Loading,
            }
        }
        None => match &model.script_error {
            Some(error) => PaneBody::Error(shown(error, PURPOSE_SHOWN)),
            None => PaneBody::Loading,
        },
    };
    ScriptPane {
        header,
        tabs,
        selected,
        body,
    }
}

fn item_of(kind: ItemKind, id: &str) -> ItemRef {
    ItemRef {
        kind,
        id: id.to_string(),
    }
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ScenarioField {
    pub pr: PrRef,
    pub scenario: String,
    pub name: String,
}

/// One choice of a segmented input: a click commits it as the input's value.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ScenarioChoice {
    pub pr: PrRef,
    pub scenario: String,
    pub name: String,
    pub value: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RoundsField {
    pub pr: PrRef,
    pub item: ItemRef,
}

/// `name` is a parameter, or `warmup`, `secs` or `rounds`.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct BenchField {
    pub pr: PrRef,
    pub function: String,
    pub name: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ScriptTab {
    pub pr: PrRef,
    pub name: String,
    pub index: usize,
}

/// The run button of an item; `seen` is the item's hash the window drew it with.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RunThisItem {
    pub pr: PrRef,
    pub item: ItemRef,
    pub seen: String,
}

/// **Approve and run…**: opens the approval of the scenario.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct OpenApproval {
    pub pr: PrRef,
    pub scenario: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct StopItem(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RemoveScenarioButton {
    pub pr: PrRef,
    pub scenario: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ResetScenarioButton {
    pub pr: PrRef,
    pub scenario: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct WriteBenchValues {
    pub pr: PrRef,
    pub function: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SuggestAsComment {
    pub pr: PrRef,
    pub scenario: String,
}

/// The `CenterView` the center region was drawn from.
#[derive(Component)]
pub struct CenterBuilt(CenterView);

/// What the foot draws: the action row. The errors are drawn above it, where the card
/// scrolls, and the foot holds them only to know which item it draws for.
#[derive(Debug, Clone, PartialEq)]
pub struct FootView {
    pub item: ItemRef,
    pub errors: Vec<ErrorRow>,
    pub action: ActionView,
    /// A scenario's rounds, a field next to the run button.
    pub rounds: Option<NumberRow>,
    pub alternated: bool,
    pub on_main: bool,
}

#[derive(Component)]
pub struct FootBuilt(FootView);

/// What the foot draws for the item the center shows, if it shows one.
pub fn foot_view(model: &CockpitModel, agent: &str, now: i64) -> Option<FootView> {
    let center = center_view(model, agent, now);
    Some(FootView {
        item: center.item?,
        errors: center.errors,
        action: center.action?,
        rounds: center.form.as_ref().map(|f| f.rounds.clone()),
        alternated: model.on_main,
        on_main: model.on_main,
    })
}

const LABEL_WIDTH: f32 = 110.0;
const FIELD_WIDTH: f32 = 150.0;
const NUMBER_WIDTH: f32 = 90.0;
/// A value longer than this gets a row of its own and a wide field.
const LONG_VALUE: usize = 16;
const WHOLE_WIDTH: f32 = 340.0;
const SECTION_GAP: f32 = 14.0;

const PATH: Type = Type {
    size: 12.0,
    weight: 400,
    ink: Swatch::Faint,
    mono: true,
};

fn field_key_of(item: &ItemRef, name: &str) -> FieldKey {
    FieldKey {
        item: item.clone(),
        field: match name {
            "warmup" => Field::Warmup,
            "secs" => Field::Secs,
            "rounds" => Field::Rounds,
            other => Field::Value(other.to_string()),
        },
    }
}

/// The text a field of the region holds when a rebuild must not take it away: typed and not
/// committed, or refused. The key is the field's name.
fn kept_text(
    model: &CockpitModel,
    item: &ItemRef,
    name: &str,
    text: &EditableText,
    kept: &Kept,
) -> Option<String> {
    let value = text.value().to_string();
    (value != kept.committed || model.field_errors.contains_key(&field_key_of(item, name)))
        .then_some(value)
}

#[allow(clippy::too_many_arguments)]
pub fn fill_center(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    clock: Res<Clock>,
    models: Res<CockpitModels>,
    fonts: Res<UiFonts>,
    focus: Res<InputFocus>,
    scenario_fields: Query<(&ScenarioField, &EditableText, &Kept)>,
    bench_fields: Query<(&BenchField, &EditableText, &Kept)>,
    regions: Query<(Entity, &CockpitCenter, Option<&CenterBuilt>)>,
) {
    let now = clock.now();
    for (entity, region, built) in &regions {
        let pr = &region.0;
        let Some(tab) = tabs.0.get(pr).filter(|t| t.ready().is_some()) else {
            continue;
        };
        let Some(model) = models.0.get(pr) else {
            continue;
        };
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let view = center_view(model, &agent, now);
        let Some(item) = view.item.clone() else {
            // The frame draws the center while no item is selected; what was built here is gone.
            if built.is_some() {
                commands.entity(entity).remove::<CenterBuilt>();
            }
            continue;
        };
        if built.is_some_and(|b| b.0 == view) {
            continue;
        }
        let same_item = built.is_some_and(|b| b.0.item.as_ref() == Some(&item));
        let mut typed: HashMap<String, String> = HashMap::new();
        let mut focused: Option<String> = None;
        if same_item {
            for (f, text, kept) in &scenario_fields {
                if f.pr == *pr
                    && f.scenario == item.id
                    && let Some(v) = kept_text(model, &item, &f.name, text, kept)
                {
                    typed.insert(f.name.clone(), v);
                }
            }
            for (f, text, kept) in &bench_fields {
                if f.pr == *pr
                    && f.function == item.id
                    && let Some(v) = kept_text(model, &item, &f.name, text, kept)
                {
                    typed.insert(f.name.clone(), v);
                }
            }
            focused = focus.get().and_then(|e| {
                scenario_fields
                    .get(e)
                    .ok()
                    .filter(|(f, ..)| f.pr == *pr && f.scenario == item.id)
                    .map(|(f, ..)| f.name.clone())
                    .or_else(|| {
                        bench_fields
                            .get(e)
                            .ok()
                            .filter(|(f, ..)| f.pr == *pr && f.function == item.id)
                            .map(|(f, ..)| f.name.clone())
                    })
            });
        }
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| draw_center(p, &fonts, pr, &view, focused.as_deref(), &typed));
        commands
            .entity(entity)
            .remove::<FrameBuilt>()
            .insert(CenterBuilt(view));
    }
}

#[allow(clippy::too_many_arguments)]
pub fn fill_foot(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    clock: Res<Clock>,
    models: Res<CockpitModels>,
    fonts: Res<UiFonts>,
    focus: Res<InputFocus>,
    rounds: Query<(&RoundsField, &EditableText, &Kept)>,
    regions: Query<(Entity, &CockpitCenterFoot, Option<&FootBuilt>)>,
) {
    let now = clock.now();
    for (entity, region, built) in &regions {
        let pr = &region.0;
        let Some(tab) = tabs.0.get(pr).filter(|t| t.ready().is_some()) else {
            continue;
        };
        let Some(model) = models.0.get(pr) else {
            continue;
        };
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let Some(view) = foot_view(model, &agent, now) else {
            if built.is_some() {
                commands.entity(entity).despawn_related::<Children>();
                commands.entity(entity).remove::<FootBuilt>();
            }
            continue;
        };
        if built.is_some_and(|b| b.0 == view) {
            continue;
        }
        let same_item = built.is_some_and(|b| b.0.item == view.item);
        let mut typed = None;
        let mut focused = false;
        if same_item {
            for (f, text, kept) in &rounds {
                if f.pr == *pr && f.item == view.item {
                    typed = kept_text(model, &view.item, "rounds", text, kept);
                }
            }
            focused = focus
                .get()
                .and_then(|e| rounds.get(e).ok())
                .is_some_and(|(f, ..)| f.pr == *pr && f.item == view.item);
        }
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| draw_foot(p, &fonts, pr, &view, focused, typed.as_deref()));
        commands.entity(entity).insert(FootBuilt(view));
    }
}

fn section_head(p: &mut Spawner, fonts: &UiFonts, title: &str, note: Option<&str>) {
    p.spawn(Node {
        width: percent(100),
        justify_content: JustifyContent::SpaceBetween,
        align_items: AlignItems::Baseline,
        margin: UiRect::top(px(SECTION_GAP)),
        ..default()
    })
    .with_children(|row| {
        row.spawn(text(fonts, title.to_string(), Type::STRONG));
        if let Some(note) = note {
            row.spawn(text(fonts, note.to_string(), Type::META.ink(Swatch::Faint)));
        }
    });
}

/// A text field that keeps what was typed, takes the focus back after a redraw and shows a
/// changed value with the green border.
fn editable<B: Bundle>(
    p: &mut Spawner,
    fonts: &UiFonts,
    saved: &str,
    typed: Option<&String>,
    width: f32,
    edited: bool,
    focused: bool,
    marker: B,
) {
    let shown = typed.map_or(saved, String::as_str);
    let mut e = p.spawn((text_field(fonts, shown, width, true), marker));
    e.insert(Kept {
        committed: saved.to_string(),
    });
    if edited {
        e.insert(Stroke(Swatch::GreenLine));
    }
    if focused {
        e.insert(Refocus);
    } else {
        e.queue(|mut e: EntityWorldMut| {
            if let Some(mut t) = e.get_mut::<EditableText>() {
                t.pending_edits.clear();
                t.queue_edit(TextEdit::TextStart(false));
            }
        });
    }
}

fn error_line(p: &mut Spawner, fonts: &UiFonts, error: &Option<String>) {
    if let Some(error) = error {
        p.spawn(Node {
            padding: UiRect::left(px(LABEL_WIDTH + 8.0)),
            ..default()
        })
        .with_child(wrapped_text(
            fonts,
            error.clone(),
            Type::META.ink(Swatch::Orange),
        ));
    }
}

/// A label with its field and the error under it; two of these make a row of the grid.
fn cell(
    p: &mut Spawner,
    fonts: &UiFonts,
    label: &str,
    error: &Option<String>,
    whole: bool,
    field: impl FnOnce(&mut Spawner),
) {
    p.spawn(Node {
        width: percent(if whole { 100 } else { 48 }),
        min_width: px(LABEL_WIDTH + FIELD_WIDTH + 8.0),
        flex_grow: 1.0,
        flex_direction: FlexDirection::Column,
        row_gap: px(3),
        padding: UiRect::bottom(px(8)),
        ..default()
    })
    .with_children(|c| {
        c.spawn(Node {
            align_items: AlignItems::Center,
            column_gap: px(8),
            ..default()
        })
        .with_children(|pair| {
            pair.spawn(Node {
                width: px(LABEL_WIDTH),
                flex_shrink: 0.0,
                ..default()
            })
            .with_child(wrapped_text(fonts, label.to_string(), Type::MUTED));
            field(pair);
        });
        error_line(c, fonts, error);
    });
}

fn grid<'a>(p: &'a mut Spawner) -> EntityCommands<'a> {
    p.spawn(Node {
        width: percent(100),
        flex_wrap: FlexWrap::Wrap,
        column_gap: px(16),
        margin: UiRect::top(px(8)),
        ..default()
    })
}

fn draw_center(
    p: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    view: &CenterView,
    focused: Option<&str>,
    typed: &HashMap<String, String>,
) {
    if let Some(failed) = &view.failed {
        failed_line(p, fonts, pr, failed);
    }
    let Some(item) = &view.item else { return };
    p.spawn(Node {
        width: percent(100),
        flex_shrink: 0.0,
        flex_direction: FlexDirection::Column,
        padding: UiRect::new(px(18), px(18), px(14), px(12)),
        ..default()
    })
    .with_children(|body| {
        let scenario = view.form.is_some();
        body.spawn(Node {
            width: percent(100),
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::FlexStart,
            column_gap: px(12),
            ..default()
        })
        .with_children(|row| {
            let title = if scenario {
                Type::HEADING.size(16.0)
            } else {
                Type {
                    mono: true,
                    weight: 700,
                    ..Type::HEADING
                }
            };
            row.spawn(Node {
                flex_grow: 1.0,
                flex_basis: px(0),
                min_width: px(0),
                ..default()
            })
            .with_child(wrapped_text(fonts, view.title.clone(), title));
            if scenario {
                row.spawn(Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::FlexEnd,
                    flex_shrink: 0.0,
                    max_width: percent(50),
                    ..default()
                })
                .with_children(|right| {
                    right.spawn(wrapped_text(fonts, view.path.clone(), PATH));
                    right
                        .spawn(Node {
                            column_gap: px(4),
                            ..default()
                        })
                        .with_children(|links| {
                            if view.edited {
                                links.spawn((
                                    button(fonts, "Reset", Variant::Ghost),
                                    ResetScenarioButton {
                                        pr: pr.clone(),
                                        scenario: item.id.clone(),
                                    },
                                    observe(on_reset),
                                ));
                            }
                            links.spawn((
                                button(fonts, "Remove", Variant::Ghost),
                                RemoveScenarioButton {
                                    pr: pr.clone(),
                                    scenario: item.id.clone(),
                                },
                                observe(on_remove),
                            ));
                        });
                });
            }
        });
        if !view.purpose.is_empty() {
            body.spawn(wrapped_text(fonts, view.purpose.clone(), Type::MUTED));
        }
        if !view.steps.is_empty() {
            body.spawn(Node {
                margin: UiRect::top(px(6)),
                ..default()
            })
            .with_child(wrapped_text(fonts, view.steps.clone(), Type::MUTED));
        }
        if let Some(form) = &view.form {
            draw_inputs(body, fonts, pr, &item.id, form, focused, typed);
        }
        if let Some(pane) = &view.script {
            draw_script(body, fonts, pr, pane);
        }
        if let Some(bench) = &view.bench {
            draw_bench(body, fonts, pr, bench, focused, typed);
        }
    });
}

fn draw_inputs(
    body: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    scenario: &str,
    form: &ScenarioForm,
    focused: Option<&str>,
    typed: &HashMap<String, String>,
) {
    section_head(body, fonts, "Inputs", Some("fake values, safe to change"));
    grid(body).with_children(|g| {
        for row in &form.inputs {
            cell(g, fonts, &row.label, &row.error, false, |pair| {
                match &row.choices {
                    Some(choices) => {
                        pair.spawn(segments()).with_children(|seg| {
                            for choice in choices {
                                seg.spawn((
                                    segment(fonts, choice, *choice == row.value),
                                    ScenarioChoice {
                                        pr: pr.clone(),
                                        scenario: scenario.to_string(),
                                        name: row.name.clone(),
                                        value: choice.clone(),
                                    },
                                    observe(on_choice),
                                ));
                            }
                        });
                    }
                    None => editable(
                        pair,
                        fonts,
                        &row.value,
                        typed.get(&row.name),
                        FIELD_WIDTH,
                        row.edited,
                        focused == Some(row.name.as_str()),
                        ScenarioField {
                            pr: pr.clone(),
                            scenario: scenario.to_string(),
                            name: row.name.clone(),
                        },
                    ),
                }
            });
        }
    });
}

fn draw_script(body: &mut Spawner, fonts: &UiFonts, pr: &PrRef, pane: &ScriptPane) {
    body.spawn(Node {
        width: percent(100),
        justify_content: JustifyContent::SpaceBetween,
        align_items: AlignItems::Baseline,
        column_gap: px(12),
        margin: UiRect::top(px(SECTION_GAP)),
        ..default()
    })
    .with_children(|row| {
        row.spawn(text(fonts, "Script", Type::STRONG));
        row.spawn(Node {
            min_width: px(0),
            flex_shrink: 1.0,
            ..default()
        })
        .with_child(wrapped_text(fonts, pane.header.clone(), PATH));
    });
    body.spawn(panel(
        Node {
            width: percent(100),
            margin: UiRect::top(px(8)),
            flex_direction: FlexDirection::Column,
            border: px(1).all(),
            border_radius: BorderRadius::all(px(8)),
            overflow: Overflow::clip(),
            ..default()
        },
        Swatch::Chrome,
    ))
    .insert((BorderColor::default(), Stroke(Swatch::Line)))
    .with_children(|card| {
        card.spawn(Node {
            width: percent(100),
            ..default()
        })
        .with_children(|strip| tab_strip(strip, fonts, pr, &pane.tabs, pane.selected));
        card.spawn(divider());
        card.spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Column,
            padding: UiRect::axes(px(12), px(10)),
            row_gap: px(4),
            ..default()
        })
        .with_children(|text_| match &pane.body {
            PaneBody::Loading => {
                text_.spawn(text(fonts, "Loading the script…", Type::MUTED));
            }
            PaneBody::Error(error) => {
                text_.spawn(wrapped_text(fonts, error.clone(), Type::MUTED));
            }
            PaneBody::Text { lines, more } => {
                text_.spawn(wrapped_text(
                    fonts,
                    lines.clone(),
                    Type::MONO.ink(Swatch::Fg),
                ));
                if let Some(more) = more {
                    text_.spawn(text(fonts, more.clone(), Type::META.ink(Swatch::Faint)));
                }
            }
        });
    });
}

/// The file tabs of a script card: the selected one underlined in green.
pub(super) fn tab_strip(
    strip: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    tabs: &[String],
    selected: usize,
) {
    for (index, name) in tabs.iter().enumerate() {
        let on = index == selected;
        strip
            .spawn((
                Node {
                    padding: UiRect::axes(px(12), px(7)),
                    border: UiRect::bottom(px(2)),
                    ..default()
                },
                (WidgetButton, Clickable),
                Hovered::default(),
                TabIndex(0),
                BackgroundColor::default(),
                Fill(Swatch::Clear),
                HoverFill(Swatch::Hover),
                BorderColor::default(),
                Stroke(if on { Swatch::Green } else { Swatch::Clear }),
                ScriptTab {
                    pr: pr.clone(),
                    name: name.clone(),
                    index,
                },
                observe(on_script_tab),
            ))
            .with_child(text(
                fonts,
                name.clone(),
                Type {
                    ink: if on { Swatch::Fg } else { Swatch::Muted },
                    ..Type::MONO
                },
            ));
    }
}

fn draw_bench(
    body: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    form: &BenchForm,
    focused: Option<&str>,
    typed: &HashMap<String, String>,
) {
    let field = |pair: &mut Spawner, name: &str, value: &str, width: f32| {
        editable(
            pair,
            fonts,
            value,
            typed.get(name),
            width,
            false,
            focused == Some(name),
            BenchField {
                pr: pr.clone(),
                function: form.function.clone(),
                name: name.to_string(),
            },
        );
    };
    section_head(
        body,
        fonts,
        "Values",
        Some("passed as env, like the function's cases"),
    );
    grid(body).with_children(|g| {
        for v in &form.values {
            let long = v.value.chars().count() > LONG_VALUE;
            cell(g, fonts, &v.label, &v.error, long, |pair| {
                field(
                    pair,
                    &v.name,
                    &v.value,
                    if long { WHOLE_WIDTH } else { FIELD_WIDTH },
                );
            });
        }
    });
    section_head(body, fonts, "Loop", None);
    grid(body).with_children(|g| {
        cell(
            g,
            fonts,
            "warm-up calls",
            &form.warmup.error,
            false,
            |pair| {
                field(pair, "warmup", &form.warmup.value, NUMBER_WIDTH);
            },
        );
        cell(
            g,
            fonts,
            "seconds a round",
            &form.secs.error,
            false,
            |pair| {
                field(pair, "secs", &form.secs.value, NUMBER_WIDTH);
            },
        );
        cell(
            g,
            fonts,
            "rounds a side",
            &form.rounds.error,
            false,
            |pair| {
                field(pair, "rounds", &form.rounds.value, NUMBER_WIDTH);
                pair.spawn(wrapped_text(
                    fonts,
                    form.limit.clone(),
                    Type::META.ink(Swatch::Faint),
                ));
            },
        );
    });
    section_head(body, fonts, "Loop file", None);
    body.spawn(Node {
        width: percent(100),
        justify_content: JustifyContent::SpaceBetween,
        align_items: AlignItems::Center,
        column_gap: px(12),
        margin: UiRect::top(px(6)),
        ..default()
    })
    .with_children(|row| {
        row.spawn(Node {
            flex_shrink: 1.0,
            min_width: px(0),
            ..default()
        })
        .with_child(wrapped_text(fonts, form.loop_file.clone(), PATH));
        match &form.ask_off {
            None => {
                row.spawn((
                    text_link(fonts, &form.ask_label),
                    WriteBenchValues {
                        pr: pr.clone(),
                        function: form.function.clone(),
                    },
                    observe(on_write_bench_values),
                ));
            }
            Some(_) => {
                row.spawn(text(
                    fonts,
                    form.ask_label.clone(),
                    Type::META.ink(Swatch::Faint),
                ));
            }
        }
    });
}

/// Underlined text in the regular ink that is clicked: a link on a row of actions.
pub(super) fn text_link(fonts: &UiFonts, label: &str) -> impl Bundle {
    (
        Node {
            flex_shrink: 0.0,
            ..default()
        },
        (WidgetButton, Clickable),
        Hovered::default(),
        TabIndex(0),
        children![(
            text(fonts, label.to_string(), Type::META.ink(Swatch::Fg)),
            TextLayout::no_wrap(),
            Underline,
        )],
    )
}

fn draw_foot(
    p: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    view: &FootView,
    focused: bool,
    typed: Option<&str>,
) {
    p.spawn(Node {
        width: percent(100),
        flex_direction: FlexDirection::Column,
        row_gap: px(6),
        padding: UiRect::new(px(18), px(18), px(0), px(16)),
        ..default()
    })
    .with_children(|foot| {
        foot.spawn(Node {
            margin: UiRect::top(px(10)),
            flex_direction: FlexDirection::Column,
            row_gap: px(4),
            ..default()
        })
        .with_children(|action| draw_action(action, fonts, pr, view, focused, typed));
    });
}

/// The errors of the item the center shows, under its chart.
#[derive(Component)]
pub struct ErrorsBuilt(Vec<ErrorRow>);

/// Draws the errors of the selected item, in the part of the card that scrolls.
pub fn fill_errors(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    clock: Res<Clock>,
    models: Res<CockpitModels>,
    fonts: Res<UiFonts>,
    regions: Query<(Entity, &CockpitErrors, Option<&ErrorsBuilt>)>,
) {
    let now = clock.now();
    for (entity, region, built) in &regions {
        let pr = &region.0;
        let Some(tab) = tabs.0.get(pr).filter(|t| t.ready().is_some()) else {
            continue;
        };
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let Some(rows) = models
            .0
            .get(pr)
            .and_then(|m| foot_view(m, &agent, now))
            .map(|f| f.errors)
        else {
            if built.is_some() {
                commands.entity(entity).despawn_related::<Children>();
                commands.entity(entity).remove::<ErrorsBuilt>();
            }
            continue;
        };
        if built.is_some_and(|b| b.0 == rows) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| draw_errors(p, &fonts, &rows));
        commands.entity(entity).insert(ErrorsBuilt(rows));
    }
}

fn draw_errors(p: &mut Spawner, fonts: &UiFonts, rows: &[ErrorRow]) {
    p.spawn(Node {
        width: percent(100),
        flex_direction: FlexDirection::Column,
        row_gap: px(6),
        padding: UiRect::new(px(18), px(18), px(0), px(12)),
        ..default()
    })
    .with_children(|c| {
        c.spawn(Node {
            margin: UiRect::top(px(SECTION_GAP)),
            ..default()
        })
        .with_child(text(fonts, "Errors", Type::STRONG));
        for row in rows {
            let ink = match row.tone {
                ErrorTone::Pr => Swatch::Orange,
                ErrorTone::Main => Swatch::Muted,
                ErrorTone::More | ErrorTone::None => Swatch::Faint,
            };
            c.spawn(wrapped_text(fonts, row.text.clone(), Type::BODY.ink(ink)));
        }
    });
}

fn draw_action(
    p: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    view: &FootView,
    focused: bool,
    typed: Option<&str>,
) {
    if let ActionView::Blocked { reason } = &view.action {
        p.spawn(wrapped_text(
            fonts,
            reason.clone(),
            Type::BODY.ink(Swatch::Amber),
        ));
    }
    p.spawn(Node {
        width: percent(100),
        align_items: AlignItems::Center,
        column_gap: px(8),
        flex_wrap: FlexWrap::Wrap,
        ..default()
    })
    .with_children(|row| {
        let run_label = if view.on_main {
            "▶ Run on this PR and main"
        } else {
            "▶ Run on this PR"
        };
        match &view.action {
            ActionView::Run {
                label,
                enabled,
                hash,
                ..
            } => {
                if *enabled {
                    row.spawn((
                        button(fonts, label, Variant::Primary),
                        RunThisItem {
                            pr: pr.clone(),
                            item: view.item.clone(),
                            seen: hash.clone(),
                        },
                        observe(on_run),
                    ));
                } else {
                    row.spawn(disabled_button(fonts, label));
                }
            }
            ActionView::ApproveAndRun => {
                row.spawn((
                    button(fonts, "Approve and run…", Variant::Primary),
                    OpenApproval {
                        pr: pr.clone(),
                        scenario: view.item.id.clone(),
                    },
                    observe(on_open_approval),
                ));
            }
            ActionView::Stop { text: line } => {
                row.spawn((
                    button(fonts, "■ Stop", Variant::Secondary),
                    StopItem(pr.clone()),
                    observe(on_stop),
                ));
                row.spawn(text(fonts, line.clone(), Type::MUTED));
            }
            ActionView::Blocked { .. } => {
                row.spawn(disabled_button(fonts, run_label));
            }
        }
        if let (Some(rounds), true) = (
            &view.rounds,
            matches!(
                view.action,
                ActionView::Run { .. } | ActionView::ApproveAndRun
            ),
        ) {
            row.spawn(Node {
                column_gap: px(6),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|r| {
                editable(
                    r,
                    fonts,
                    &rounds.value,
                    typed.map(str::to_string).as_ref(),
                    42.0,
                    false,
                    focused,
                    RoundsField {
                        pr: pr.clone(),
                        item: view.item.clone(),
                    },
                );
                let note = if view.alternated {
                    "rounds a side, alternated"
                } else {
                    "rounds a side"
                };
                r.spawn(text(fonts, note, Type::MUTED));
            });
        }
        if view.item.kind == ItemKind::Scenario {
            row.spawn(Node {
                flex_grow: 1.0,
                justify_content: JustifyContent::FlexEnd,
                ..default()
            })
            .with_children(|end| {
                end.spawn((
                    text_link(fonts, "Suggest as a test in a comment"),
                    SuggestAsComment {
                        pr: pr.clone(),
                        scenario: view.item.id.clone(),
                    },
                ));
            });
        }
    });
    if let ActionView::Run {
        reason: Some(reason),
        ..
    } = &view.action
    {
        p.spawn(text(fonts, reason.clone(), Type::META.ink(Swatch::Faint)));
    }
    if let Some(rounds) = &view.rounds
        && let Some(error) = &rounds.error
    {
        p.spawn(text(fonts, error.clone(), Type::META.ink(Swatch::Orange)));
    }
}

/// Whether the review may send; a toast says why not.
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

#[allow(clippy::too_many_arguments)]
fn on_run(
    activate: On<Activate>,
    buttons: Query<&RunThisItem>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(RunThisItem { pr, item, seen }) = buttons.get(activate.entity) else {
        return;
    };
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if !may_send(&tabs, &app_model, pr, &mut toasts, &time) {
        return;
    }
    let on_main = model.on_main;
    if model
        .run_disabled_reason(Some(item), on_main, model.tests_busy)
        .is_some()
    {
        return;
    }
    if on_main
        && model
            .view
            .as_ref()
            .is_some_and(|v| v.main.phase == MainPhase::NotCreated)
    {
        // Main is made first; the reviewer clicks again once it is ready.
        if model.pending != Some(CockpitAskKind::OpenMain) {
            asks.send(Ask::OpenMain(pr.clone()));
            model.sent(CockpitAskKind::OpenMain);
        }
        return;
    }
    let scope = CockpitScope::Item { item: item.clone() };
    if let Some(ask) = model.click_run(pr, scope, on_main, seen.clone()) {
        asks.send(ask);
    }
}

fn on_open_approval(
    activate: On<Activate>,
    buttons: Query<&OpenApproval>,
    mut models: ResMut<CockpitModels>,
) {
    let Ok(OpenApproval { pr, scenario }) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(model) = models.0.get_mut(pr) {
        model.modal = Some(CockpitModal::Approve {
            scenario: scenario.clone(),
        });
        model.touch();
    }
}

fn on_stop(
    activate: On<Activate>,
    buttons: Query<&StopItem>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(StopItem(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if !may_send(&tabs, &app_model, pr, &mut toasts, &time) {
        return;
    }
    if let Some(ask) = model.click_stop(pr) {
        asks.send(ask);
    }
}

fn on_remove(
    activate: On<Activate>,
    buttons: Query<&RemoveScenarioButton>,
    mut models: ResMut<CockpitModels>,
) {
    let Ok(RemoveScenarioButton { pr, scenario }) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(model) = models.0.get_mut(pr) {
        model.modal = Some(CockpitModal::Remove {
            scenario: scenario.clone(),
        });
        model.touch();
    }
}

#[allow(clippy::too_many_arguments)]
fn on_reset(
    activate: On<Activate>,
    buttons: Query<&ResetScenarioButton>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(ResetScenarioButton { pr, scenario }) = buttons.get(activate.entity) else {
        return;
    };
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if !may_send(&tabs, &app_model, pr, &mut toasts, &time) {
        return;
    }
    if model.pending == Some(CockpitAskKind::Reset) {
        return;
    }
    asks.send(Ask::ResetScenario {
        pr: pr.clone(),
        scenario: scenario.clone(),
    });
    model.sent(CockpitAskKind::Reset);
}

fn on_script_tab(
    activate: On<Activate>,
    tabs: Query<&ScriptTab>,
    mut models: ResMut<CockpitModels>,
) {
    let Ok(ScriptTab { pr, index, .. }) = tabs.get(activate.entity) else {
        return;
    };
    if let Some(model) = models.0.get_mut(pr)
        && model.script_tab != *index
    {
        model.script_tab = *index;
        model.touch();
    }
}

#[allow(clippy::too_many_arguments)]
fn on_write_bench_values(
    activate: On<Activate>,
    buttons: Query<&WriteBenchValues>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(WriteBenchValues { pr, function }) = buttons.get(activate.entity) else {
        return;
    };
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if model.busy() || model.tests_busy {
        return;
    }
    if !may_send(&tabs, &app_model, pr, &mut toasts, &time) {
        return;
    }
    asks.send(Ask::WriteBench {
        pr: pr.clone(),
        function: function.clone(),
        ask: BenchAsk::Values,
    });
    model.sent(CockpitAskKind::Write);
}

/// Sends what a commit asked for, or leaves its message under the field.
fn send(asks: &mut Asks, result: Result<Option<Ask>, String>) {
    if let Ok(Some(ask)) = result {
        asks.send(ask);
    }
}

#[allow(clippy::too_many_arguments)]
fn on_choice(
    activate: On<Activate>,
    choices: Query<&ScenarioChoice>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(c) = choices.get(activate.entity) else {
        return;
    };
    let Some(model) = models.0.get_mut(&c.pr) else {
        return;
    };
    if !may_send(&tabs, &app_model, &c.pr, &mut toasts, &time) {
        return;
    }
    send(
        &mut asks,
        input_commit(model, &c.pr, &c.scenario, &c.name, &c.value),
    );
}

/// The raw value a field's printable text stands for, when the text was not changed.
fn raw_or(saved: Option<&str>, typed: &str) -> String {
    match saved {
        Some(saved) if shown(saved, VALUE_SHOWN) == typed => saved.to_string(),
        _ => typed.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn commit_scenario_fields(
    mut committed: MessageReader<FieldCommitted>,
    inputs: Query<&ScenarioField>,
    rounds: Query<&RoundsField>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    for message in committed.read() {
        if let Ok(f) = inputs.get(message.entity) {
            if !may_send(&tabs, &app_model, &f.pr, &mut toasts, &time) {
                continue;
            }
            let Some(model) = models.0.get_mut(&f.pr) else {
                continue;
            };
            let saved = scenario_of(model, &f.scenario)
                .and_then(|s| s.inputs.iter().find(|i| i.input.name == f.name))
                .map(|i| i.value.clone());
            let value = raw_or(saved.as_deref(), &message.value);
            send(
                &mut asks,
                input_commit(model, &f.pr, &f.scenario, &f.name, &value),
            );
        } else if let Ok(f) = rounds.get(message.entity) {
            if f.item.kind != ItemKind::Scenario {
                continue;
            }
            if !may_send(&tabs, &app_model, &f.pr, &mut toasts, &time) {
                continue;
            }
            let Some(model) = models.0.get_mut(&f.pr) else {
                continue;
            };
            send(
                &mut asks,
                rounds_commit(model, &f.pr, &f.item.id, &message.value),
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn commit_bench_fields(
    mut committed: MessageReader<FieldCommitted>,
    fields: Query<&BenchField>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<CockpitModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    for message in committed.read() {
        let Ok(f) = fields.get(message.entity) else {
            continue;
        };
        if !may_send(&tabs, &app_model, &f.pr, &mut toasts, &time) {
            continue;
        }
        let Some(model) = models.0.get_mut(&f.pr) else {
            continue;
        };
        let saved = bench_of(model, &f.function).and_then(|b| b.values.get(&f.name).cloned());
        let value = raw_or(saved.as_deref(), &message.value);
        send(
            &mut asks,
            bench_commit(model, &f.pr, &f.function, &f.name, &value),
        );
    }
}

/// Asks for the selected scenario's script when it is not the one held: reading a script is no
/// run, so it needs no click. Written around change detection and announced only when something
/// was sent.
pub fn ask_script_when_needed(
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    tests: Res<TestsModels>,
    mut models: ResMut<CockpitModels>,
    mut asks: ResMut<Asks>,
) {
    let mut sent = false;
    for (pr, model) in &mut models.bypass_change_detection().0 {
        if !model.loaded
            || !cockpit_open(tests.0.get(pr))
            || read_only_reason(&tabs, &app_model, pr).is_some()
        {
            continue;
        }
        if let Some(ask) = model.want_script(pr) {
            asks.send(ask);
            sent = true;
        }
    }
    if sent {
        models.set_changed();
    }
}

/// What the app-level tests of the center and the modals share.
#[cfg(test)]
pub(super) mod support {
    use bevy::input_focus::{FocusCause, InputFocus};
    use bevy::prelude::*;
    use clusia_core::PrRef;
    use clusia_core::cockpit::{CockpitView, ItemKind, ItemRef};

    use super::super::{CockpitModel, CockpitModels};
    use crate::bridge::{CockpitTell, Tell};
    use crate::fixture::{self, DemoCockpit, demo_cockpit};
    use crate::review_state::ReviewSection;
    use crate::testing::{self, NOW};

    pub const REFRESH: &str = "refresh-under-steady-traffic";
    pub const THIRD: &str = "token-endpoint-is-down";

    pub fn pr() -> PrRef {
        fixture::demo_pr()
    }

    pub fn scenario_item(slug: &str) -> ItemRef {
        ItemRef {
            kind: ItemKind::Scenario,
            id: slug.into(),
        }
    }

    pub fn app_with(view: CockpitView) -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        testing::open_cockpit(&mut app, &pr, view);
        testing::recorded(&mut app);
        (app, pr)
    }

    pub fn scenario_app() -> (App, PrRef) {
        app_with(demo_cockpit(NOW, DemoCockpit::Scenario))
    }

    pub fn model<'a>(app: &'a App, pr: &PrRef) -> &'a CockpitModel {
        &app.world().resource::<CockpitModels>().0[pr]
    }

    pub fn edit_model(app: &mut App, pr: &PrRef, edit: impl FnOnce(&mut CockpitModel)) {
        edit(
            app.world_mut()
                .resource_mut::<CockpitModels>()
                .0
                .get_mut(pr)
                .expect("a model"),
        );
        testing::settle(app);
    }

    /// Selects `item` and drops the asks that went out with it.
    pub fn select(app: &mut App, pr: &PrRef, item: ItemRef) {
        edit_model(app, pr, |m| m.select(item));
        testing::recorded(app);
    }

    pub fn tell(app: &mut App, tell: CockpitTell) {
        testing::tell(app, Tell::Cockpit(tell));
        testing::settle(app);
    }

    pub fn changed(app: &mut App, view: CockpitView) {
        tell(
            app,
            CockpitTell::Changed {
                pr: pr(),
                view: Box::new(view),
            },
        );
    }

    pub fn click<C: Component>(app: &mut App, want: impl Fn(&C) -> bool) {
        let e = testing::find::<C>(app, want);
        testing::activate(app, e);
        testing::settle(app);
    }

    pub fn focus(app: &mut App, entity: Entity) {
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(entity, FocusCause::Navigated);
    }

    /// Types `text` into `entity` and commits it as the field does on Enter.
    pub fn commit(app: &mut App, entity: Entity, text: &str) {
        testing::type_into(app, entity, text);
        let value = app
            .world()
            .get::<bevy::text::EditableText>(entity)
            .expect("a field")
            .value()
            .to_string();
        app.world_mut()
            .write_message(crate::ui::kit::FieldCommitted { entity, value });
        testing::settle(app);
    }

    pub fn descendants(app: &App, root: Entity) -> Vec<Entity> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(e) = stack.pop() {
            if let Some(children) = app.world().get::<Children>(e) {
                for c in children.iter() {
                    out.push(c);
                    stack.push(c);
                }
            }
        }
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use clusia_core::cockpit::{
        CockpitActivity, CockpitScope, CockpitView, ErrorLine, ItemSupport, ScenarioApproval,
    };
    use clusia_core::functions::ParamKind;
    use clusia_core::printable::is_invisible;

    use super::super::views::{PURPOSE_SHOWN, TITLE_SHOWN, VALUE_SHOWN, center_view};
    use super::super::{Field, FieldKey};
    use super::*;
    use crate::bridge::{CockpitAskKind, CockpitTell};
    use crate::fixture::{self, DemoCockpit, demo_cockpit};
    use crate::testing::NOW;

    const REFRESH: &str = "refresh-under-steady-traffic";

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn refresh() -> ItemRef {
        ItemRef {
            kind: ItemKind::Scenario,
            id: REFRESH.into(),
        }
    }

    fn loaded(view: CockpitView) -> CockpitTell {
        CockpitTell::Loaded {
            pr: pr(),
            view: Box::new(view),
        }
    }

    fn model_of(view: CockpitView) -> CockpitModel {
        let mut m = CockpitModel::default();
        m.apply(&loaded(view));
        m.select(refresh());
        m
    }

    fn scenario_model() -> CockpitModel {
        model_of(demo_cockpit(NOW, DemoCockpit::Scenario))
    }

    fn bench_item(view: &CockpitView) -> ItemRef {
        ItemRef {
            kind: ItemKind::Bench,
            id: view.benches[0].slug.clone(),
        }
    }

    fn key(name: &str) -> FieldKey {
        FieldKey {
            item: refresh(),
            field: Field::Value(name.into()),
        }
    }

    #[test]
    fn input_commit_rejects_a_bad_value_and_sends_nothing() {
        let mut m = scenario_model();
        let got = input_commit(&mut m, &pr(), REFRESH, "callers", "abc");
        assert_eq!(
            got,
            Err(clusia_core::cockpit::check_input(InputKind::Integer, &[], "abc").unwrap_err())
        );
        assert_eq!(m.field_errors[&key("callers")], "Enter a whole number");
        assert!(m.saving.is_none() && m.queued_save.is_none());
        assert!(m.awaiting.is_empty());
    }

    #[test]
    fn input_commit_sends_every_value_with_this_one_replaced() {
        let mut m = scenario_model();
        let view = m.view.clone().expect("a view");
        let s = view
            .scenarios
            .iter()
            .find(|s| s.slug == REFRESH)
            .expect("s");
        let first = s.inputs[0].input.name.clone();
        let second = s.inputs[1].input.name.clone();
        let ask = input_commit(&mut m, &pr(), REFRESH, &second, "7 s")
            .expect("valid")
            .expect("sent now");
        let Ask::SaveScenario {
            scenario, input, ..
        } = ask
        else {
            panic!("a save");
        };
        assert_eq!(scenario, REFRESH);
        let mut want: BTreeMap<String, String> = s
            .inputs
            .iter()
            .map(|i| (i.input.name.clone(), i.value.clone()))
            .collect();
        want.insert(second.clone(), "7 s".into());
        assert_eq!(input.values, want);
        assert_eq!(input.values[&first], s.inputs[0].value);
        assert!(input.files.is_empty());
        assert_eq!(input.run, Some(s.run.clone()));
        assert_eq!(input.rounds, Some(s.rounds));
        assert_eq!(input.timeout_secs, Some(s.timeout_secs));
    }

    #[test]
    fn a_second_commit_replaces_the_queued_save() {
        let mut m = scenario_model();
        let a = input_commit(&mut m, &pr(), REFRESH, "callers", "1").expect("a");
        assert!(a.is_some());
        assert_eq!(
            input_commit(&mut m, &pr(), REFRESH, "duration", "30 s"),
            Ok(None)
        );
        let b = input_commit(&mut m, &pr(), REFRESH, "token_lifetime", "4 s");
        assert_eq!(b, Ok(None));
        let queued = m.queued_save.as_ref().expect("queued").ask.clone();
        m.apply(&loaded(demo_cockpit(NOW, DemoCockpit::Scenario)));
        assert_eq!(
            m.release_queued(&pr()),
            super::super::Released::Send(queued.clone())
        );
        let Ask::SaveScenario { input, .. } = queued else {
            panic!("a save");
        };
        assert_eq!(input.values["token_lifetime"], "4 s");
        assert_eq!(m.release_queued(&pr()), super::super::Released::Nothing);
    }

    #[test]
    fn rounds_field_accepts_one_to_ten_only() {
        for bad in ["0", "11", "x", "", "-1", "2.5"] {
            assert_eq!(
                in_range(bad, 1, 10),
                Err("Between 1 and 10".to_string()),
                "{bad}"
            );
        }
        assert_eq!(in_range("3", 1, 10), Ok(3));
        assert_eq!(in_range(" 10 ", 1, 10), Ok(10));
        let mut m = scenario_model();
        assert_eq!(
            rounds_commit(&mut m, &pr(), REFRESH, "11"),
            Err("Between 1 and 10".to_string())
        );
        let Ok(Some(Ask::SaveScenario { input, .. })) = rounds_commit(&mut m, &pr(), REFRESH, "4")
        else {
            panic!("a save");
        };
        assert_eq!(input.rounds, Some(4));
    }

    #[test]
    fn bench_commit_checks_every_range() {
        let view = demo_cockpit(NOW, DemoCockpit::Scenario);
        let slug = view.benches[0].slug.clone();
        let mut m = model_of(view);
        let go = |m: &mut CockpitModel, name: &str, text: &str| {
            bench_commit(m, &pr(), &slug, name, text)
        };
        assert_eq!(
            go(&mut m, "warmup", "1001"),
            Err("Between 0 and 1000".into())
        );
        assert_eq!(go(&mut m, "secs", "0"), Err("Between 1 and 30".into()));
        assert_eq!(go(&mut m, "secs", "31"), Err("Between 1 and 30".into()));
        assert_eq!(go(&mut m, "rounds", "11"), Err("Between 1 and 10".into()));
        assert_eq!(
            go(&mut m, "attempt", "x"),
            Err(clusia_core::functions::check_value(ParamKind::Int, "x")
                .unwrap_err()
                .to_string())
        );
        assert!(m.saving.is_none());
        let Ok(Some(Ask::SaveBench {
            function, input, ..
        })) = go(&mut m, "attempt", "9")
        else {
            panic!("a save");
        };
        assert_eq!(function, slug);
        assert_eq!(input.values["attempt"], "9");
        assert_eq!(input.values["base"], "Duration::from_millis(200)");
        assert_eq!(input.warmup, Some(1000));
        assert_eq!(input.secs, Some(6));
        assert_eq!(input.rounds, Some(5));
    }

    fn running_on(item: &ItemRef, side: RunSide, round: u32) -> CockpitView {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.activity = Some(CockpitActivity::Running {
            run: "r".into(),
            item: item.clone(),
            side,
            round,
            rounds: 3,
            done_items: 0,
            total_items: 1,
        });
        view
    }

    #[test]
    fn shown_action_precedence() {
        let m = model_of(running_on(&refresh(), RunSide::Pr, 2));
        assert_eq!(
            shown_action(&m, &refresh()),
            ActionView::Stop {
                text: "this PR, round 2 of 3 · finished rounds are kept".into()
            }
        );
        let m = model_of(running_on(&refresh(), RunSide::Main, 0));
        assert_eq!(
            shown_action(&m, &refresh()),
            ActionView::Stop {
                text: "main, warm-up · finished rounds are kept".into()
            }
        );
        let blocked = model_of(demo_with(|v| {
            v.scenarios[1].support = ItemSupport::NoCommand {
                reason: "No command\u{202e}".into(),
            };
        }));
        assert_eq!(
            shown_action(&blocked, &refresh()),
            ActionView::Blocked {
                reason: "No command\\u{202e}".into()
            }
        );
        for approval in [ScenarioApproval::NotApproved, ScenarioApproval::Changed] {
            let m = model_of(demo_with(|v| v.scenarios[1].approval = approval));
            assert_eq!(shown_action(&m, &refresh()), ActionView::ApproveAndRun);
        }
        let other = model_of(running_on(
            &bench_item(&demo_cockpit(NOW, DemoCockpit::Scenario)),
            RunSide::Pr,
            1,
        ));
        let ActionView::Run {
            enabled, reason, ..
        } = shown_action(&other, &refresh())
        else {
            panic!("a run button");
        };
        assert!(!enabled && reason.is_some());
        let mut idle = scenario_model();
        let ActionView::Run {
            label,
            enabled,
            reason,
            hash,
        } = shown_action(&idle, &refresh())
        else {
            panic!("a run button");
        };
        assert_eq!(
            (label.as_str(), enabled, reason),
            ("▶ Run on this PR", true, None)
        );
        assert!(!hash.is_empty());
        idle.on_main = true;
        let ActionView::Run { label, .. } = shown_action(&idle, &refresh()) else {
            panic!("a run button");
        };
        assert_eq!(label, "▶ Run on this PR and main");
    }

    fn demo_with(edit: impl FnOnce(&mut CockpitView)) -> CockpitView {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        edit(&mut view);
        view
    }

    #[test]
    fn a_run_carries_the_hash_the_window_drew() {
        let mut m = scenario_model();
        let ActionView::Run { hash: drew, .. } = shown_action(&m, &refresh()) else {
            panic!("a run button");
        };
        let mut after = demo_cockpit(NOW, DemoCockpit::Scenario);
        after.scenarios[1].hash = "h2".into();
        m.view = Some(after);
        let scope = CockpitScope::Item { item: refresh() };
        let Some(Ask::RunCockpit { seen, .. }) = m.click_run(&pr(), scope, false, drew.clone())
        else {
            panic!("a run");
        };
        assert_eq!(seen, drew, "the hash the window drew, not the live one");
        // With a save in flight the run waits and goes with the post-save hash.
        let mut m = scenario_model();
        let _ = input_commit(&mut m, &pr(), REFRESH, "callers", "9");
        let scope = CockpitScope::Item { item: refresh() };
        assert_eq!(m.click_run(&pr(), scope.clone(), false, drew), None);
        let mut saved = demo_cockpit(NOW, DemoCockpit::Scenario);
        saved.scenarios[1].hash = "after-save".into();
        m.apply(&loaded(saved));
        assert_eq!(
            m.release_queued(&pr()),
            super::super::Released::Send(Ask::RunCockpit {
                pr: pr(),
                scope,
                on_main: false,
                seen: "after-save".into()
            })
        );
    }

    #[test]
    fn a_second_click_on_run_while_one_is_in_flight_sends_nothing() {
        let mut m = scenario_model();
        let scope = CockpitScope::Item { item: refresh() };
        assert!(
            m.click_run(&pr(), scope.clone(), false, "h".into())
                .is_some()
        );
        assert_eq!(m.pending, Some(CockpitAskKind::Run));
        assert_eq!(m.click_run(&pr(), scope, false, "h".into()), None);
        let ActionView::Run {
            enabled, reason, ..
        } = shown_action(&m, &refresh())
        else {
            panic!("a run button");
        };
        assert!(!enabled, "the button is off while the run is in flight");
        assert_eq!(reason.as_deref(), Some("The QA cockpit is running"));
    }

    fn line(text: &str, count: u32) -> ErrorLine {
        ErrorLine {
            text: text.into(),
            count,
            first_ms: 1800,
        }
    }

    fn side(errors: Vec<ErrorLine>) -> SideView {
        let mut s = demo_cockpit(NOW, DemoCockpit::Scenario).scenarios[1]
            .comparison
            .clone()
            .expect("a comparison")
            .pr;
        s.errors = errors;
        s
    }

    fn comparison_with(pr: Vec<ErrorLine>, main: Vec<ErrorLine>) -> ComparisonView {
        let mut c = demo_cockpit(NOW, DemoCockpit::Scenario).scenarios[1]
            .comparison
            .clone()
            .expect("a comparison");
        c.pr.errors = pr;
        c.main.errors = main;
        c
    }

    #[test]
    fn errors_view_counts_main_and_caps_at_five() {
        let pr_lines: Vec<ErrorLine> = (0..7).map(|i| line(&format!("e{i}"), 2)).collect();
        let main = vec![line("e1", 1), line("only on main", 4)];
        let rows = errors_view(None, Some(&comparison_with(pr_lines, main)));
        assert_eq!(rows.len(), 6, "five rows and the more line");
        assert!(rows[..5].iter().all(|r| r.tone == ErrorTone::Pr));
        assert_eq!(rows[0].text, "2 × e0 after 1.8 s (this PR) · 0 on main");
        assert_eq!(rows[1].text, "2 × e1 after 1.8 s (this PR) · 1 on main");
        assert_eq!(
            rows[5],
            ErrorRow {
                text: "+ 3 more".into(),
                tone: ErrorTone::More
            }
        );
        let rows = errors_view(
            None,
            Some(&comparison_with(
                vec![line("a", 1)],
                vec![line("only on main", 4)],
            )),
        );
        assert_eq!(rows[0].tone, ErrorTone::Pr);
        assert_eq!(rows[1].tone, ErrorTone::Main);
        assert_eq!(rows[1].text, "4 × only on main after 1.8 s (main)");
        let none = errors_view(None, Some(&comparison_with(Vec::new(), Vec::new())));
        assert_eq!(
            none,
            vec![ErrorRow {
                text: "None".into(),
                tone: ErrorTone::None
            }]
        );
        let last = errors_view(Some(&side(vec![line("x", 3)])), None);
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].text, "3 × x after 1.8 s (this PR)");
        assert_eq!(errors_view(None, None)[0].tone, ErrorTone::None);
    }

    const HOSTILE: &str = "[x](http://evil)\u{202e}**";

    fn hostile() -> String {
        format!("{HOSTILE}{}", "a".repeat(10_000))
    }

    fn all_text(view: &super::super::views::CenterView) -> Vec<String> {
        let mut out = vec![
            view.title.clone(),
            view.path.clone(),
            view.purpose.clone(),
            view.steps.clone(),
        ];
        if let Some(form) = &view.form {
            for i in &form.inputs {
                out.push(i.label.clone());
                out.push(i.value.clone());
                out.extend(i.choices.iter().flatten().cloned());
            }
        }
        if let Some(bench) = &view.bench {
            for v in &bench.values {
                out.push(v.label.clone());
                out.push(v.value.clone());
            }
        }
        out.extend(view.errors.iter().map(|e| e.text.clone()));
        if let Some(ActionView::Blocked { reason }) = &view.action {
            out.push(reason.clone());
        }
        if let Some(pane) = &view.script {
            out.push(pane.header.clone());
            out.extend(pane.tabs.clone());
        }
        out
    }

    #[test]
    fn untrusted_text_is_printable_and_bounded() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        {
            let s = &mut view.scenarios[1];
            s.title = hostile();
            s.purpose = hostile();
            s.steps = vec![hostile()];
            s.inputs[0].input.label = hostile();
            s.inputs[0].value = hostile();
            s.command = Some(hostile());
            s.scripts[0].name = hostile();
            let c = s.comparison.as_mut().expect("a comparison");
            c.pr.errors = vec![line(&hostile(), 1)];
        }
        let mut m = model_of(view);
        let hash = m
            .selected_scenario()
            .expect("selected")
            .approval_hash
            .clone();
        let hostile_line = format!("{HOSTILE}{}", "b".repeat(10_000));
        m.script = Some(clusia_core::cockpit::ScenarioScript {
            scenario: REFRESH.into(),
            files: vec![(
                "x.sh".into(),
                format!("{hostile_line}\nsecond\tline\n{hostile_line}"),
            )],
            approval_hash: hash,
        });
        let center = center_view(&m, "Claude Code", NOW);
        let Some(PaneBody::Text { lines, .. }) = center.script.as_ref().map(|p| p.body.clone())
        else {
            panic!("the held script is drawn");
        };
        assert!(lines.contains("[x](http://evil)"));
        assert!(!lines.chars().any(is_invisible));
        assert!(lines.contains("second\tline"), "tabs stay tabs");
        for l in lines.lines() {
            assert!(
                l.chars().count() <= SCRIPT_LINE_SHOWN + 1,
                "{} chars",
                l.chars().count()
            );
        }
        let texts = all_text(&center);
        assert!(texts.iter().any(|t| t.contains("[x](http://evil)")));
        let cap = PURPOSE_SHOWN.max(TITLE_SHOWN).max(VALUE_SHOWN) + 80;
        for t in &texts {
            assert!(!t.chars().any(is_invisible), "{t:?}");
            assert!(t.chars().count() <= cap + 1, "{} chars", t.chars().count());
        }
        assert!(center.title.chars().count() <= TITLE_SHOWN + 1);
        assert!(
            center.form.as_ref().expect("a form").inputs[0]
                .value
                .chars()
                .count()
                <= VALUE_SHOWN + 1
        );
    }
}

#[cfg(test)]
mod app_tests {
    use clusia_core::cockpit::{
        BenchAsk, COCKPIT_CHANGED, CockpitView, ErrorLine, ScriptMeta, Verdict,
    };

    use super::super::frame::{CockpitCenter, CockpitCenterFoot};
    use super::support::*;
    use super::*;
    use crate::bridge::{CockpitAskKind, CockpitTell};
    use crate::fixture::{self, DemoCockpit, demo_cockpit};
    use crate::testing::{self, NOW};
    use crate::ui::spinner::Spinner;

    fn bench_item_of(view: &CockpitView) -> ItemRef {
        ItemRef {
            kind: ItemKind::Bench,
            id: view.benches[0].slug.clone(),
        }
    }

    fn script_tell(slug: &str) -> CockpitTell {
        CockpitTell::Script {
            pr: pr(),
            script: Box::new(fixture::demo_cockpit_script(slug).expect("a demo script")),
        }
    }

    fn field_of(app: &mut App, name: &str) -> Entity {
        testing::find::<ScenarioField>(app, |f| f.name == name)
    }

    #[test]
    fn scenario_card_draws_the_mockup_content() {
        let (mut app, pr) = scenario_app();
        select(&mut app, &pr, scenario_item(THIRD));
        tell(&mut app, script_tell(THIRD));
        for needle in [
            "Token endpoint is down",
            ".clusia/e2e/token-endpoint-is-down",
            "The endpoint answers 503; the refresh must give up and keep the old token.",
            "1 Start a fake endpoint that answers 503 · 2 Refresh with a short timeout",
            "retries",
            "timeout",
            "sh down.sh \"$CLUSIA_ARG_RETRIES\" \"$CLUSIA_ARG_TIMEOUT\" · not approved",
            "down.sh",
            "fake_oauth.py",
            "python3 fake_oauth.py --status 503 &",
            "Approve and run…",
            "Suggest as a test in a comment",
            "Errors",
            "None",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        let f = field_of(&mut app, "retries");
        assert_eq!(
            app.world()
                .get::<bevy::text::EditableText>(f)
                .expect("a field")
                .value()
                .to_string(),
            "5"
        );
        // The script shows at most twelve lines, then how many more.
        let text = fixture::demo_cockpit_script(THIRD).expect("script").files[0]
            .1
            .clone();
        let (lines, more) = script_lines(&text);
        assert!(lines.lines().count() <= SCRIPT_LINES_SHOWN);
        assert_eq!(more, None);
        let long: String = (0..30).map(|i| format!("line {i}\n")).collect();
        let (lines, more) = script_lines(&long);
        assert_eq!(lines.lines().count(), 12);
        assert_eq!(more.as_deref(), Some("+ 18 more lines"));
    }

    #[test]
    fn an_edited_input_gets_the_green_border_and_a_reset_link() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.scenarios[1].inputs[0].edited = true;
        view.scenarios[1].edited = true;
        let (mut app, pr) = app_with(view);
        select(&mut app, &pr, scenario_item(REFRESH));
        let green = |app: &mut App| {
            let mut q = app
                .world_mut()
                .query::<(&ScenarioField, &crate::ui::kit::Stroke)>();
            q.iter(app.world())
                .filter(|(_, s)| s.0 == Swatch::GreenLine)
                .map(|(f, _)| f.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(green(&mut app), ["callers"]);
        assert_eq!(testing::count::<ResetScenarioButton>(&mut app), 1);
        let (mut app, pr) = scenario_app();
        select(&mut app, &pr, scenario_item(REFRESH));
        assert!(green(&mut app).is_empty());
        assert_eq!(testing::count::<ResetScenarioButton>(&mut app), 0);
    }

    #[test]
    fn reset_and_script_tabs_work() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.scenarios[1].edited = true;
        let (mut app, pr) = app_with(view);
        select(&mut app, &pr, scenario_item(REFRESH));
        tell(&mut app, script_tell(REFRESH));
        click::<ScriptTab>(&mut app, |t| t.index == 1);
        assert_eq!(model(&app, &pr).script_tab, 1);
        assert!(testing::shows(&mut app, "HTTPServer"));
        testing::recorded(&mut app);
        click::<ResetScenarioButton>(&mut app, |b| b.pr == pr);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::ResetScenario {
                pr: pr.clone(),
                scenario: REFRESH.into()
            }]
        );
        assert_eq!(model(&app, &pr).pending, Some(CockpitAskKind::Reset));
    }

    #[test]
    fn a_field_keeps_focus_and_typed_text_across_a_rebuild() {
        let (mut app, pr) = scenario_app();
        select(&mut app, &pr, scenario_item(REFRESH));
        let f = field_of(&mut app, "callers");
        focus(&mut app, f);
        testing::type_into(&mut app, f, "5");
        let typed = app
            .world()
            .get::<bevy::text::EditableText>(f)
            .expect("a field")
            .value()
            .to_string();
        assert!(typed.contains('5'));
        // Another input's saved value changes, so the center really rebuilds.
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.scenarios[1].inputs[2].value = "250 ms".into();
        changed(&mut app, view);
        let latency = field_of(&mut app, "endpoint_latency");
        assert_eq!(
            app.world()
                .get::<bevy::text::EditableText>(latency)
                .expect("a field")
                .value()
                .to_string(),
            "250 ms"
        );
        let f = field_of(&mut app, "callers");
        assert_eq!(
            app.world()
                .get::<bevy::text::EditableText>(f)
                .expect("a field")
                .value()
                .to_string(),
            typed
        );
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(f));
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn a_committed_bad_value_shows_the_error_and_keeps_the_text() {
        let (mut app, pr) = scenario_app();
        select(&mut app, &pr, scenario_item(REFRESH));
        let f = field_of(&mut app, "callers");
        testing::type_into(&mut app, f, "abc");
        let typed = app
            .world()
            .get::<bevy::text::EditableText>(f)
            .expect("a field")
            .value()
            .to_string();
        app.world_mut().write_message(FieldCommitted {
            entity: f,
            value: typed.clone(),
        });
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Enter a whole number"));
        let f = field_of(&mut app, "callers");
        assert_eq!(
            app.world()
                .get::<bevy::text::EditableText>(f)
                .expect("a field")
                .value()
                .to_string(),
            typed
        );
        assert!(
            !testing::recorded(&mut app)
                .iter()
                .any(|a| matches!(a, Ask::SaveScenario { .. }))
        );
    }

    #[test]
    fn a_valid_commit_saves_the_whole_overlay_and_a_choice_commits() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.scenarios[1].inputs[0].input.kind = InputKind::OneOf;
        view.scenarios[1].inputs[0].input.choices = vec!["10".into(), "40".into(), "80".into()];
        view.scenarios[1].inputs[3].input.kind = InputKind::String;
        let (mut app, pr) = app_with(view);
        select(&mut app, &pr, scenario_item(REFRESH));
        let f = field_of(&mut app, "duration");
        commit(&mut app, f, "x");
        let sent = testing::recorded(&mut app);
        let [Ask::SaveScenario { input, .. }] = sent.as_slice() else {
            panic!("one save: {sent:?}");
        };
        assert!(input.run.is_some() && input.rounds.is_some());
        // The first input has three short choices: a segmented control, no text field.
        assert_eq!(
            testing::count::<ScenarioChoice>(&mut app),
            3,
            "one segment per choice"
        );
    }

    #[test]
    fn rounds_field_commits_in_the_foot() {
        let (mut app, pr) = scenario_app();
        select(&mut app, &pr, scenario_item(REFRESH));
        let e = testing::find::<RoundsField>(&mut app, |f| f.pr == pr);
        commit(&mut app, e, "x");
        assert!(testing::shows(&mut app, "Between 1 and 10"));
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn opening_the_scenario_asks_for_its_script_once_without_a_run() {
        let (mut app, pr) = scenario_app();
        edit_model(&mut app, &pr, |m| m.select(scenario_item(THIRD)));
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::GetScenarioScript {
                pr: pr.clone(),
                scenario: THIRD.into()
            }]
        );
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty());
        assert!(testing::shows(&mut app, "Loading the script…"));
    }

    #[test]
    fn a_save_keeps_cli_script_edits() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.scenarios[1].scripts[0] = ScriptMeta {
            edited: true,
            ..view.scenarios[1].scripts[0].clone()
        };
        let (mut app, pr) = app_with(view.clone());
        select(&mut app, &pr, scenario_item(REFRESH));
        let f = field_of(&mut app, "callers");
        commit(&mut app, f, "0");
        assert!(
            testing::recorded(&mut app).is_empty(),
            "no script held, nothing sent"
        );
        assert_eq!(
            model(&app, &pr)
                .field_errors
                .values()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["Loading the script…"]
        );
        tell(&mut app, script_tell(REFRESH));
        let f = field_of(&mut app, "callers");
        commit(&mut app, f, "1");
        let sent = testing::recorded(&mut app);
        let [Ask::SaveScenario { input, .. }] = sent.as_slice() else {
            panic!("one save: {sent:?}");
        };
        let script = fixture::demo_cockpit_script(REFRESH).expect("script");
        assert_eq!(
            input.files,
            script.files.into_iter().collect::<BTreeMap<_, _>>()
        );
        assert_eq!(input.run, Some(view.scenarios[1].run.clone()));
    }

    #[test]
    fn the_run_button_sends_the_hash_it_drew_once() {
        let (mut app, pr) = scenario_app();
        select(&mut app, &pr, scenario_item(REFRESH));
        let hash = model(&app, &pr)
            .selected_scenario()
            .expect("selected")
            .hash
            .clone();
        let e = testing::find::<RunThisItem>(&mut app, |b| b.pr == pr);
        app.world_mut()
            .trigger(bevy::ui_widgets::Activate { entity: e });
        app.world_mut()
            .trigger(bevy::ui_widgets::Activate { entity: e });
        app.update();
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RunCockpit {
                pr: pr.clone(),
                scope: CockpitScope::Item {
                    item: scenario_item(REFRESH)
                },
                on_main: false,
                seen: hash
            }]
        );
        assert_eq!(testing::count::<RunThisItem>(&mut app), 0, "drawn off");
    }

    #[test]
    fn run_on_main_with_main_not_created_opens_main_and_does_not_run() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.main.phase = clusia_core::cockpit::MainPhase::NotCreated;
        let (mut app, pr) = app_with(view);
        select(&mut app, &pr, scenario_item(REFRESH));
        edit_model(&mut app, &pr, |m| m.on_main = true);
        click::<RunThisItem>(&mut app, |b| b.pr == pr);
        assert_eq!(testing::recorded(&mut app), [Ask::OpenMain(pr.clone())]);
    }

    #[test]
    fn stop_sends_stop_tests_and_holds_pending() {
        let running = demo_cockpit(NOW, DemoCockpit::BenchRunning);
        let (mut app, pr) = app_with(running.clone());
        select(&mut app, &pr, bench_item_of(&running));
        assert!(testing::shows(
            &mut app,
            "main, round 3 of 5 · finished rounds are kept"
        ));
        let e = testing::find::<StopItem>(&mut app, |b| b.0 == pr);
        app.world_mut()
            .trigger(bevy::ui_widgets::Activate { entity: e });
        app.world_mut()
            .trigger(bevy::ui_widgets::Activate { entity: e });
        app.update();
        testing::settle(&mut app);
        assert_eq!(testing::recorded(&mut app), [Ask::StopTests(pr.clone())]);
        assert_eq!(model(&app, &pr).pending, Some(CockpitAskKind::Run));
        click::<StopItem>(&mut app, |b| b.0 == pr);
        assert!(testing::recorded(&mut app).is_empty(), "a second click");
        changed(&mut app, demo_cockpit(NOW, DemoCockpit::Scenario));
        assert_eq!(model(&app, &pr).pending, None);
    }

    #[test]
    fn a_changed_refusal_reasks_and_never_resends() {
        let (mut app, pr) = scenario_app();
        select(&mut app, &pr, scenario_item(REFRESH));
        click::<RunThisItem>(&mut app, |b| b.pr == pr);
        assert!(matches!(
            testing::recorded(&mut app).as_slice(),
            [Ask::RunCockpit { .. }]
        ));
        tell(
            &mut app,
            CockpitTell::Refused {
                pr: pr.clone(),
                kind: CockpitAskKind::Run,
                message: COCKPIT_CHANGED.into(),
            },
        );
        testing::settle(&mut app);
        assert_eq!(testing::recorded(&mut app), [Ask::GetCockpit(pr.clone())]);
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn ask_for_new_values_sends_write_bench_values() {
        let (mut app, pr) = scenario_app();
        let view = demo_cockpit(NOW, DemoCockpit::Scenario);
        let bench = bench_item_of(&view);
        select(&mut app, &pr, bench.clone());
        testing::recorded(&mut app);
        click::<WriteBenchValues>(&mut app, |b| b.pr == pr);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::WriteBench {
                pr: pr.clone(),
                function: bench.id.clone(),
                ask: BenchAsk::Values
            }]
        );
        assert_eq!(model(&app, &pr).pending, Some(CockpitAskKind::Write));
        assert_eq!(testing::count::<WriteBenchValues>(&mut app), 0);
        assert!(testing::shows(&mut app, "The QA cockpit is running"));
        let running = demo_cockpit(NOW, DemoCockpit::BenchRunning);
        let (mut app, pr) = app_with(running.clone());
        select(&mut app, &pr, bench_item_of(&running));
        assert_eq!(testing::count::<WriteBenchValues>(&mut app), 0);
    }

    #[test]
    fn the_benchmark_card_has_values_loop_and_no_script_pane() {
        let running = demo_cockpit(NOW, DemoCockpit::BenchRunning);
        let (mut app, pr) = app_with(running.clone());
        let bench = bench_item_of(&running);
        select(&mut app, &pr, bench.clone());
        let mut q = app.world_mut().query::<&BenchField>();
        let mut names: Vec<String> = q.iter(app.world()).map(|f| f.name.clone()).collect();
        names.sort();
        assert_eq!(names, ["attempt", "base", "rounds", "secs", "warmup"]);
        assert_eq!(testing::count::<ScriptTab>(&mut app), 0);
        assert_eq!(testing::count::<ScenarioField>(&mut app), 0);
        for needle in [
            "pub fn backoff_delay",
            "fn backoff_delay(attempt: int, base: text)",
            "warm-up calls",
            "seconds a round",
            "rounds a side",
            "at most 10 rounds × 30 s a side",
            ".clusia/functions/",
            "Ask Claude Code for new values",
            "main, round 3 of 5 · finished rounds are kept",
            "Errors",
        ] {
            if needle == "pub fn backoff_delay" {
                continue;
            }
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert_eq!(testing::count::<StopItem>(&mut app), 1);
        assert_eq!(testing::count::<RunThisItem>(&mut app), 0);
        // A committed loop value saves the whole loop.
        let (mut app, pr) = scenario_app();
        select(&mut app, &pr, bench.clone());
        testing::recorded(&mut app);
        let f = testing::find::<BenchField>(&mut app, |f| f.name == "secs");
        commit(&mut app, f, "x");
        assert!(testing::shows(&mut app, "Between 1 and 30"));
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn a_verdict_state_never_leaks_into_the_center() {
        let base = {
            let (_app, _pr) = (0, 0);
            demo_cockpit(NOW, DemoCockpit::Scenario)
        };
        let idle = {
            let mut m = CockpitModel::default();
            m.apply(&CockpitTell::Loaded {
                pr: pr(),
                view: Box::new(base.clone()),
            });
            m.select(scenario_item(REFRESH));
            m
        };
        let want = shown_action(&idle, &scenario_item(REFRESH));
        for verdict in [Verdict::Running, Verdict::Stopped, Verdict::NotEnoughRounds] {
            let mut view = base.clone();
            let c = view.scenarios[1].comparison.as_mut().expect("comparison");
            c.verdict = verdict;
            c.pr.errors.clear();
            c.main.errors = vec![ErrorLine {
                text: "boom".into(),
                count: 2,
                first_ms: 100,
            }];
            let mut m = CockpitModel::default();
            m.apply(&CockpitTell::Loaded {
                pr: pr(),
                view: Box::new(view),
            });
            m.select(scenario_item(REFRESH));
            let center = center_view(&m, "Claude Code", NOW);
            assert!(
                center.errors.iter().all(|e| e.tone != ErrorTone::Pr),
                "main's lines never read as the PR's"
            );
            assert_eq!(center.action, Some(want.clone()));
        }
    }

    #[test]
    fn the_cockpit_wakes_nothing_while_idle() {
        let (mut app, pr) = scenario_app();
        select(&mut app, &pr, scenario_item(REFRESH));
        let center = testing::find::<CockpitCenter>(&mut app, |_| true);
        let foot = testing::find::<CockpitCenterFoot>(&mut app, |_| true);
        let (c, f) = (descendants(&app, center), descendants(&app, foot));
        assert!(!c.is_empty() && !f.is_empty());
        for _ in 0..5 {
            testing::settle(&mut app);
        }
        assert_eq!(descendants(&app, center), c);
        assert_eq!(descendants(&app, foot), f);
        for e in c.iter().chain(&f) {
            assert!(app.world().get::<Spinner>(*e).is_none());
        }
    }

    #[test]
    fn an_item_that_goes_away_and_comes_back_is_drawn_again() {
        let (mut app, pr) = scenario_app();
        select(&mut app, &pr, scenario_item(REFRESH));
        changed(&mut app, demo_cockpit(NOW, DemoCockpit::Empty));
        assert_eq!(testing::count::<ScenarioField>(&mut app), 0);
        changed(&mut app, demo_cockpit(NOW, DemoCockpit::Scenario));
        select(&mut app, &pr, scenario_item(REFRESH));
        assert!(testing::count::<ScenarioField>(&mut app) > 0);
    }

    #[test]
    fn a_failed_turn_shows_one_line_at_the_top_of_the_item_center() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.failed = Some("the agent stopped".into());
        let (mut app, _pr) = app_with(view);
        assert_eq!(
            testing::count::<super::super::frame::CockpitDismissFailed>(&mut app),
            1
        );
        assert!(testing::shows(&mut app, "the agent stopped"));
    }
}
