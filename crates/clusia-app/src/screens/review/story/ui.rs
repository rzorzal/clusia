//! Draws the Story view. `StoryRoot` holds two parts, the left list and the body; each rebuilds
//! only when its pure view (`view::story_view`) changes, and the composer's typed text is in no
//! view, so typing never rebuilds them.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::{Strikethrough, TextSpan};
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::story::Tone as ChipTone;
use clusia_diagram::Rect as GRect;

use super::model::Stories;
use super::view::*;
use crate::bridge::{Ask, Asks, Connection, Model, Toasts};
use crate::fonts::UiFonts;
use crate::review_state::{DiffMode, EditTarget, Editor, ReviewTabs};
use crate::screens::review::agent::harness_ready;
use crate::screens::review::checks::Checks;
use crate::screens::review::diagrams::canvas::{Dots, dashed_ring};
use crate::screens::review::diff::{Gutter, HighlightCache, show_line, unified_rows, unsent};
use crate::screens::review::editor::{editor_box, not_sent, read_only_reason};
use crate::theme::{Swatch, Theme};
use crate::ui::composer::ComposerMode;
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Ink, Stroke, Tone, Type, Variant, badge, button, card, checkbox,
    disabled_button, font, panel, text,
};
use crate::ui::markdown::RenderOpts;
use crate::ui::spinner::spinner;

/// A step in the list (`row`) or a one-line card (`!row`): a click makes it current.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct StepButton {
    pub pr: PrRef,
    pub step: String,
    pub row: bool,
}

/// **Mark read · next** (`read: true`) or **Mark unread** (`read: false`) on the current card.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct MarkReadButton {
    pub pr: PrRef,
    pub step: String,
    pub read: bool,
    pub number: u32,
    /// The step that becomes current after a mark.
    pub next: Option<String>,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CommentStepButton {
    pub pr: PrRef,
    pub step: String,
    pub number: u32,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FullDiffButton {
    pub pr: PrRef,
    pub path: Option<String>,
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryAction {
    Stop,
    Rewrite,
    /// `rewrite`: the daemon lists a version.
    Retry {
        rewrite: bool,
    },
    Write,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct StoryActionButton {
    pub pr: PrRef,
    pub action: StoryAction,
}

/// The ◀ and ▶ of the version stepper (`-1` older, `1` newer).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct StoryStepper {
    pub pr: PrRef,
    pub step: i32,
}

/// The *Show changes since version n* checkbox.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct StoryCompareToggle(pub PrRef);

/// The dashed ring around the row of a step added since the version compared with.
#[derive(Component, Debug)]
pub struct AddedRing;

#[derive(Component)]
pub struct ListPart {
    pub pr: PrRef,
    built: Option<ListView>,
}

#[derive(Component)]
pub struct BodyPart {
    pub pr: PrRef,
    built: Option<(BodyView, RenderOpts)>,
}

/// The height of a list row that carries the dashed ring, so the ring has a fixed box.
const ADDED_ROW_HEIGHT: f32 = 54.0;
/// The width of a list row: the column's 258 px less its padding.
const ROW_WIDTH: f32 = 238.0;

/// The left list and the body, in the Story frame's root.
pub(super) fn parts(p: &mut ChildSpawnerCommands, pr: &PrRef) {
    p.spawn((
        Node {
            width: px(258),
            flex_shrink: 0.0,
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            border: UiRect::right(px(1)),
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
        ListPart {
            pr: pr.clone(),
            built: None,
        },
    ));
    p.spawn((
        panel(
            Node {
                flex_grow: 1.0,
                min_width: px(0),
                min_height: px(0),
                flex_direction: FlexDirection::Column,
                row_gap: px(12),
                padding: UiRect::axes(px(20), px(14)),
                overflow: Overflow {
                    x: OverflowAxis::Clip,
                    y: OverflowAxis::Scroll,
                },
                ..default()
            },
            Swatch::Surface,
        ),
        ScrollArea,
        ScrollPosition::default(),
        BodyPart {
            pr: pr.clone(),
            built: None,
        },
    ));
}

pub(super) fn add_systems(app: &mut App) {
    app.add_systems(
        Update,
        rebuild_story
            .after(super::trigger_write)
            .after(crate::screens::review::diff::fill_region),
    );
}

fn ctx_of<'a>(
    model: &Model,
    harness: &'a str,
    harness_ready: bool,
    code_size: f32,
) -> StoryCtx<'a> {
    StoryCtx {
        harness,
        harness_ready,
        story_setting: model.snapshot.config.harness.story,
        live: model.connection == Connection::Live,
        code_size,
    }
}

#[allow(clippy::too_many_arguments)]
fn rebuild_story(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    theme: Res<Theme>,
    model: Res<Model>,
    stories: Res<Stories>,
    checks: Res<Checks>,
    fonts: Res<UiFonts>,
    mut cache: ResMut<HighlightCache>,
    dots: Option<Res<Dots>>,
    mut last_opts: Local<Option<RenderOpts>>,
    mut lists: Query<(Entity, &mut ListPart)>,
    mut bodies: Query<(Entity, &mut BodyPart, &mut ScrollPosition)>,
) {
    let opts = RenderOpts::from_config(theme.code_size, &model.snapshot.config);
    let fresh = lists.iter().any(|(_, p)| p.built.is_none())
        || bodies.iter().any(|(_, p, _)| p.built.is_none());
    let opts_changed = last_opts.as_ref() != Some(&opts);
    if !(fresh
        || tabs.is_changed()
        || theme.is_changed()
        || model.is_changed()
        || stories.is_changed()
        || checks.is_changed()
        || opts_changed)
    {
        return;
    }
    *last_opts = Some(opts.clone());
    let mut views = HashMap::new();
    for pr in lists
        .iter()
        .map(|(_, p)| p.pr.clone())
        .chain(bodies.iter().map(|(_, p, _)| p.pr.clone()))
    {
        if views.contains_key(&pr) {
            continue;
        }
        let Some(tab) = tabs.0.get(&pr) else { continue };
        let harness = crate::agent_name::of_tab(&model.snapshot, tab);
        let ready = harness_ready(
            &model.snapshot,
            crate::agent_name::kind_of_tab(&model.snapshot, tab),
        );
        let ctx = ctx_of(&model, &harness, ready, theme.code_size);
        let view = story_view(tab, stories.0.get(&pr), checks.0.get(&pr), &ctx, &mut cache);
        views.insert(pr, (view, tab.ui.editor.clone()));
    }
    let fonts = &*fonts;
    for (entity, mut part) in &mut lists {
        let Some((view, _)) = views.get(&part.pr) else {
            continue;
        };
        let want = view.as_ref().map(|v| v.list.clone());
        if part.built.is_some() && part.built == want {
            continue;
        }
        let pr = part.pr.clone();
        commands.entity(entity).despawn_related::<Children>();
        if let Some(list) = &want {
            commands.entity(entity).with_children(|p| {
                list_view(p, fonts, &pr, list, dots.as_deref());
            });
        }
        part.built = want.or_else(|| Some(placeholder_list()));
    }
    for (entity, mut part, mut scroll) in &mut bodies {
        let Some((view, editor)) = views.get(&part.pr) else {
            continue;
        };
        let Some(view) = view else {
            if part.built.is_none() {
                commands.entity(entity).with_children(|p| {
                    p.spawn(text(fonts, "Reading the story…", Type::MUTED));
                });
                part.built = Some((placeholder_body(), opts.clone()));
            }
            continue;
        };
        let key = (view.body.clone(), opts.clone());
        if part.built.as_ref() == Some(&key) {
            continue;
        }
        let was = part
            .built
            .as_ref()
            .and_then(|(b, _)| b.card.as_ref().map(|c| c.step.clone()));
        if was != view.body.card.as_ref().map(|c| c.step.clone()) {
            scroll.0 = Vec2::ZERO; // another step starts at its top
        }
        let pr = part.pr.clone();
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|p| {
            body_view(p, fonts, &pr, &view.body, editor.as_ref(), &opts);
        });
        part.built = Some(key);
    }
}

fn placeholder_list() -> ListView {
    ListView {
        heading: String::new(),
        counts: String::new(),
        rows: Vec::new(),
        note: String::new(),
        footer: None,
    }
}

fn placeholder_body() -> BodyView {
    BodyView {
        banner: None,
        legend: None,
        summary: None,
        card: None,
        others: Vec::new(),
        code_size: 0.0,
        editor_error: None,
        saving: false,
    }
}

// ---- the list

fn list_view(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    list: &ListView,
    dots: Option<&Dots>,
) {
    p.spawn((
        Node {
            flex_grow: 1.0,
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(4),
            padding: UiRect::axes(px(10), px(12)),
            overflow: Overflow {
                x: OverflowAxis::Clip,
                y: OverflowAxis::Scroll,
            },
            ..default()
        },
        ScrollArea,
        ScrollPosition::default(),
    ))
    .with_children(|c| {
        c.spawn(Node {
            width: percent(100),
            justify_content: JustifyContent::SpaceBetween,
            padding: UiRect::bottom(px(4)),
            ..default()
        })
        .with_children(|h| {
            h.spawn(text(fonts, list.heading.clone(), Type::META));
            h.spawn(text(fonts, list.counts.clone(), Type::META));
        });
        for row in &list.rows {
            row_button(c, fonts, pr, row, dots);
        }
        c.spawn(Node {
            flex_grow: 1.0,
            min_height: px(8),
            ..default()
        });
        c.spawn(text(fonts, list.note.clone(), Type::META));
        if let Some(footer) = &list.footer {
            footer_view(c, fonts, pr, footer);
        }
    });
}

fn step_circle(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    number: Option<usize>,
    state: RowState,
) {
    let (fill, label, t) = match state {
        RowState::Read => (
            Swatch::Green,
            "✓".to_string(),
            Type::STRONG.ink(Swatch::OnGreen),
        ),
        RowState::Current => (
            Swatch::GreenSoft,
            number.map(|n| n.to_string()).unwrap_or_default(),
            Type::STRONG.ink(Swatch::Green),
        ),
        RowState::Open => (
            Swatch::Chrome,
            number.map(|n| n.to_string()).unwrap_or_default(),
            Type::STRONG.ink(Swatch::Muted),
        ),
        RowState::Removed => (
            Swatch::Clear,
            "–".to_string(),
            Type::STRONG.ink(Swatch::Muted),
        ),
    };
    p.spawn((
        Node {
            width: px(22),
            height: px(22),
            flex_shrink: 0.0,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            border_radius: BorderRadius::MAX,
            ..default()
        },
        BackgroundColor::default(),
        Fill(fill),
    ))
    .with_children(|c| {
        c.spawn(text(fonts, label, t));
    });
}

fn mark_badge(p: &mut ChildSpawnerCommands, fonts: &UiFonts, mark: Mark) {
    let (label, tone) = match mark {
        Mark::Added => ("added", Tone::Green),
        Mark::Changed => ("changed", Tone::Orange),
        Mark::Removed => ("removed", Tone::Neutral),
    };
    p.spawn(badge(fonts, label, tone));
}

fn row_button(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    row: &ListRow,
    dots: Option<&Dots>,
) {
    let removed = row.state == RowState::Removed;
    let added = row.mark == Some(Mark::Added);
    let fill = if row.state == RowState::Current {
        Swatch::GreenSoft
    } else {
        Swatch::Clear
    };
    let node = Node {
        width: percent(100),
        height: if added {
            px(ADDED_ROW_HEIGHT)
        } else {
            Val::Auto
        },
        min_height: px(40),
        flex_shrink: 0.0,
        column_gap: px(8),
        padding: UiRect::axes(px(8), px(7)),
        align_items: AlignItems::Center,
        border: px(1).all(),
        border_radius: BorderRadius::all(px(6)),
        overflow: if added {
            Overflow::clip()
        } else {
            Overflow::visible()
        },
        ..default()
    };
    let stroke = if row.mark == Some(Mark::Changed) {
        Swatch::Amber
    } else {
        Swatch::Clear
    };
    let mut e = p.spawn((
        node,
        BackgroundColor::default(),
        Fill(fill),
        BorderColor::default(),
        Stroke(stroke),
    ));
    if !removed {
        e.insert((
            WidgetButton,
            Hovered::default(),
            Clickable,
            TabIndex(0),
            HoverFill(if row.state == RowState::Current {
                Swatch::GreenSoft
            } else {
                Swatch::Hover
            }),
            StepButton {
                pr: pr.clone(),
                step: row.id.clone(),
                row: true,
            },
            observe(on_step),
        ));
    }
    e.with_children(|r| {
        step_circle(r, fonts, row.number, row.state);
        r.spawn(Node {
            flex_grow: 1.0,
            flex_basis: px(0),
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            ..default()
        })
        .with_children(|c| {
            let t = if row.mono {
                Type::MONO
            } else if row.mechanical || removed {
                Type::MUTED
            } else if row.state == RowState::Current {
                Type::STRONG
            } else {
                Type::BODY
            };
            if removed {
                c.spawn((
                    text(fonts, row.title.clone(), t.ink(Swatch::GhostInk)),
                    Strikethrough,
                ));
            } else {
                c.spawn(text(fonts, row.title.clone(), t));
            }
            c.spawn(text(fonts, row.meta.clone(), Type::META));
        });
        if let Some(mark) = row.mark {
            mark_badge(r, fonts, mark);
        }
        if added {
            r.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: px(0),
                    top: px(0),
                    ..default()
                },
                AddedRing,
            ))
            .with_children(|o| {
                dashed_ring(
                    o,
                    dots,
                    GRect {
                        x: 0.0,
                        y: 0.0,
                        w: ROW_WIDTH,
                        h: ADDED_ROW_HEIGHT,
                    },
                    6.0,
                    0.0,
                    Swatch::Green,
                );
            });
        }
    });
}

fn stepper_button(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    label: &str,
    step: i32,
    on: bool,
) {
    if on {
        p.spawn((
            button(fonts, label, Variant::Ghost),
            StoryStepper {
                pr: pr.clone(),
                step,
            },
            observe(on_stepper),
        ));
    } else {
        p.spawn(disabled_button(fonts, label));
    }
}

fn footer_view(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, f: &FooterView) {
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(6),
        padding: UiRect::top(px(8)),
        ..default()
    })
    .with_children(|c| {
        c.spawn(Node {
            align_items: AlignItems::Center,
            column_gap: px(6),
            ..default()
        })
        .with_children(|r| {
            stepper_button(r, fonts, pr, "◀", -1, f.prev);
            r.spawn(text(
                fonts,
                format!("Version {} of {}", f.number, f.of),
                Type::STRONG,
            ));
            stepper_button(r, fonts, pr, "▶", 1, f.next);
        });
        if f.old {
            c.spawn(text(fonts, "An older version: read-only.", Type::META));
        }
        if let Some((since, on)) = f.compare {
            c.spawn(Node {
                align_items: AlignItems::Center,
                column_gap: px(8),
                ..default()
            })
            .with_children(|r| {
                r.spawn((
                    checkbox(on),
                    StoryCompareToggle(pr.clone()),
                    observe(on_compare),
                ));
                r.spawn(text(
                    fonts,
                    format!("Show changes since version {since}"),
                    Type::BODY,
                ));
            });
        }
    });
}

// ---- the body

fn body_view(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    body: &BodyView,
    editor: Option<&Editor>,
    opts: &RenderOpts,
) {
    if let Some(banner) = &body.banner {
        banner_strip(p, fonts, pr, banner);
    }
    if let Some(legend) = &body.legend {
        legend_row(p, fonts, legend);
    }
    if let Some(summary) = &body.summary {
        summary_card(p, fonts, summary);
    }
    if let Some(card) = &body.card {
        current_card(p, fonts, pr, card, body.code_size, editor, opts);
    }
    for other in &body.others {
        other_card(p, fonts, pr, other);
    }
}

fn plural_commits(n: u32) -> String {
    match n {
        0 => String::new(),
        1 => " (1 new commit)".to_string(),
        n => format!(" ({n} new commits)"),
    }
}

fn banner_strip(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, banner: &Banner) {
    let (fill, spin, message, action): (
        Swatch,
        bool,
        String,
        Option<(StoryAction, &str, Variant)>,
    ) = match banner {
        Banner::Writing { harness } => (
            Swatch::Chrome,
            true,
            format!(
                "{harness} is writing the story: names, a summary and the use cases. Meanwhile the steps follow the code, each function after the ones it calls."
            ),
            Some((StoryAction::Stop, "Stop", Variant::Secondary)),
        ),
        Banner::Waiting => (
            Swatch::Chrome,
            true,
            "Waiting for another check to finish…".to_string(),
            Some((StoryAction::Stop, "Stop", Variant::Secondary)),
        ),
        Banner::Failed { message, rewrite } => (
            Swatch::OrangeSoft,
            false,
            format!("The story could not be written: {message}. Showing the static order."),
            Some((
                StoryAction::Retry { rewrite: *rewrite },
                "Retry",
                Variant::Secondary,
            )),
        ),
        Banner::NotWritten => (
            Swatch::AmberSoft,
            false,
            "The story was not written. Showing the static order.".to_string(),
            Some((
                StoryAction::Retry { rewrite: true },
                "Retry",
                Variant::Secondary,
            )),
        ),
        Banner::Outdated {
            written,
            now,
            behind,
        } => (
            Swatch::AmberSoft,
            false,
            format!(
                "Written for commit {written}. The pull request is now at {now}{}; the steps on the left still follow the old commit.",
                plural_commits(*behind)
            ),
            Some((StoryAction::Rewrite, "Rewrite", Variant::Primary)),
        ),
        Banner::Unavailable { harness } => (
            Swatch::Chrome,
            false,
            format!("{harness} is not available. Showing the static order."),
            None,
        ),
        Banner::NoHarness => (
            Swatch::Chrome,
            false,
            "No harness is set up. Showing the static order.".to_string(),
            None,
        ),
        Banner::Manual { harness } => (
            Swatch::Chrome,
            false,
            format!("Write the story with {harness}: one more turn."),
            Some((StoryAction::Write, "Write the story", Variant::Primary)),
        ),
    };
    p.spawn(panel(
        Node {
            width: percent(100),
            flex_shrink: 0.0,
            column_gap: px(10),
            padding: UiRect::axes(px(12), px(8)),
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        fill,
    ))
    .with_children(|b| {
        if spin {
            b.spawn(spinner());
        }
        b.spawn((
            text(fonts, message, Type::BODY),
            Node {
                flex_grow: 1.0,
                flex_basis: px(0),
                min_width: px(0),
                ..default()
            },
        ));
        if let Some((action, label, variant)) = action {
            b.spawn((
                button(fonts, label, variant),
                StoryActionButton {
                    pr: pr.clone(),
                    action,
                },
                observe(on_action),
            ));
        }
    });
}

fn legend_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, l: &Legend) {
    p.spawn(Node {
        width: percent(100),
        flex_shrink: 0.0,
        column_gap: px(14),
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|r| {
        r.spawn(text(
            fonts,
            format!("Changes since version {}", l.since),
            Type::STRONG,
        ));
        for (label, stroke, ink) in [
            ("Added", Swatch::Green, Swatch::Muted),
            ("Changed", Swatch::Amber, Swatch::Muted),
            ("Removed", Swatch::GhostInk, Swatch::GhostInk),
        ] {
            r.spawn(Node {
                align_items: AlignItems::Center,
                column_gap: px(5),
                ..default()
            })
            .with_children(|k| {
                k.spawn((
                    Node {
                        width: px(12),
                        height: px(12),
                        border: px(1).all(),
                        border_radius: BorderRadius::all(px(3)),
                        ..default()
                    },
                    BorderColor::default(),
                    Stroke(stroke),
                ));
                k.spawn(text(fonts, label, Type::META.ink(ink)));
            });
        }
        r.spawn(Node {
            flex_grow: 1.0,
            ..default()
        });
        r.spawn(text(
            fonts,
            format!(
                "{} added · {} changed · {} removed",
                l.added, l.changed, l.removed
            ),
            Type::META,
        ));
    });
}

fn pill(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    label: &str,
    fill: Swatch,
    dot: Option<Swatch>,
) {
    p.spawn((
        Node {
            padding: UiRect::axes(px(9), px(3)),
            column_gap: px(6),
            align_items: AlignItems::Center,
            border_radius: BorderRadius::MAX,
            flex_shrink: 0.0,
            ..default()
        },
        BackgroundColor::default(),
        Fill(fill),
    ))
    .with_children(|c| {
        if let Some(dot) = dot {
            c.spawn((
                Node {
                    width: px(8),
                    height: px(8),
                    border_radius: BorderRadius::MAX,
                    ..default()
                },
                BackgroundColor::default(),
                Fill(dot),
            ));
        }
        c.spawn(text(fonts, label.to_string(), Type::BODY));
    });
}

fn use_case_chips(p: &mut ChildSpawnerCommands, fonts: &UiFonts, chips: &[UseCaseChip]) {
    for chip in chips {
        pill(p, fonts, &chip.name, Swatch::Chrome, Some(chip.swatch));
    }
}

fn chip_row(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    label: &str,
    chips: &[clusia_core::story::Chip],
) {
    p.spawn(Node {
        width: percent(100),
        flex_wrap: FlexWrap::Wrap,
        align_items: AlignItems::Center,
        column_gap: px(6),
        row_gap: px(6),
        ..default()
    })
    .with_children(|r| {
        r.spawn(text(fonts, label, Type::META));
        for (at, chip) in chips.iter().enumerate() {
            if at > 0 {
                r.spawn(text(fonts, "→", Type::META));
            }
            let fill = match chip.tone {
                ChipTone::Good => Swatch::GreenSoft,
                ChipTone::Bad => Swatch::OrangeSoft,
                _ => Swatch::Chrome,
            };
            pill(r, fonts, &chip.text, fill, None);
        }
    });
}

fn summary_card(p: &mut ChildSpawnerCommands, fonts: &UiFonts, s: &SummaryView) {
    p.spawn(card(Node {
        width: percent(100),
        flex_shrink: 0.0,
        flex_direction: FlexDirection::Column,
        row_gap: px(10),
        padding: px(14).all(),
        ..default()
    }))
    .with_children(|c| {
        c.spawn(text(fonts, "In one sentence: ", Type::STRONG))
            .with_children(|t| {
                t.spawn((
                    TextSpan::new(s.sentence.clone()),
                    font(fonts, Type::BODY),
                    TextColor::default(),
                    Ink(Type::BODY.ink),
                ));
            });
        chip_row(c, fonts, "Before", &s.before);
        chip_row(c, fonts, "After", &s.after);
        if !s.use_cases.is_empty() {
            c.spawn(Node {
                width: percent(100),
                flex_wrap: FlexWrap::Wrap,
                align_items: AlignItems::Center,
                column_gap: px(6),
                row_gap: px(6),
                ..default()
            })
            .with_children(|r| {
                r.spawn(text(fonts, "Use cases it touches", Type::META));
                use_case_chips(r, fonts, &s.use_cases);
            });
        }
    });
}

fn current_card(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    c: &CardView,
    size: f32,
    editor: Option<&Editor>,
    opts: &RenderOpts,
) {
    let outline = match c.mark {
        Some(Mark::Changed) => Swatch::Amber,
        _ => Swatch::Green,
    };
    let mut current = p.spawn(card(Node {
        width: percent(100),
        flex_shrink: 0.0,
        flex_direction: FlexDirection::Column,
        row_gap: px(10),
        padding: px(14).all(),
        ..default()
    }));
    current.insert(Stroke(outline));
    current.with_children(|k| {
        k.spawn(Node {
            width: percent(100),
            column_gap: px(8),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|h| {
            h.spawn(text(
                fonts,
                c.number.to_string(),
                Type::STRONG.ink(Swatch::Green),
            ));
            h.spawn(text(
                fonts,
                c.title.clone(),
                if c.mono {
                    Type::MONO.size(Type::HEADING.size)
                } else {
                    Type::HEADING
                },
            ));
            if let Some(mark) = c.mark {
                mark_badge(h, fonts, mark);
            }
            h.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            h.spawn(text(
                fonts,
                c.places.join("  "),
                Type::MONO.size(11.0).ink(Swatch::Muted),
            ));
        });
        let lead = match &c.since {
            Some(_) => (
                format!("Since version {}: ", c.version.saturating_sub(1)),
                Type::STRONG,
            ),
            None => (c.explanation.clone(), Type::BODY),
        };
        k.spawn(text(fonts, lead.0, lead.1)).with_children(|t| {
            if let Some(since) = &c.since {
                t.spawn((
                    TextSpan::new(format!("{since} {}", c.explanation)),
                    font(fonts, Type::BODY),
                    TextColor::default(),
                    Ink(Type::BODY.ink),
                ));
            }
        });
        if !c.use_cases.is_empty() {
            k.spawn(Node {
                width: percent(100),
                flex_wrap: FlexWrap::Wrap,
                column_gap: px(6),
                row_gap: px(6),
                ..default()
            })
            .with_children(|r| use_case_chips(r, fonts, &c.use_cases));
        }
        for block in &c.blocks {
            block_view(k, fonts, pr, block, c.blocks.len() > 1, size, editor, opts);
        }
        for note in &c.notes {
            note_row(k, fonts, note);
        }
        if c.editing
            && let Some(editor) = editor
        {
            editor_box(k, fonts, editor, pr);
        }
        k.spawn(Node {
            width: percent(100),
            column_gap: px(8),
            align_items: AlignItems::Center,
            flex_wrap: FlexWrap::Wrap,
            ..default()
        })
        .with_children(|r| {
            r.spawn((
                button(fonts, "Comment on this step", Variant::Secondary),
                CommentStepButton {
                    pr: pr.clone(),
                    step: c.step.clone(),
                    number: c.version,
                },
                observe(on_comment),
            ));
            let first = c.first_hunk.as_ref();
            r.spawn((
                button(fonts, "Show the full diff", Variant::Ghost),
                FullDiffButton {
                    pr: pr.clone(),
                    path: first.map(|h| h.path.clone()),
                    start: first.map_or(0, |h| h.start_line),
                    end: first.map_or(0, |h| h.end_line),
                },
                observe(on_full_diff),
            ));
            r.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            if c.can_mark {
                let (label, variant) = match (c.read, &c.next) {
                    (true, _) => ("Mark unread".to_string(), Variant::Secondary),
                    (false, Some(next)) => {
                        (format!("Mark read · next: {next} →"), Variant::Primary)
                    }
                    (false, None) => ("Mark read".to_string(), Variant::Primary),
                };
                r.spawn((
                    button(fonts, &label, variant),
                    MarkReadButton {
                        pr: pr.clone(),
                        step: c.step.clone(),
                        read: !c.read,
                        number: c.version,
                        next: c.next_id.clone(),
                    },
                    observe(on_mark),
                ));
            }
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn block_view(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    block: &Block,
    caption: bool,
    size: f32,
    editor: Option<&Editor>,
    opts: &RenderOpts,
) {
    p.spawn(panel(
        Node {
            width: percent(100),
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            padding: px(6).all(),
            border_radius: BorderRadius::all(px(6)),
            overflow: Overflow::clip_x(),
            ..default()
        },
        Swatch::Chrome,
    ))
    .with_children(|b| {
        if caption {
            b.spawn(text(
                fonts,
                block.path.clone(),
                Type::META.ink(Swatch::Muted),
            ));
        }
        unified_rows(
            b,
            fonts,
            pr,
            &block.path,
            &block.lines,
            size,
            editor,
            opts,
            Gutter::Head,
            None,
        );
        if block.hidden > 0 {
            let first = block.lines.iter().find_map(|(l, _)| l.new).unwrap_or(0);
            b.spawn((
                button(
                    fonts,
                    &format!("{} more lines · Show the full diff", block.hidden),
                    Variant::Link,
                ),
                FullDiffButton {
                    pr: pr.clone(),
                    path: Some(block.path.clone()),
                    start: first,
                    end: first,
                },
                observe(on_full_diff),
            ));
        }
    });
}

fn note_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, n: &NoteView) {
    let (label, tone) = match n.kind {
        NoteKind::Draft => ("YOUR DRAFT", Tone::Green),
        NoteKind::Audit => ("AUDIT", Tone::Orange),
        NoteKind::Security => ("SECURITY", Tone::Orange),
    };
    p.spawn(panel(
        Node {
            width: percent(100),
            flex_shrink: 0.0,
            column_gap: px(8),
            padding: UiRect::axes(px(10), px(7)),
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        Swatch::Chrome,
    ))
    .with_children(|r| {
        r.spawn(badge(fonts, label, tone));
        r.spawn((
            text(fonts, n.text.clone(), Type::BODY),
            Node {
                flex_grow: 1.0,
                flex_basis: px(0),
                min_width: px(0),
                ..default()
            },
        ));
        r.spawn(text(fonts, n.who.clone(), Type::META));
    });
}

fn other_card(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, o: &OtherView) {
    let outline = match o.mark {
        Some(Mark::Added) => Some(Swatch::Green),
        Some(Mark::Changed) => Some(Swatch::Amber),
        _ => None,
    };
    let mut e = p.spawn((
        card(Node {
            width: percent(100),
            flex_shrink: 0.0,
            column_gap: px(10),
            padding: UiRect::axes(px(14), px(9)),
            align_items: AlignItems::Center,
            ..default()
        }),
        WidgetButton,
        Hovered::default(),
        Clickable,
        TabIndex(0),
        HoverFill(Swatch::Hover),
        StepButton {
            pr: pr.clone(),
            step: o.step.clone(),
            row: false,
        },
        observe(on_step),
    ));
    if let Some(outline) = outline {
        e.insert(Stroke(outline));
    }
    e.with_children(|r| {
        let ink = if o.read { Swatch::Green } else { Swatch::Muted };
        r.spawn(text(
            fonts,
            o.number.map(|n| n.to_string()).unwrap_or_default(),
            Type::STRONG.ink(ink),
        ));
        r.spawn(text(
            fonts,
            o.title.clone(),
            if o.mono { Type::MONO } else { Type::STRONG },
        ));
        if let Some(mark) = o.mark {
            mark_badge(r, fonts, mark);
        }
        r.spawn((
            text(fonts, o.text.clone(), Type::MUTED),
            Node {
                flex_grow: 1.0,
                flex_basis: px(0),
                min_width: px(0),
                max_height: px(20),
                overflow: Overflow::clip(),
                ..default()
            },
        ));
        use_case_chips(r, fonts, &o.use_cases);
    });
}

// ---- the clicks

fn on_step(activate: On<Activate>, buttons: Query<&StepButton>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(&b.pr) {
        tab.ui.story_step = Some(b.step.clone());
    }
}

#[allow(clippy::too_many_arguments)]
fn on_mark(
    activate: On<Activate>,
    buttons: Query<&MarkReadButton>,
    mut tabs: ResMut<ReviewTabs>,
    mut stories: ResMut<Stories>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, &b.pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    if let Some(m) = stories.0.get_mut(&b.pr) {
        m.mark(&b.step, b.read);
    }
    asks.send(Ask::MarkStepRead {
        pr: b.pr.clone(),
        number: b.number,
        step: b.step.clone(),
        read: b.read,
    });
    if b.read
        && let (Some(next), Some(tab)) = (&b.next, tabs.0.get_mut(&b.pr))
    {
        tab.ui.story_step = Some(next.clone());
    }
}

fn on_comment(
    activate: On<Activate>,
    buttons: Query<&CommentStepButton>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    let Some(tab) = tabs.0.get_mut(&b.pr) else {
        return;
    };
    if unsent(tab.ui.editor.as_ref()) {
        return;
    }
    tab.ui.editor = Some(Editor {
        target: EditTarget::StoryStep {
            number: b.number,
            step: b.step.clone(),
        },
        text: String::new(),
        error: None,
        ticket: None,
        mode: ComposerMode::Write,
    });
}

fn on_full_diff(
    activate: On<Activate>,
    buttons: Query<&FullDiffButton>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    let Some(tab) = tabs.0.get_mut(&b.pr) else {
        return;
    };
    tab.ui.mode = DiffMode::Unified;
    if let Some(path) = &b.path {
        show_line(tab, path, Some((b.start, b.end)));
    }
}

fn on_action(
    activate: On<Activate>,
    buttons: Query<&StoryActionButton>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, &b.pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    let pr = b.pr.clone();
    asks.send(match b.action {
        StoryAction::Stop => Ask::StopStory(pr),
        StoryAction::Rewrite | StoryAction::Retry { rewrite: true } => Ask::RewriteStory(pr),
        StoryAction::Retry { rewrite: false } | StoryAction::Write => Ask::WriteStory(pr),
    });
}

/// Moves to the next older or newer listed version; landing on the latest follows it again.
fn on_stepper(
    activate: On<Activate>,
    steps: Query<&StoryStepper>,
    mut stories: ResMut<Stories>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let Ok(StoryStepper { pr, step }) = steps.get(activate.entity) else {
        return;
    };
    let (Some(m), Some(tab)) = (stories.0.get_mut(pr), tabs.0.get_mut(pr)) else {
        return;
    };
    let Some(now) = tab
        .ui
        .story_version
        .filter(|n| m.versions.iter().any(|v| v.number == *n))
        .or(m.latest_number())
    else {
        return;
    };
    let target = if *step < 0 {
        m.versions.iter().rev().find(|v| v.number < now)
    } else {
        m.versions.iter().find(|v| v.number > now)
    };
    if let Some(target) = target.map(|t| t.number) {
        let latest = m.latest_number() == Some(target);
        tab.ui.story_version = (!latest).then_some(target);
        // Coming back to a version the daemon refused asks for it again.
        m.failed.remove(&target);
    }
}

fn on_compare(
    activate: On<Activate>,
    toggles: Query<&StoryCompareToggle>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let Ok(StoryCompareToggle(pr)) = toggles.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(pr) {
        tab.ui.story_compare = !tab.ui.story_compare;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Connection, StoryTell, Tell};
    use crate::fixture;
    use crate::review_state::EditTarget;
    use crate::screens::review::diff::{DiffRegion, LineButton};
    use crate::screens::review::editor::EditorArea;
    use crate::screens::review::story::{StoryStage, stage};
    use crate::testing::{self, NOW};
    use clusia_core::draft::Side;
    use clusia_protocol::StoryState;

    fn staged(kind: StoryStage) -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        stage(app.world_mut(), &pr, kind, NOW);
        testing::settle(&mut app);
        testing::recorded(&mut app);
        (app, pr)
    }

    fn ui_of(app: &App, pr: &PrRef) -> crate::review_state::TabUi {
        testing::tab(app, pr).ui
    }

    /// The Compare stage shows the comparison; this turns the box off and redraws.
    fn compare_off(app: &mut App, pr: &PrRef) {
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(pr)
            .unwrap()
            .ui
            .story_compare = false;
        testing::settle(app);
    }

    #[test]
    fn the_story_fills_its_two_parts_with_the_heading_the_card_and_the_buttons() {
        let (mut app, pr) = staged(StoryStage::Compare);
        compare_off(&mut app, &pr);
        for needle in [
            "THE CHANGE IN 6 STEPS",
            "1 of 6 read",
            "One refresh at a time",
            "In one sentence:",
            "Use cases it touches",
            "Comment on this step",
            "Show the full diff",
            "Mark read · next: Safer saves →",
            "Version 2 of 2",
            "Show changes since version 1",
        ] {
            assert!(testing::shows(&mut app, needle), "missing {needle:?}");
        }
        assert!(
            !testing::shows(&mut app, "Changes since version 1"),
            "the legend is for the comparison"
        );
    }

    #[test]
    fn a_click_on_a_row_or_a_card_makes_the_step_current() {
        let (mut app, pr) = staged(StoryStage::Written);
        let row = testing::find::<StepButton>(&mut app, |b| b.step == "safer-saves" && b.row);
        testing::activate(&mut app, row);
        testing::settle(&mut app);
        assert_eq!(ui_of(&app, &pr).story_step.as_deref(), Some("safer-saves"));
        assert!(testing::shows(
            &mut app,
            "Mark read · next: Tests for the race →"
        ));
        let card = testing::find::<StepButton>(&mut app, |b| b.step == "race-tests" && !b.row);
        testing::activate(&mut app, card);
        testing::settle(&mut app);
        assert_eq!(ui_of(&app, &pr).story_step.as_deref(), Some("race-tests"));
    }

    #[test]
    fn mark_read_marks_the_step_asks_the_daemon_and_moves_to_the_next() {
        let (mut app, pr) = staged(StoryStage::Written);
        let mark = testing::find::<MarkReadButton>(&mut app, |_| true);
        testing::activate(&mut app, mark);
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::MarkStepRead {
                pr: pr.clone(),
                number: 1,
                step: "one-at-a-time".into(),
                read: true
            }]
        );
        assert!(
            app.world().resource::<Stories>().0[&pr]
                .read
                .contains("one-at-a-time")
        );
        assert_eq!(ui_of(&app, &pr).story_step.as_deref(), Some("safer-saves"));
        assert!(testing::shows(&mut app, "2 of 5 read"));
        // A read step offers to undo it and stays current.
        let row = testing::find::<StepButton>(&mut app, |b| b.step == "refresh-early" && b.row);
        testing::activate(&mut app, row);
        testing::settle(&mut app);
        let unmark = testing::find::<MarkReadButton>(&mut app, |b| !b.read);
        testing::activate(&mut app, unmark);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::MarkStepRead {
                pr: pr.clone(),
                number: 1,
                step: "refresh-early".into(),
                read: false
            }]
        );
        assert_eq!(
            ui_of(&app, &pr).story_step.as_deref(),
            Some("refresh-early")
        );
    }

    #[test]
    fn marks_on_the_static_order_send_version_zero() {
        let (mut app, pr) = staged(StoryStage::Static);
        let mark = testing::find::<MarkReadButton>(&mut app, |_| true);
        testing::activate(&mut app, mark);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::MarkStepRead {
                pr,
                number: 0,
                step: "HttpClient::exchange".into(),
                read: true
            }]
        );
    }

    #[test]
    fn offline_a_mark_says_why_and_changes_nothing() {
        let (mut app, pr) = staged(StoryStage::Written);
        app.world_mut().resource_mut::<Model>().connection = Connection::Lost("gone".into());
        let mark = testing::find::<MarkReadButton>(&mut app, |_| true);
        testing::activate(&mut app, mark);
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty());
        assert!(
            !app.world().resource::<Stories>().0[&pr]
                .read
                .contains("one-at-a-time")
        );
        assert_eq!(
            ui_of(&app, &pr).story_step.as_deref(),
            Some("one-at-a-time")
        );
        let toasts = &app.world().resource::<crate::bridge::Toasts>().0;
        assert!(
            toasts
                .iter()
                .any(|t| t.warning && t.text.contains("Not connected"))
        );
    }

    #[test]
    fn comment_on_this_step_opens_the_composer_in_the_card_unless_text_is_unsent() {
        let (mut app, pr) = staged(StoryStage::Compare);
        let button = testing::find::<CommentStepButton>(&mut app, |_| true);
        testing::activate(&mut app, button);
        testing::settle(&mut app);
        assert_eq!(
            ui_of(&app, &pr).editor.map(|e| e.target),
            Some(EditTarget::StoryStep {
                number: 2,
                step: "one-at-a-time".into()
            })
        );
        assert_eq!(testing::count::<EditorArea>(&mut app), 1);

        let area = testing::find::<EditorArea>(&mut app, |_| true);
        testing::type_into(&mut app, area, "half a thought");
        testing::settle(&mut app);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .story_step = Some("safer-saves".into());
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<EditorArea>(&mut app),
            0,
            "the editor is drawn in its own step's card"
        );
        let button = testing::find::<CommentStepButton>(&mut app, |_| true);
        testing::activate(&mut app, button);
        assert_eq!(
            ui_of(&app, &pr).editor.map(|e| (e.target, e.text)),
            Some((
                EditTarget::StoryStep {
                    number: 2,
                    step: "one-at-a-time".into()
                },
                "half a thought".into()
            )),
            "unsent text stays"
        );
    }

    #[test]
    fn show_the_full_diff_goes_to_unified_at_the_first_hunk_without_storing_the_mode() {
        let (mut app, pr) = staged(StoryStage::Compare);
        let button = testing::find::<FullDiffButton>(&mut app, |_| true);
        testing::activate(&mut app, button);
        testing::settle(&mut app);
        let ui = ui_of(&app, &pr);
        assert_eq!(ui.mode, DiffMode::Unified);
        assert_eq!(ui.file.as_deref(), Some("src/auth/refresh.rs"));
        assert_eq!(ui.focus_line, Some((Side::Right, 43)));
        assert!(
            !testing::recorded(&mut app)
                .iter()
                .any(|a| matches!(a, Ask::SetConfig { .. })),
            "appearance.diff_view is not touched"
        );
        assert_eq!(testing::count::<DiffRegion>(&mut app), 1);
    }

    #[test]
    fn a_line_of_a_steps_hunk_opens_the_composer_like_unified_with_one_number_column() {
        let (mut app, pr) = staged(StoryStage::Compare);
        let line = testing::find::<LineButton>(&mut app, |b| {
            b.path == "src/auth/refresh.rs" && b.line == 44
        });
        let kids = app.world().get::<Children>(line).expect("cells").len();
        assert_eq!(
            kids, 2,
            "a number column and the code: no sign, no second number"
        );
        testing::activate(&mut app, line);
        testing::settle(&mut app);
        let target = ui_of(&app, &pr).editor.map(|e| e.target);
        assert_eq!(
            target,
            Some(EditTarget::Line {
                path: "src/auth/refresh.rs".into(),
                side: Side::Right,
                start: None,
                line: 44
            })
        );
        assert_eq!(
            testing::count::<EditorArea>(&mut app),
            1,
            "under its row, in the step"
        );
    }

    #[test]
    fn the_stepper_asks_an_older_version_and_the_checkbox_turns_the_comparison_on() {
        let (mut app, pr) = staged(StoryStage::Compare);
        let prev = testing::find::<StoryStepper>(&mut app, |s| s.step < 0);
        testing::activate(&mut app, prev);
        testing::settle(&mut app);
        assert_eq!(ui_of(&app, &pr).story_version, Some(1));
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::GetStoryVersion {
                pr: pr.clone(),
                number: 1
            }]
        );
        testing::tell(
            &mut app,
            Tell::Story(StoryTell::Version {
                pr: pr.clone(),
                version: Box::new(fixture::demo_story_version(NOW, 1).expect("v1")),
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "THE CHANGE IN 5 STEPS"));
        assert!(testing::shows(&mut app, "Version 1 of 2"));
        assert!(testing::shows(&mut app, "An older version: read-only."));
        assert_eq!(
            testing::count::<MarkReadButton>(&mut app),
            0,
            "no marks on an older version"
        );
        let next = testing::find::<StoryStepper>(&mut app, |s| s.step > 0);
        testing::activate(&mut app, next);
        testing::settle(&mut app);
        assert_eq!(
            ui_of(&app, &pr).story_version,
            None,
            "landing on the latest follows it again"
        );

        compare_off(&mut app, &pr);
        let toggle = testing::find::<StoryCompareToggle>(&mut app, |_| true);
        testing::activate(&mut app, toggle);
        testing::settle(&mut app);
        assert!(ui_of(&app, &pr).story_compare);
        assert!(testing::shows(&mut app, "Changes since version 1"));
        assert!(testing::shows(&mut app, "1 added · 1 changed · 1 removed"));
        assert!(testing::shows(&mut app, "Retry forever on 5xx"));
    }

    #[test]
    fn the_banner_buttons_send_stop_rewrite_retry_and_write() {
        let click = |app: &mut App, want: StoryAction| {
            // A run that ended makes the daemon's answer stale, so the tab reads it again.
            testing::recorded(app);
            let b = testing::find::<StoryActionButton>(app, |b| b.action == want);
            testing::activate(app, b);
            testing::recorded(app)
        };
        let (mut app, pr) = staged(StoryStage::Static);
        assert!(testing::shows(&mut app, "Claude Code is writing the story"));
        assert_eq!(
            click(&mut app, StoryAction::Stop),
            [Ask::StopStory(pr.clone())]
        );

        let (mut app, pr) = staged(StoryStage::Outdated);
        assert!(testing::shows(&mut app, "Written for commit 3f2a91c."));
        assert!(testing::shows(&mut app, "(2 new commits)"));
        assert_eq!(
            click(&mut app, StoryAction::Rewrite),
            [Ask::RewriteStory(pr.clone())]
        );
        testing::set_config_locally(&mut app, "harness.story", "false");
        testing::settle(&mut app);
        assert_eq!(
            click(&mut app, StoryAction::Rewrite),
            [Ask::RewriteStory(pr.clone())],
            "harness.story gates only the automatic write"
        );

        // A failed run with no version at all starts again with a write; with a version, a rewrite.
        let (mut app, pr) = staged(StoryStage::Static);
        testing::tell(
            &mut app,
            Tell::Story(StoryTell::State {
                pr: pr.clone(),
                state: StoryState::Failed {
                    message: "timed out".into(),
                },
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(
            &mut app,
            "The story could not be written: timed out."
        ));
        assert_eq!(
            click(&mut app, StoryAction::Retry { rewrite: false }),
            [Ask::WriteStory(pr.clone())]
        );

        let (mut app, pr) = staged(StoryStage::Written);
        let mut failed = fixture::demo_story(NOW).first;
        failed.state = clusia_core::story::StoryVersionState::Failed {
            message: "bad json".into(),
        };
        failed.spec = None;
        testing::tell(
            &mut app,
            Tell::Story(StoryTell::Loaded {
                pr: pr.clone(),
                versions: vec![clusia_protocol::StoryVersionInfo {
                    number: 1,
                    commit: failed.commit.clone(),
                    state: failed.state.clone(),
                }],
                latest: Some(Box::new(failed)),
                state: StoryState::Idle,
                static_steps: fixture::demo_story(NOW).static_steps,
                read: Vec::new(),
                head: fixture::demo_story(NOW).head,
                new_commits: 0,
            }),
        );
        testing::settle(&mut app);
        assert_eq!(
            click(&mut app, StoryAction::Retry { rewrite: true }),
            [Ask::RewriteStory(pr.clone())]
        );

        // harness.story off, no version: the view offers the write.
        let (mut app, pr) = staged(StoryStage::Static);
        testing::set_config_locally(&mut app, "harness.program", "/opt/homebrew/bin/claude");
        testing::set_config_locally(&mut app, "harness.story", "false");
        testing::tell(
            &mut app,
            Tell::Story(StoryTell::State {
                pr: pr.clone(),
                state: StoryState::Idle,
            }),
        );
        testing::settle(&mut app);
        assert_eq!(click(&mut app, StoryAction::Write), [Ask::WriteStory(pr)]);
    }

    #[test]
    fn a_banner_button_says_why_when_nothing_can_be_sent() {
        let (mut app, _) = staged(StoryStage::Static);
        app.world_mut().resource_mut::<Model>().connection = Connection::Lost("gone".into());
        let b = testing::find::<StoryActionButton>(&mut app, |b| b.action == StoryAction::Stop);
        testing::activate(&mut app, b);
        assert!(testing::recorded(&mut app).is_empty());
        let toasts = &app.world().resource::<crate::bridge::Toasts>().0;
        assert!(toasts.iter().any(|t| t.warning));
    }

    #[test]
    fn an_added_step_has_the_dashed_ring_and_a_removed_one_a_strikethrough_in_the_comparison() {
        let (mut app, pr) = staged(StoryStage::Compare);
        assert_eq!(
            testing::count::<AddedRing>(&mut app),
            1,
            "the Compare stage has the box checked"
        );
        compare_off(&mut app, &pr);
        assert_eq!(
            testing::count::<AddedRing>(&mut app),
            0,
            "the box is not checked"
        );
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .story_compare = true;
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<AddedRing>(&mut app),
            1,
            "one added step: Retry with a limit"
        );
        let mut struck = app
            .world_mut()
            .query::<(&Text, &bevy::text::Strikethrough)>();
        assert!(
            struck
                .iter(app.world())
                .any(|(t, _)| t.0.contains("Retry forever on 5xx"))
        );
        assert!(
            struck
                .iter(app.world())
                .all(|(t, _)| !t.0.contains("Safer saves")),
            "only the removed step is struck"
        );
    }

    #[test]
    fn the_part_rebuilds_only_when_its_view_changes() {
        let (mut app, pr) = staged(StoryStage::Written);
        let ids = |app: &mut App| {
            let mut q = app.world_mut().query::<(Entity, &StepButton)>();
            let mut v: Vec<Entity> = q.iter(app.world()).map(|(e, _)| e).collect();
            v.sort();
            v
        };
        let before = ids(&mut app);
        assert!(!before.is_empty());
        testing::settle(&mut app);
        assert_eq!(ids(&mut app), before, "nothing changed: nothing rebuilt");

        // Typing is in no view: it touches the tab but rebuilds nothing.
        let comment = testing::find::<CommentStepButton>(&mut app, |_| true);
        testing::activate(&mut app, comment);
        testing::settle(&mut app);
        let with_editor = ids(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        testing::type_into(&mut app, area, "a thought");
        testing::settle(&mut app);
        assert_eq!(ids(&mut app), with_editor, "typing rebuilds nothing");
        assert!(
            ui_of(&app, &pr)
                .editor
                .is_some_and(|e| e.text == "a thought")
        );
    }

    #[test]
    fn switching_the_theme_while_comparing_keeps_the_comparison() {
        let (mut app, pr) = staged(StoryStage::Compare);
        for dark in [true, false, true] {
            app.world_mut().insert_resource(crate::theme::Theme::new(
                dark,
                13,
                clusia_core::Density::Comfortable,
            ));
            testing::settle(&mut app);
            assert!(ui_of(&app, &pr).story_compare, "dark={dark}");
            assert!(
                testing::shows(&mut app, "Changes since version 1"),
                "dark={dark}"
            );
            assert!(
                testing::shows(&mut app, "Retry forever on 5xx"),
                "dark={dark}"
            );
            assert_eq!(testing::count::<AddedRing>(&mut app), 1, "dark={dark}");
        }
    }

    #[test]
    fn a_refused_step_comment_keeps_its_text_and_says_why() {
        let (mut app, pr) = staged(StoryStage::Compare);
        let button = testing::find::<CommentStepButton>(&mut app, |_| true);
        testing::activate(&mut app, button);
        testing::settle(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        testing::type_into(&mut app, area, "Why a lock here?");
        testing::settle(&mut app);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor
            .as_mut()
            .unwrap()
            .ticket = Some(7);
        testing::tell(
            &mut app,
            Tell::Refused {
                pr: pr.clone(),
                ticket: 7,
                message: "The step is gone".into(),
            },
        );
        testing::settle(&mut app);
        let editor = ui_of(&app, &pr).editor.expect("the editor stays");
        assert_eq!(
            (editor.text.as_str(), editor.ticket, editor.error.as_deref()),
            ("Why a lock here?", None, Some("The step is gone"))
        );
        assert!(testing::shows(&mut app, "The step is gone"));
    }

    #[test]
    fn the_scene_stages_draw_without_a_panic_in_dark_and_light() {
        for kind in [
            StoryStage::Written,
            StoryStage::Static,
            StoryStage::Outdated,
            StoryStage::Compare,
        ] {
            for dark in [false, true] {
                let (mut app, _) = staged(kind);
                app.world_mut().insert_resource(crate::theme::Theme::new(
                    dark,
                    13,
                    clusia_core::Density::Comfortable,
                ));
                testing::settle(&mut app);
                assert!(
                    testing::shows(&mut app, "THE CHANGE IN"),
                    "{kind:?} dark={dark}"
                );
            }
        }
    }
}
