//! The cases column: the selected function's signature, its cases, and the open case's values as
//! fields that save on Enter. Every name, value and reason is drawn through `printable` as plain
//! text; a field shows the printable form of a value and a click on a run button reads the saved
//! value back for a field nobody changed.

use std::collections::{BTreeMap, HashMap};

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::InputFocus;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::{EditableText, TextEdit, TextSpan};
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};
use clusia_core::PrRef;
use clusia_core::functions::{
    CASES_MAX, CaseInput, CaseOrigin, CasesAsk, FunctionsActivity, ParamKind, Support,
};
use clusia_core::printable::printable;

use super::{FunctionsModel, FunctionsModels, disabled_reason, signature_parts};
use crate::agent_name;
use crate::bridge::{Ask, Asks, FunctionsAskKind, Model, Toasts};
use crate::fonts::UiFonts;
use crate::review_state::ReviewTabs;
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::screens::review::tests_tab::plan::Refocus;
use crate::screens::review::tests_tab::{TestsModel, TestsModels, row_duration};
use crate::theme::Swatch;
use crate::ui::kit::{
    Clickable, Field, FieldCommitted, Fill, HoverFill, Ink, Stroke, Type, Variant, button, card,
    disabled_button, font, text, text_field, wrapped_text,
};

/// The card of the selected function's cases.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FunctionsCases(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CaseRow {
    pub pr: PrRef,
    pub function: String,
    pub case: String,
}

/// The ▶ of a case row.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CaseRun {
    pub pr: PrRef,
    pub function: String,
    pub case: String,
}

/// The text field of one parameter of the open case.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CaseField {
    pub pr: PrRef,
    pub function: String,
    pub case: String,
    pub param: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RunThisCase {
    pub pr: PrRef,
    pub function: String,
    pub case: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct NewValues {
    pub pr: PrRef,
    pub function: String,
    pub case: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ResetCaseButton {
    pub pr: PrRef,
    pub function: String,
    pub case: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RemoveCaseButton {
    pub pr: PrRef,
    pub function: String,
    pub case: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct AddCase {
    pub pr: PrRef,
    pub function: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct MoreCases {
    pub pr: PrRef,
    pub function: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FreshCases {
    pub pr: PrRef,
    pub function: String,
}

/// The `CasesView` the region it sits on was drawn from.
#[derive(Component)]
pub struct CasesBuilt(CasesView);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Header {
    pub bold: String,
    pub rest: String,
    pub intro: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Glyph {
    Pass,
    Fail,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FieldView {
    pub param: String,
    pub label: String,
    /// The saved value, as the daemon has it.
    pub value: String,
    pub error: Option<String>,
    pub whole_row: bool,
}

/// What the open case holds under its row. A `Some` reason says why that button is off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OpenCase {
    pub fields: Vec<FieldView>,
    pub run_off: Option<&'static str>,
    /// Why the buttons that edit or write (New fake values, Reset, Remove) are off.
    pub edit_off: Option<&'static str>,
    pub reset: bool,
    pub remove: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CaseRowView {
    pub id: String,
    pub name: String,
    pub glyph: Glyph,
    pub took: Option<String>,
    pub run_off: Option<&'static str>,
    pub open: Option<OpenCase>,
}

/// The lines under the rows. `add_full` is the function being at `CASES_MAX`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Tail {
    pub add_full: bool,
    pub off: Option<&'static str>,
    pub more: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NoteButton {
    pub label: String,
    pub off: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Body {
    Cases(Vec<CaseRowView>, Tail),
    Note(String, Option<NoteButton>),
    Writing(String),
    Empty,
}

/// Everything the column draws, as text: two columns that draw the same are equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CasesView {
    /// The selected function's slug; empty when none is.
    pub function: String,
    pub header: Option<Header>,
    pub body: Body,
}

/// What the column draws for `model`. `run_reason` and `write_reason` say why the run buttons and
/// the buttons that write are off, when they are.
pub(super) fn cases_view(
    model: &FunctionsModel,
    agent: &str,
    run_reason: Option<&'static str>,
    write_reason: Option<&'static str>,
) -> CasesView {
    let Some(f) = model.selected_view() else {
        return CasesView {
            function: String::new(),
            header: None,
            body: Body::Empty,
        };
    };
    let (bold, rest) = signature_parts(&printable(&f.signature), &printable(&f.name));
    let header = Some(Header {
        bold,
        rest,
        intro: format!(
            "Cases written by {agent} from the diff and the existing tests. Edit any value, or add your own."
        ),
    });
    let mut view = CasesView {
        function: f.slug.clone(),
        header,
        body: Body::Empty,
    };
    let writing = match model.view.as_ref().and_then(|v| v.activity.as_ref()) {
        Some(FunctionsActivity::Writing { function, ask })
            if function.as_deref().is_none_or(|s| s == f.slug) =>
        {
            Some(ask.clone())
        }
        _ => None,
    };
    if let Some(ask) = writing {
        let line = match ask {
            Some(CasesAsk::Values { case }) => {
                let name = f
                    .cases
                    .iter()
                    .find(|c| c.id == case)
                    .map_or(String::new(), |c| printable(&c.name));
                format!("{agent} is writing new values for “{name}”…")
            }
            _ => format!("{agent} is writing cases…"),
        };
        view.body = Body::Writing(line);
        return view;
    }
    let ask = |label: String| NoteButton {
        label,
        off: write_reason,
    };
    view.body = match &f.support {
        Support::Ready => {
            let rows = f
                .cases
                .iter()
                .map(|c| {
                    let result = model.current_result(c);
                    let open =
                        (model.open_case.as_deref() == Some(c.id.as_str())).then(|| OpenCase {
                            fields: f
                                .params
                                .iter()
                                .map(|p| {
                                    let value = c.values.get(&p.name).cloned().unwrap_or_default();
                                    FieldView {
                                        param: p.name.clone(),
                                        label: printable(&p.label),
                                        error: model
                                            .field_errors
                                            .get(&(f.slug.clone(), c.id.clone(), p.name.clone()))
                                            .cloned(),
                                        whole_row: p.kind == ParamKind::Json
                                            || value.chars().count() > 30,
                                        value,
                                    }
                                })
                                .collect(),
                            run_off: run_reason,
                            edit_off: write_reason,
                            reset: c.origin == CaseOrigin::Agent && c.edited,
                            remove: c.origin == CaseOrigin::Reviewer,
                        });
                    CaseRowView {
                        id: c.id.clone(),
                        name: printable(&c.name),
                        glyph: match result {
                            Some(r) if r.status == clusia_core::functions::CaseStatus::Passed => {
                                Glyph::Pass
                            }
                            Some(_) => Glyph::Fail,
                            None => Glyph::None,
                        },
                        took: result.map(|r| row_duration(r.took_ms)),
                        run_off: run_reason,
                        open,
                    }
                })
                .collect();
            Body::Cases(
                rows,
                Tail {
                    add_full: f.cases.len() >= CASES_MAX,
                    off: write_reason,
                    more: format!("Ask {agent} for more cases"),
                },
            )
        }
        Support::NoCases => Body::Note(
            format!("{agent} has not written cases for this function yet."),
            Some(ask(format!("Ask {agent} for cases"))),
        ),
        Support::Stale { reason } => {
            Body::Note(printable(reason), Some(ask("Write new cases".to_string())))
        }
        Support::NoCommand => {
            let (label, key) = f
                .lang
                .map_or(("these", "functions"), |l| (l.label(), l.key()));
            Body::Note(
                format!(
                    "The recipe has no command to run {label} functions. Refresh the recipe, or add functions.{key} to it."
                ),
                None,
            )
        }
        Support::Unsupported { reason } => Body::Note(
            format!("Clúsia can't run this function yet: {}", printable(reason)),
            None,
        ),
    };
    view
}

type Spawner<'a> = ChildSpawnerCommands<'a>;

/// `(case, param)` of the field that holds keyboard focus, when it is one of `pr`'s.
fn focused_key(
    focus: &InputFocus,
    fields: &Query<(&CaseField, &EditableText, &Field)>,
    pr: &PrRef,
) -> Option<(String, String)> {
    let (field, ..) = fields.get(focus.get()?).ok()?;
    (field.pr == *pr).then(|| (field.case.clone(), field.param.clone()))
}

#[allow(clippy::too_many_arguments)]
pub fn fill_cases(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    tests: Res<TestsModels>,
    functions: Res<FunctionsModels>,
    app_model: Res<Model>,
    fonts: Res<UiFonts>,
    focus: Res<InputFocus>,
    fields: Query<(&CaseField, &EditableText, &Field)>,
    regions: Query<(Entity, &FunctionsCases, Option<&CasesBuilt>)>,
) {
    for (entity, region, built) in &regions {
        let pr = &region.0;
        let Some(tab) = tabs.0.get(pr) else {
            continue;
        };
        if tab.ready().is_none() {
            continue;
        }
        let Some(model) = functions.0.get(pr).filter(|m| m.view.is_some()) else {
            continue;
        };
        let tests: Option<&TestsModel> = tests.0.get(pr);
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let view = cases_view(
            model,
            &agent,
            disabled_reason(&tabs, &app_model, tests, model, pr, true),
            disabled_reason(&tabs, &app_model, tests, model, pr, false),
        );
        if built.is_some_and(|b| b.0 == view) {
            continue;
        }
        let focused = focused_key(&focus, &fields, pr);
        // What a person typed and has not saved, and a refused text under its message, stay in
        // their fields: the region redraws for results and button states, not for edits.
        let typed: HashMap<(String, String), String> = fields
            .iter()
            .filter(|(f, ..)| f.pr == *pr)
            .filter(|(f, text, kept)| {
                text.value().to_string() != kept.committed
                    || model.field_errors.contains_key(&(
                        f.function.clone(),
                        f.case.clone(),
                        f.param.clone(),
                    ))
            })
            .map(|(f, t, _)| ((f.case.clone(), f.param.clone()), t.value().to_string()))
            .collect();
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| draw(p, &fonts, pr, &view, focused.as_ref(), &typed));
        commands.entity(entity).insert(CasesBuilt(view));
    }
}

fn draw(
    p: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    view: &CasesView,
    focused: Option<&(String, String)>,
    typed: &HashMap<(String, String), String>,
) {
    p.spawn(card(Node {
        flex_direction: FlexDirection::Column,
        flex_grow: 1.0,
        min_height: px(0),
        overflow: Overflow::scroll_y(),
        ..default()
    }))
    .with_children(|c| {
        if let Some(h) = &view.header {
            c.spawn(Node {
                width: percent(100),
                flex_shrink: 0.0,
                flex_direction: FlexDirection::Column,
                row_gap: px(6),
                padding: UiRect::axes(px(16), px(12)),
                ..default()
            })
            .with_children(|head| {
                let t = Type::MONO.ink(Swatch::Fg).size(13.0);
                // JetBrains Mono's weight axis runs to 800; 600 is hard to tell from 400 at this size.
                let bold = Type { weight: 700, ..t };
                head.spawn((
                    text(fonts, h.bold.clone(), bold),
                    TextLayout::linebreak(bevy::text::LineBreak::WordOrCharacter),
                    children![(
                        TextSpan::new(h.rest.clone()),
                        font(fonts, t),
                        TextColor::default(),
                        Ink(t.ink),
                    )],
                ));
                head.spawn(wrapped_text(fonts, h.intro.clone(), Type::MUTED));
            });
        }
        match &view.body {
            Body::Cases(rows, tail) => {
                for row in rows {
                    draw_case(c, fonts, pr, &view.function, row, focused, typed);
                }
                draw_tail(c, fonts, pr, &view.function, tail);
            }
            Body::Note(line, action) => {
                c.spawn(Node {
                    width: percent(100),
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::FlexStart,
                    row_gap: px(10),
                    padding: UiRect::axes(px(16), px(10)),
                    ..default()
                })
                .with_children(|n| {
                    n.spawn(wrapped_text(fonts, line.clone(), Type::MUTED));
                    if let Some(action) = action {
                        match action.off {
                            None => {
                                n.spawn((
                                    button(fonts, &action.label, Variant::Primary),
                                    FreshCases {
                                        pr: pr.clone(),
                                        function: view.function.clone(),
                                    },
                                    observe(on_fresh),
                                ));
                            }
                            Some(reason) => off(n, fonts, &action.label, reason),
                        }
                    }
                });
            }
            Body::Writing(line) => {
                c.spawn(Node {
                    width: percent(100),
                    padding: UiRect::axes(px(16), px(10)),
                    ..default()
                })
                .with_child(wrapped_text(fonts, line.clone(), Type::MUTED));
            }
            Body::Empty => {
                c.spawn(Node {
                    width: percent(100),
                    padding: UiRect::axes(px(16), px(10)),
                    ..default()
                })
                .with_child(wrapped_text(
                    fonts,
                    "Select a function to see its cases.",
                    Type::MUTED,
                ));
            }
        }
    });
}

/// A disabled button and the reason under it.
fn off(p: &mut Spawner, fonts: &UiFonts, label: &str, reason: &'static str) {
    p.spawn(disabled_button(fonts, label));
    p.spawn(wrapped_text(fonts, reason, Type::META));
}

fn draw_case(
    c: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    function: &str,
    row: &CaseRowView,
    focused: Option<&(String, String)>,
    typed: &HashMap<(String, String), String>,
) {
    let open = row.open.is_some();
    let mut shell = c.spawn((
        Node {
            width: percent(100),
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            border: UiRect::top(px(1)),
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
        BackgroundColor::default(),
        Fill(if open { Swatch::Chrome } else { Swatch::Clear }),
    ));
    shell.with_children(|s| {
        s.spawn(Node {
            width: percent(100),
            height: px(super::CASE_ROW_HEIGHT),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|line| {
            line.spawn((
                Node {
                    flex_grow: 1.0,
                    min_width: px(0),
                    height: percent(100),
                    align_items: AlignItems::Center,
                    column_gap: px(10),
                    padding: UiRect::left(px(16)),
                    ..default()
                },
                (WidgetButton, Clickable),
                Hovered::default(),
                TabIndex(0),
                BackgroundColor::default(),
                Fill(Swatch::Clear),
                HoverFill(Swatch::Hover),
                CaseRow {
                    pr: pr.clone(),
                    function: function.to_string(),
                    case: row.id.clone(),
                },
                observe(on_case_row),
            ))
            .with_children(|r| {
                let (glyph, ink) = match row.glyph {
                    Glyph::Pass => ("✓", Swatch::Green),
                    Glyph::Fail => ("×", Swatch::Orange),
                    Glyph::None => (" ", Swatch::Faint),
                };
                r.spawn(text(fonts, glyph, Type::BODY.ink(ink)));
                r.spawn(Node {
                    flex_grow: 1.0,
                    min_width: px(0),
                    ..default()
                })
                .with_child(text(
                    fonts,
                    row.name.clone(),
                    if open { Type::STRONG } else { Type::BODY },
                ));
                if let Some(took) = &row.took {
                    r.spawn(text(fonts, took.clone(), Type::META.ink(Swatch::Muted)));
                }
            });
            match row.run_off {
                None => {
                    line.spawn((
                        Node {
                            width: px(34),
                            height: percent(100),
                            align_items: AlignItems::Center,
                            justify_content: JustifyContent::Center,
                            ..default()
                        },
                        (WidgetButton, Clickable),
                        Hovered::default(),
                        TabIndex(0),
                        BackgroundColor::default(),
                        Fill(Swatch::Clear),
                        HoverFill(Swatch::Hover),
                        CaseRun {
                            pr: pr.clone(),
                            function: function.to_string(),
                            case: row.id.clone(),
                        },
                        observe(on_run),
                    ))
                    .with_child(text(fonts, "▶", Type::META.ink(Swatch::Fg)));
                }
                Some(_) => {
                    line.spawn(Node {
                        width: px(34),
                        justify_content: JustifyContent::Center,
                        ..default()
                    })
                    .with_child(text(fonts, "▶", Type::META));
                }
            }
        });
        if let Some(open) = &row.open {
            draw_open(s, fonts, pr, function, &row.id, open, focused, typed);
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn draw_open(
    s: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    function: &str,
    case: &str,
    open: &OpenCase,
    focused: Option<&(String, String)>,
    typed: &HashMap<(String, String), String>,
) {
    s.spawn(Node {
        width: percent(100),
        flex_direction: FlexDirection::Column,
        row_gap: px(8),
        padding: UiRect::new(px(16), px(16), px(6), px(14)),
        ..default()
    })
    .with_children(|b| {
        let mut line: Vec<&FieldView> = Vec::new();
        let mut rows: Vec<Vec<&FieldView>> = Vec::new();
        for f in &open.fields {
            if f.whole_row {
                if !line.is_empty() {
                    rows.push(std::mem::take(&mut line));
                }
                rows.push(vec![f]);
            } else {
                line.push(f);
                if line.len() == 2 {
                    rows.push(std::mem::take(&mut line));
                }
            }
        }
        if !line.is_empty() {
            rows.push(line);
        }
        for row in rows {
            b.spawn(Node {
                width: percent(100),
                column_gap: px(10),
                align_items: AlignItems::FlexStart,
                ..default()
            })
            .with_children(|r| {
                for f in row {
                    field(r, fonts, pr, function, case, f, focused, typed);
                }
            });
        }
        b.spawn(Node {
            column_gap: px(10),
            row_gap: px(6),
            flex_wrap: FlexWrap::Wrap,
            align_items: AlignItems::FlexStart,
            padding: UiRect::top(px(4)),
            ..default()
        })
        .with_children(|buttons| {
            let key = |pr: &PrRef| (pr.clone(), function.to_string(), case.to_string());
            let (pr, function, case) = key(pr);
            match open.run_off {
                None => {
                    buttons.spawn((
                        button(fonts, "▶ Run this case", Variant::Primary),
                        RunThisCase {
                            pr: pr.clone(),
                            function: function.clone(),
                            case: case.clone(),
                        },
                        observe(on_run),
                    ));
                }
                Some(reason) => off(buttons, fonts, "▶ Run this case", reason),
            }
            match open.edit_off {
                None => {
                    buttons.spawn((
                        button(fonts, "New fake values", Variant::Secondary),
                        NewValues {
                            pr: pr.clone(),
                            function: function.clone(),
                            case: case.clone(),
                        },
                        observe(on_new_values),
                    ));
                }
                Some(reason) => off(buttons, fonts, "New fake values", reason),
            }
            if open.reset {
                match open.edit_off {
                    None => {
                        buttons.spawn((
                            button(fonts, "Reset", Variant::Secondary),
                            ResetCaseButton {
                                pr: pr.clone(),
                                function: function.clone(),
                                case: case.clone(),
                            },
                            observe(on_reset),
                        ));
                    }
                    Some(reason) => off(buttons, fonts, "Reset", reason),
                }
            }
            if open.remove {
                match open.edit_off {
                    None => {
                        buttons.spawn((
                            button(fonts, "Remove", Variant::Danger),
                            RemoveCaseButton {
                                pr: pr.clone(),
                                function: function.clone(),
                                case: case.clone(),
                            },
                            observe(on_remove),
                        ));
                    }
                    Some(reason) => off(buttons, fonts, "Remove", reason),
                }
            }
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn field(
    r: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    function: &str,
    case: &str,
    f: &FieldView,
    focused: Option<&(String, String)>,
    typed: &HashMap<(String, String), String>,
) {
    r.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(3),
        ..default()
    })
    .with_children(|cell| {
        cell.spawn(Node {
            align_items: AlignItems::Center,
            column_gap: px(8),
            ..default()
        })
        .with_children(|pair| {
            pair.spawn(Node {
                width: px(LABEL_WIDTH),
                flex_shrink: 0.0,
                justify_content: JustifyContent::FlexEnd,
                ..default()
            })
            .with_child(text(fonts, f.label.clone(), Type::MUTED));
            let key = (case.to_string(), f.param.clone());
            let saved = printable(&f.value);
            let shown = typed.get(&key).unwrap_or(&saved).clone();
            let editable = text_field(
                fonts,
                &shown,
                if f.whole_row { WHOLE_WIDTH } else { PAIR_WIDTH },
                true,
            );
            let mut field = pair.spawn((
                editable,
                CaseField {
                    pr: pr.clone(),
                    function: function.to_string(),
                    case: case.to_string(),
                    param: f.param.clone(),
                },
            ));
            // The saved value stays the field's committed one, so unsaved text is still an
            // edit to commit.
            field.insert(Field { committed: saved });
            if focused == Some(&key) {
                field.insert(Refocus);
            } else {
                // A field nobody is in shows the start of its value, not the end the cursor
                // is created at.
                field.queue(|mut e: EntityWorldMut| {
                    if let Some(mut t) = e.get_mut::<EditableText>() {
                        t.pending_edits.clear();
                        t.queue_edit(TextEdit::TextStart(false));
                    }
                });
            }
        });
        if let Some(error) = &f.error {
            cell.spawn(Node {
                padding: UiRect::left(px(LABEL_WIDTH + 8.0)),
                ..default()
            })
            .with_child(wrapped_text(
                fonts,
                error.clone(),
                Type::META.ink(Swatch::Orange),
            ));
        }
    });
}

const LABEL_WIDTH: f32 = 120.0;
const PAIR_WIDTH: f32 = 150.0;
const WHOLE_WIDTH: f32 = 420.0;

fn draw_tail(c: &mut Spawner, fonts: &UiFonts, pr: &PrRef, function: &str, tail: &Tail) {
    c.spawn((
        Node {
            width: percent(100),
            flex_shrink: 0.0,
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            flex_wrap: FlexWrap::Wrap,
            padding: UiRect::axes(px(16), px(6)),
            border: UiRect::top(px(1)),
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|t| {
        if tail.add_full {
            t.spawn(text(
                fonts,
                "This function has 20 cases, the most it keeps.",
                Type::META,
            ));
        } else if tail.off.is_none() {
            t.spawn((
                button(fonts, "+ Add a case", Variant::Ghost),
                AddCase {
                    pr: pr.clone(),
                    function: function.to_string(),
                },
                observe(on_add),
            ));
        } else {
            t.spawn(disabled_button(fonts, "+ Add a case"));
        }
        if tail.off.is_none() {
            t.spawn((
                button(fonts, &tail.more, Variant::Ghost),
                MoreCases {
                    pr: pr.clone(),
                    function: function.to_string(),
                },
                observe(on_more),
            ));
        } else {
            t.spawn(disabled_button(fonts, &tail.more));
        }
    });
}

/// The text of each field of `case`, read back for a run: a field nobody touched holds the
/// printable form of the saved value, so the saved value itself stands for it.
fn shown_values(
    fields: &Query<(&CaseField, &EditableText)>,
    model: &FunctionsModel,
    pr: &PrRef,
    function: &str,
    case: &str,
) -> BTreeMap<String, String> {
    let saved = model
        .view
        .as_ref()
        .and_then(|v| v.functions.iter().find(|f| f.slug == function))
        .and_then(|f| f.cases.iter().find(|c| c.id == case))
        .map(|c| c.values.clone())
        .unwrap_or_default();
    let mut shown = saved.clone();
    for (f, text) in fields.iter() {
        if f.pr == *pr && f.function == function && f.case == case {
            let value = text.value().to_string();
            if saved.get(&f.param).is_none_or(|s| printable(s) != value) {
                shown.insert(f.param.clone(), value);
            }
        }
    }
    shown
}

pub fn commit_fields(
    mut committed: MessageReader<FieldCommitted>,
    fields: Query<&CaseField>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    mut models: ResMut<FunctionsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    for message in committed.read() {
        let Ok(field) = fields.get(message.entity) else {
            continue;
        };
        if let Some(reason) = read_only_reason(&tabs, &app_model, &field.pr) {
            not_sent(&mut toasts, &time, reason);
            continue;
        }
        let Some(model) = models.0.get_mut(&field.pr) else {
            continue;
        };
        let saved = model
            .view
            .as_ref()
            .and_then(|v| v.functions.iter().find(|f| f.slug == field.function))
            .and_then(|f| f.cases.iter().find(|c| c.id == field.case))
            .and_then(|c| c.values.get(&field.param))
            .cloned();
        // The printable form of the saved value is what the field shows, not an edit.
        let value = match saved {
            Some(saved) if printable(&saved) == message.value => saved,
            _ => message.value.clone(),
        };
        if let Some(ask) = model.commit_field(
            &field.pr,
            &field.function,
            &field.case,
            &field.param,
            &value,
        ) {
            asks.send(ask);
        }
    }
}

fn on_case_row(activate: On<Activate>, rows: Query<&CaseRow>, mut models: ResMut<FunctionsModels>) {
    let Ok(CaseRow { pr, case, .. }) = rows.get(activate.entity) else {
        return;
    };
    if let Some(model) = models.0.get_mut(pr) {
        model.open(case);
    }
}

#[allow(clippy::too_many_arguments)]
fn on_run(
    activate: On<Activate>,
    run: Query<&CaseRun>,
    this: Query<&RunThisCase>,
    fields: Query<(&CaseField, &EditableText)>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    tests: Res<TestsModels>,
    mut models: ResMut<FunctionsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let (pr, function, case) = if let Ok(r) = run.get(activate.entity) {
        (&r.pr, &r.function, &r.case)
    } else if let Ok(r) = this.get(activate.entity) {
        (&r.pr, &r.function, &r.case)
    } else {
        return;
    };
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if let Some(reason) = disabled_reason(&tabs, &app_model, tests.0.get(pr), model, pr, true) {
        return not_sent(&mut toasts, &time, reason);
    }
    let shown = shown_values(&fields, model, pr, function, case);
    if let Some(ask) = model.click_run(pr, function, case, shown) {
        asks.send(ask);
    }
}

#[allow(clippy::too_many_arguments)]
fn write(
    pr: &PrRef,
    function: &str,
    ask: CasesAsk,
    tabs: &ReviewTabs,
    app_model: &Model,
    tests: &TestsModels,
    models: &mut FunctionsModels,
    asks: &mut Asks,
    toasts: &mut Toasts,
    time: &Time,
) {
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if let Some(reason) = disabled_reason(tabs, app_model, tests.0.get(pr), model, pr, false) {
        return not_sent(toasts, time, reason);
    }
    asks.send(Ask::WriteCases {
        pr: pr.clone(),
        function: function.to_string(),
        ask,
    });
    model.sent(FunctionsAskKind::Write);
}

#[allow(clippy::too_many_arguments)]
fn on_new_values(
    activate: On<Activate>,
    buttons: Query<&NewValues>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    tests: Res<TestsModels>,
    mut models: ResMut<FunctionsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(NewValues { pr, function, case }) = buttons.get(activate.entity) else {
        return;
    };
    write(
        pr,
        function,
        CasesAsk::Values { case: case.clone() },
        &tabs,
        &app_model,
        &tests,
        &mut models,
        &mut asks,
        &mut toasts,
        &time,
    );
}

#[allow(clippy::too_many_arguments)]
fn on_more(
    activate: On<Activate>,
    buttons: Query<&MoreCases>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    tests: Res<TestsModels>,
    mut models: ResMut<FunctionsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(MoreCases { pr, function }) = buttons.get(activate.entity) else {
        return;
    };
    write(
        pr,
        function,
        CasesAsk::More,
        &tabs,
        &app_model,
        &tests,
        &mut models,
        &mut asks,
        &mut toasts,
        &time,
    );
}

#[allow(clippy::too_many_arguments)]
fn on_fresh(
    activate: On<Activate>,
    buttons: Query<&FreshCases>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    tests: Res<TestsModels>,
    mut models: ResMut<FunctionsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(FreshCases { pr, function }) = buttons.get(activate.entity) else {
        return;
    };
    write(
        pr,
        function,
        CasesAsk::Fresh,
        &tabs,
        &app_model,
        &tests,
        &mut models,
        &mut asks,
        &mut toasts,
        &time,
    );
}

/// A reset or a removal of one case, refused where the other edits are.
#[allow(clippy::too_many_arguments)]
fn edit(
    pr: &PrRef,
    ask: Ask,
    kind: FunctionsAskKind,
    tabs: &ReviewTabs,
    app_model: &Model,
    tests: &TestsModels,
    models: &mut FunctionsModels,
    asks: &mut Asks,
    toasts: &mut Toasts,
    time: &Time,
) {
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    if let Some(reason) = disabled_reason(tabs, app_model, tests.0.get(pr), model, pr, false) {
        return not_sent(toasts, time, reason);
    }
    asks.send(ask);
    model.sent(kind);
}

#[allow(clippy::too_many_arguments)]
fn on_reset(
    activate: On<Activate>,
    buttons: Query<&ResetCaseButton>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    tests: Res<TestsModels>,
    mut models: ResMut<FunctionsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(ResetCaseButton { pr, function, case }) = buttons.get(activate.entity) else {
        return;
    };
    let ask = Ask::ResetCase {
        pr: pr.clone(),
        function: function.clone(),
        case: case.clone(),
    };
    edit(
        pr,
        ask,
        FunctionsAskKind::Reset,
        &tabs,
        &app_model,
        &tests,
        &mut models,
        &mut asks,
        &mut toasts,
        &time,
    );
}

#[allow(clippy::too_many_arguments)]
fn on_remove(
    activate: On<Activate>,
    buttons: Query<&RemoveCaseButton>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    tests: Res<TestsModels>,
    mut models: ResMut<FunctionsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(RemoveCaseButton { pr, function, case }) = buttons.get(activate.entity) else {
        return;
    };
    let ask = Ask::RemoveCase {
        pr: pr.clone(),
        function: function.clone(),
        case: case.clone(),
    };
    edit(
        pr,
        ask,
        FunctionsAskKind::Remove,
        &tabs,
        &app_model,
        &tests,
        &mut models,
        &mut asks,
        &mut toasts,
        &time,
    );
}

#[allow(clippy::too_many_arguments)]
fn on_add(
    activate: On<Activate>,
    buttons: Query<&AddCase>,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    tests: Res<TestsModels>,
    mut models: ResMut<FunctionsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(AddCase { pr, function }) = buttons.get(activate.entity) else {
        return;
    };
    let Some(f) = models
        .0
        .get(pr)
        .and_then(|m| m.view.as_ref())
        .and_then(|v| v.functions.iter().find(|f| f.slug == *function))
    else {
        return;
    };
    let values = f.cases.first().map_or_else(
        || {
            f.params
                .iter()
                .map(|p| (p.name.clone(), String::new()))
                .collect()
        },
        |c| c.values.clone(),
    );
    let ask = Ask::SaveCase {
        pr: pr.clone(),
        function: function.clone(),
        case: CaseInput {
            id: None,
            name: "New case".into(),
            values,
        },
    };
    edit(
        pr,
        ask,
        FunctionsAskKind::Save,
        &tabs,
        &app_model,
        &tests,
        &mut models,
        &mut asks,
        &mut toasts,
        &time,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, DemoFunctions};
    use crate::testing::{self, NOW};
    use clusia_core::functions::{CaseScope, CasesAsk, FunctionsActivity, Support};

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

    /// `app_with` and the first function selected, as it is not when it cannot run.
    fn app_on_first(view: clusia_core::functions::FunctionsView) -> (App, PrRef) {
        let slug = view.functions[0].slug.clone();
        let (mut app, pr) = app_with(view);
        app.world_mut()
            .resource_mut::<FunctionsModels>()
            .0
            .get_mut(&pr)
            .unwrap()
            .select(&slug);
        testing::settle(&mut app);
        (app, pr)
    }

    fn field(app: &mut App, case: &str, param: &str) -> Entity {
        testing::find::<CaseField>(app, |f| f.case == case && f.param == param)
    }

    #[test]
    fn the_cases_column_is_the_mockups() {
        let (mut app, _pr) = app_with(mockup());
        for line in [
            "pub async fn refresh(&self, now: Instant) -> Result<Token, RefreshError>",
            "Cases written by Claude Code from the diff and the existing tests. Edit any value, or add your own.",
            "Endpoint fails, old token kept",
            "0.21 s",
            "Valid token, no exchange",
            "0.01 s",
            "Expired token, one exchange",
            "0.32 s",
            "Two callers at once share one exchange",
            "1.84 s",
            "stored token",
            "endpoint status",
            "endpoint latency",
            "endpoint body",
            "▶ Run this case",
            "New fake values",
            "Reset",
            "+ Add a case",
            "Ask Claude Code for more cases",
        ] {
            assert!(testing::shows(&mut app, line), "missing {line:?}");
        }
    }

    #[test]
    fn the_failed_case_is_open_with_its_values_in_fields() {
        let (mut app, pr) = app_with(mockup());
        let model = &app.world().resource::<FunctionsModels>().0[&pr];
        assert_eq!(model.open_view().map(|c| c.id.as_str()), Some("a1"));
        let status = field(&mut app, "a1", "endpoint_status");
        let text = app.world().get::<bevy::text::EditableText>(status).unwrap();
        assert_eq!(text.value().to_string(), "500");
        assert_eq!(
            testing::count::<CaseField>(&mut app),
            5,
            "only the open case has fields"
        );
    }

    #[test]
    fn committing_a_value_saves_the_whole_case_and_a_bad_value_is_not_sent() {
        let (mut app, pr) = app_with(mockup());
        let status = field(&mut app, "a1", "endpoint_status");
        testing::type_into(&mut app, status, "3");
        app.world_mut()
            .write_message(crate::ui::kit::FieldCommitted {
                entity: status,
                value: "5003".into(),
            });
        app.update();
        let asks = testing::recorded(&mut app);
        let Some(Ask::SaveCase { function, case, .. }) = asks.first() else {
            panic!("a SaveCase, got {asks:?}");
        };
        assert_eq!(case.id.as_deref(), Some("a1"));
        assert_eq!(case.values["endpoint_status"], "5003");
        assert_eq!(case.values["stored_token"], "\"old\", expires +10 s");
        assert!(!function.is_empty());
        app.world_mut()
            .write_message(crate::ui::kit::FieldCommitted {
                entity: status,
                value: "five hundred".into(),
            });
        app.update();
        assert!(
            testing::recorded(&mut app).is_empty(),
            "a refused value is not sent"
        );
        assert!(testing::shows(&mut app, "Enter a whole number"));
        let _ = pr;
    }

    #[test]
    fn run_this_case_sends_the_runs_hash_when_nothing_is_edited() {
        let (mut app, pr) = app_with(mockup());
        let run = testing::find::<RunThisCase>(&mut app, |r| r.case == "a1");
        testing::activate(&mut app, run);
        let seen = app.world().resource::<FunctionsModels>().0[&pr]
            .selected_view()
            .unwrap()
            .hash
            .clone();
        let asks = testing::recorded(&mut app);
        assert!(matches!(
            asks.as_slice(),
            [Ask::RunCases { scope: CaseScope::Case { case, .. }, seen: s, .. }]
                if case == "a1" && *s == seen
        ));
        // ▶ on a closed row runs its saved values and saves nothing. A run in flight turns the
        // run buttons off, so it is a window of its own.
        let (mut app, _pr) = app_with(mockup());
        let closed = testing::find::<CaseRun>(&mut app, |r| r.case == "a2");
        testing::activate(&mut app, closed);
        let asks = testing::recorded(&mut app);
        assert!(
            matches!(
                asks.as_slice(),
                [Ask::RunCases { scope: CaseScope::Case { case, .. }, .. }] if case == "a2"
            ),
            "{asks:?}"
        );
    }

    #[test]
    fn run_after_an_unsaved_edit_saves_then_runs_with_the_new_hash() {
        let (mut app, pr) = app_with(mockup());
        let status = field(&mut app, "a1", "endpoint_status");
        testing::type_into(&mut app, status, "9");
        let run = testing::find::<RunThisCase>(&mut app, |r| r.case == "a1");
        testing::activate(&mut app, run);
        let asks = testing::recorded(&mut app);
        assert!(
            matches!(asks.as_slice(), [Ask::SaveCase { .. }]),
            "the edit is saved first, nothing runs yet: {asks:?}"
        );
        // The daemon answers the save with the view and its new hash.
        let mut view = mockup();
        view.functions[0].cases[0]
            .values
            .insert("endpoint_status".into(), "5009".into());
        view.functions[0].hash = "new-hash".into();
        testing::tell(
            &mut app,
            crate::bridge::Tell::Functions(crate::bridge::FunctionsTell::Loaded {
                pr: pr.clone(),
                view: Box::new(view),
            }),
        );
        let asks = testing::recorded(&mut app);
        assert!(
            matches!(
                asks.as_slice(),
                [Ask::RunCases { scope: CaseScope::Case { case, .. }, seen, .. }]
                    if case == "a1" && seen == "new-hash"
            ),
            "the queued run goes with the new hash: {asks:?}"
        );
    }

    #[test]
    fn new_fake_values_and_more_cases_ask_the_agent() {
        // A write in flight turns the next write off, so each ask gets its own window.
        let (mut app, _pr) = app_with(mockup());
        let fresh = testing::find::<NewValues>(&mut app, |n| n.case == "a1");
        testing::activate(&mut app, fresh);
        assert!(matches!(
            testing::recorded(&mut app).as_slice(),
            [Ask::WriteCases { ask: CasesAsk::Values { case }, .. }] if case == "a1"
        ));
        assert_eq!(
            testing::count::<MoreCases>(&mut app),
            0,
            "the next write is off"
        );
        let (mut app, pr) = app_with(mockup());
        let more = testing::find::<MoreCases>(&mut app, |m| m.pr == pr);
        testing::activate(&mut app, more);
        assert!(matches!(
            testing::recorded(&mut app).as_slice(),
            [Ask::WriteCases {
                ask: CasesAsk::More,
                ..
            }]
        ));
    }

    #[test]
    fn add_a_case_sends_a_reviewer_case_with_the_first_cases_values() {
        let (mut app, pr) = app_with(mockup());
        let add = testing::find::<AddCase>(&mut app, |a| a.pr == pr);
        testing::activate(&mut app, add);
        let asks = testing::recorded(&mut app);
        let [Ask::SaveCase { case, .. }] = asks.as_slice() else {
            panic!("one SaveCase, got {asks:?}");
        };
        assert_eq!(case.id, None);
        assert_eq!(case.name, "New case");
        assert_eq!(case.values["endpoint_status"], "500");
    }

    #[test]
    fn reset_shows_for_an_edited_agent_case_and_remove_for_a_reviewer_case() {
        let mut view = mockup();
        view.functions[0].cases[0].edited = true;
        let (mut app, pr) = app_with(view);
        let reset = testing::find::<ResetCaseButton>(&mut app, |r| r.case == "a1");
        testing::activate(&mut app, reset);
        assert!(matches!(
            testing::recorded(&mut app).as_slice(),
            [Ask::ResetCase { case, .. }] if case == "a1"
        ));
        let mut view = mockup();
        let mut mine = view.functions[0].cases[0].clone();
        mine.id = "r1".into();
        mine.origin = clusia_core::functions::CaseOrigin::Reviewer;
        mine.edited = true;
        mine.result = None;
        view.functions[0].cases.push(mine);
        let (mut app, pr2) = app_with(view);
        let model = &mut app.world_mut().resource_mut::<FunctionsModels>().0;
        model.get_mut(&pr2).unwrap().open_case = Some("r1".into());
        model.get_mut(&pr2).unwrap().touch();
        app.update();
        let remove = testing::find::<RemoveCaseButton>(&mut app, |r| r.case == "r1");
        testing::activate(&mut app, remove);
        assert!(matches!(
            testing::recorded(&mut app).as_slice(),
            [Ask::RemoveCase { case, .. }] if case == "r1"
        ));
        let _ = pr;
    }

    #[test]
    fn the_notes_of_a_function_that_cannot_run_are_plain() {
        for (support, shown) in [
            (
                Support::Unsupported {
                    reason: "Not public: a driver can't call it from outside its crate".into(),
                },
                "Clúsia can't run this function yet: Not public: a driver can't call it from outside its crate",
            ),
            (
                Support::NoCommand,
                "The recipe has no command to run Rust functions. Refresh the recipe, or add functions.rust to it.",
            ),
            (
                Support::NoCases,
                "Claude Code has not written cases for this function yet.",
            ),
            (
                Support::Stale {
                    reason: "The signature changed since these cases were written".into(),
                },
                "The signature changed since these cases were written",
            ),
        ] {
            let mut view = mockup();
            view.functions[0].support = support;
            let (mut app, _pr) = app_on_first(view);
            assert!(testing::shows(&mut app, shown), "missing {shown:?}");
        }
    }

    #[test]
    fn a_stale_or_empty_function_offers_new_cases_not_a_run() {
        let mut view = mockup();
        view.functions[0].support = Support::Stale { reason: "x".into() };
        let (mut app, pr) = app_on_first(view);
        assert!(testing::shows(&mut app, "Write new cases"));
        assert_eq!(testing::count::<RunThisCase>(&mut app), 0);
        let write = testing::find::<FreshCases>(&mut app, |f| f.pr == pr);
        testing::activate(&mut app, write);
        assert!(matches!(
            testing::recorded(&mut app).as_slice(),
            [Ask::WriteCases {
                ask: CasesAsk::Fresh,
                ..
            }]
        ));
    }

    #[test]
    fn while_the_agent_writes_the_buttons_are_off_and_the_line_says_what_it_does() {
        let mut view = mockup();
        view.activity = Some(FunctionsActivity::Writing {
            function: Some(view.functions[0].slug.clone()),
            ask: Some(CasesAsk::Values { case: "a1".into() }),
        });
        let (mut app, _pr) = app_with(view);
        assert!(testing::shows(
            &mut app,
            "Claude Code is writing new values for “Endpoint fails, old token kept”…"
        ));
        assert_eq!(testing::count::<RunThisCase>(&mut app), 0);
        assert_eq!(testing::count::<NewValues>(&mut app), 0);
    }

    #[test]
    fn a_field_that_had_focus_keeps_it_across_a_rebuild() {
        use bevy::input_focus::InputFocus;
        let (mut app, pr) = app_with(mockup());
        let status = field(&mut app, "a1", "endpoint_status");
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(status, bevy::input_focus::FocusCause::Navigated);
        let mut view = mockup();
        view.functions[0].cases[1].name = "Valid token, no exchange (renamed)".into();
        testing::tell(
            &mut app,
            crate::bridge::Tell::Functions(crate::bridge::FunctionsTell::Loaded {
                pr: pr.clone(),
                view: Box::new(view),
            }),
        );
        let again = field(&mut app, "a1", "endpoint_status");
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(again));
    }

    #[test]
    fn hostile_text_is_drawn_as_plain_text() {
        let mut view = mockup();
        view.functions[0].cases[0].name = "[x](https://example.test) **bold**\u{202e}end".into();
        view.functions[0].cases[0].values.insert(
            "stored_token".into(),
            "[y](https://example.test)\u{1b}[31m".into(),
        );
        let (mut app, _pr) = app_with(view);
        assert!(testing::shows(
            &mut app,
            "[x](https://example.test) **bold**\\u{202e}end"
        ));
        assert_eq!(
            testing::count::<crate::ui::markdown::MdBody>(&mut app),
            0,
            "nothing here is parsed as markdown"
        );
        assert_eq!(testing::count::<crate::ui::markdown::MdLink>(&mut app), 0);
        let token = field(&mut app, "a1", "stored_token");
        assert_eq!(
            app.world()
                .get::<EditableText>(token)
                .unwrap()
                .value()
                .to_string(),
            "[y](https://example.test)\\u{1b}[31m"
        );
    }

    fn texts_equal(app: &mut App, line: &str) -> usize {
        testing::shown_text(app)
            .iter()
            .filter(|t| *t == line)
            .count()
    }

    fn loaded(app: &mut App, pr: &PrRef, view: clusia_core::functions::FunctionsView) {
        testing::tell(
            app,
            crate::bridge::Tell::Functions(crate::bridge::FunctionsTell::Loaded {
                pr: pr.clone(),
                view: Box::new(view),
            }),
        );
    }

    #[test]
    fn text_typed_into_a_field_survives_a_change_of_the_view() {
        let (mut app, pr) = app_with(mockup());
        let token = field(&mut app, "a1", "stored_token");
        testing::type_into(&mut app, token, "X");
        let before = app
            .world()
            .get::<EditableText>(token)
            .unwrap()
            .value()
            .to_string();
        assert!(before.contains('X'));
        // A case finished: its glyph and duration change, the fields are not meant to.
        let mut view = mockup();
        view.functions[0].cases[1].result = None;
        loaded(&mut app, &pr, view);
        let token = field(&mut app, "a1", "stored_token");
        let after = app
            .world()
            .get::<EditableText>(token)
            .unwrap()
            .value()
            .to_string();
        assert_eq!(after, before);
        // Still an unsaved edit: a second change keeps it, and a click on run reads it.
        let mut view = mockup();
        view.functions[0].cases[2].result = None;
        loaded(&mut app, &pr, view);
        let token = field(&mut app, "a1", "stored_token");
        let again = app
            .world()
            .get::<EditableText>(token)
            .unwrap()
            .value()
            .to_string();
        assert_eq!(again, before);
        let run = testing::find::<RunThisCase>(&mut app, |r| r.case == "a1");
        testing::activate(&mut app, run);
        let asks = testing::recorded(&mut app);
        let Some(Ask::SaveCase { case, .. }) = asks.first() else {
            panic!("the unsaved edit is saved first: {asks:?}");
        };
        assert_eq!(case.values["stored_token"], before);
    }

    #[test]
    fn the_signature_up_to_its_name_is_heavier_than_the_rest() {
        let (mut app, _pr) = app_with(mockup());
        let mut q = app
            .world_mut()
            .query::<(&Text, &TextFont, Option<&Children>)>();
        let (_, root, kids) = q
            .iter(app.world())
            .find(|(t, _, _)| t.0 == "pub async fn refresh(")
            .expect("the signature's bold part");
        let bold = root.weight.0;
        let kid = kids.unwrap()[0];
        let rest = app.world().get::<TextFont>(kid).unwrap().weight.0;
        assert!(bold > rest, "bold {bold}, rest {rest}");
        assert!(
            app.world()
                .get::<TextSpan>(kid)
                .unwrap()
                .0
                .starts_with("&self"),
            "the rest follows the name"
        );
    }

    #[test]
    fn while_a_write_is_in_flight_every_button_that_edits_is_off_with_its_reason() {
        let mut view = mockup();
        view.functions[0].cases[0].edited = true;
        let (mut app, _pr) = app_with(view);
        let fresh = testing::find::<NewValues>(&mut app, |n| n.case == "a1");
        testing::activate(&mut app, fresh);
        testing::recorded(&mut app);
        for (name, n) in [
            ("Reset", testing::count::<ResetCaseButton>(&mut app)),
            ("New fake values", testing::count::<NewValues>(&mut app)),
            ("Run this case", testing::count::<RunThisCase>(&mut app)),
        ] {
            assert_eq!(n, 0, "{name} is not a live button");
            assert!(testing::shows(&mut app, name), "{name} is still drawn");
        }
        assert_eq!(
            texts_equal(&mut app, "Function cases are running"),
            4,
            "Run this case, New fake values and Reset each say why, and so does Run all cases"
        );
    }

    #[test]
    fn a_reviewers_case_cannot_be_removed_while_a_write_is_in_flight() {
        let mut view = mockup();
        let mut mine = view.functions[0].cases[0].clone();
        mine.id = "r1".into();
        mine.origin = clusia_core::functions::CaseOrigin::Reviewer;
        mine.result = None;
        view.functions[0].cases.push(mine);
        let (mut app, pr) = app_with(view);
        let model = &mut app.world_mut().resource_mut::<FunctionsModels>().0;
        model.get_mut(&pr).unwrap().open_case = Some("r1".into());
        model.get_mut(&pr).unwrap().touch();
        app.update();
        let more = testing::find::<MoreCases>(&mut app, |m| m.pr == pr);
        testing::activate(&mut app, more);
        assert_eq!(testing::count::<RemoveCaseButton>(&mut app), 0);
        assert!(testing::shows(&mut app, "Remove"));
        assert_eq!(texts_equal(&mut app, "Function cases are running"), 4);
    }

    #[test]
    fn a_case_whose_values_changed_after_its_result_shows_no_glyph_and_no_duration() {
        let (mut app, pr) = app_with(mockup());
        assert_eq!(texts_equal(&mut app, "✓"), 3);
        assert!(testing::shows(&mut app, "0.01 s"));
        let mut view = mockup();
        let case = &mut view.functions[0].cases[1];
        case.values.insert("endpoint_status".into(), "418".into());
        case.hash = "after-the-result".into();
        loaded(&mut app, &pr, view);
        assert!(!testing::shows(&mut app, "0.01 s"));
        assert_eq!(texts_equal(&mut app, "✓"), 2);
        assert!(testing::shows(&mut app, "Valid token, no exchange"));
    }

    #[test]
    fn committing_a_field_nobody_changed_sends_nothing() {
        let mut view = mockup();
        view.functions[0].cases[0].values.insert(
            "stored_token".into(),
            "[y](https://example.test)\u{1b}[31m".into(),
        );
        let (mut app, _pr) = app_with(view);
        for param in ["stored_token", "endpoint_status"] {
            let entity = field(&mut app, "a1", param);
            let value = app
                .world()
                .get::<EditableText>(entity)
                .unwrap()
                .value()
                .to_string();
            app.world_mut()
                .write_message(crate::ui::kit::FieldCommitted { entity, value });
            app.update();
            assert!(
                testing::recorded(&mut app).is_empty(),
                "{param} is unchanged"
            );
        }
    }

    #[test]
    fn an_unfocused_field_shows_the_start_of_its_value() {
        let (mut app, _pr) = app_with(mockup());
        let token = field(&mut app, "a1", "stored_token");
        // The window's text plugin applies it with a layout, which the headless app lacks.
        let text = app.world().get::<EditableText>(token).unwrap();
        assert!(matches!(
            text.pending_edits.as_slice(),
            [TextEdit::TextStart(false)]
        ));
    }
}
