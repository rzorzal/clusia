//! The left side of the Diagrams tab (mockup `Diagrams.png`): the list of the version's
//! diagrams, the footer with the version stepper, and the whole-tab notices. The view is a pure
//! struct of already printable text, compared with what was built so the tab redraws only when
//! it changes. The right side is `Pane`: the canvas of the selected diagram, or what that entry
//! says instead.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::{ButtonInput, InputSystems};
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::Pickable;
use bevy::picking::events::{Pointer, Press};
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui::{ComputedNode, UiGlobalTransform};
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use bevy::window::PrimaryWindow;

use clusia_core::PrRef;
use clusia_core::diagram::{EntryInfo, EntryStateName};
use clusia_core::printable::printable;
use clusia_protocol::DiagramsState;

use super::DiagramCanvas;
use super::model::{Diagrams, DiagramsModel, selected_key};
use crate::bridge::{Ask, Asks, Model, Toasts};
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen};
use crate::review_state::{Phase, ReviewSection, ReviewTabs};
use crate::screens::review::agent::harness_ready;
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::screens::review::shell::{SectionBody, on_harness};
use crate::theme::Swatch;
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Stroke, Type, Variant, button, checkbox, disabled_button, panel,
    text,
};

/// The longest reason or failure the tab prints.
const REASON_MAX: usize = 160;

/// What a button of the tab does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// **Draw diagrams**.
    Draw,
    /// **Redraw for the new commit**.
    DrawAgain,
    /// **Try again** after a failed plan.
    TryAgain,
    Stop,
    /// **Retry** one entry.
    Retry(String),
}

impl Action {
    fn label(&self) -> &'static str {
        match self {
            Action::Draw => "Draw diagrams",
            Action::DrawAgain => "Redraw for the new commit",
            Action::TryAgain => "Try again",
            Action::Stop => "Stop",
            Action::Retry(_) => "Retry",
        }
    }

    fn variant(&self) -> Variant {
        match self {
            Action::Draw | Action::Retry(_) => Variant::Primary,
            _ => Variant::Secondary,
        }
    }
}

/// What the whole tab says when there is nothing to list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub line: String,
    pub action: Option<Action>,
    /// Offer **Set up a harness**.
    pub setup: bool,
}

/// An entry's state as the row shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Ready,
    Waiting,
    Drawing,
    Failed,
    NotDrawn,
}

/// The ⋯ menu of the selected row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuView {
    pub open: bool,
    /// *Redraw*: the latest version's ready entry, while nothing runs.
    pub redraw: bool,
    /// `3 items dropped`, shown as a disabled line.
    pub dropped: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowView {
    pub key: String,
    pub name: String,
    /// The subtitle: `Flow · 5 boxes`, `drawing…`, `could not draw · Retry`.
    pub meta: String,
    pub state: RowState,
    pub selected: bool,
    pub menu: Option<MenuView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stepper {
    pub number: u32,
    /// The latest version's number.
    pub of: u32,
    pub prev: bool,
    pub next: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compare {
    /// The version the changes are shown against.
    pub since: u32,
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterView {
    /// `Drawn by Claude Code from commit 9be1d07. …`
    pub text: String,
    /// The version shown is not the latest: it is read-only.
    pub old: bool,
    /// What a run in progress is doing; **Stop** goes with it.
    pub working: Option<String>,
    /// How the last run ended, when it failed and older versions remain.
    pub failure: Option<String>,
    pub stepper: Option<Stepper>,
    pub compare: Option<Compare>,
    /// **Redraw for the new commit**, with why the drawings are out of date.
    pub redraw: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListView {
    /// `DIAGRAMS · 8`
    pub heading: String,
    pub rows: Vec<RowView>,
    pub footer: FooterView,
}

/// The right side of the tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pane {
    /// The whole tab has one message and no list.
    Tab(Notice),
    /// The selected version has not arrived.
    Loading,
    /// The selected diagram, ready: `DiagramCanvas` draws it.
    Canvas {
        number: u32,
        key: String,
        compare: bool,
    },
    /// The selected entry has no drawing: why, and **Retry** on the latest version.
    Entry {
        title: String,
        line: String,
        retry: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramsView {
    pub list: Option<ListView>,
    pub pane: Pane,
}

/// What the view is drawn from.
pub struct Inputs<'a> {
    pub model: Option<&'a DiagramsModel>,
    pub harness: bool,
    pub agent: &'a str,
    /// The tab is a cached copy.
    pub offline: bool,
    pub selected: Option<&'a str>,
    pub version: Option<u32>,
    pub compare: bool,
    pub menu: Option<&'a str>,
    /// The review's head as the tab knows it, which a push moves before the daemon is asked again.
    pub head: &'a str,
}

fn plural(n: u32, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

fn cut(s: &str, max: usize) -> String {
    let s = printable(s);
    if s.chars().count() <= max {
        return s;
    }
    let mut short: String = s.chars().take(max).collect();
    short.push('…');
    short
}

fn short_sha(commit: &str) -> String {
    commit.chars().take(7).collect()
}

fn row_state(state: EntryStateName) -> RowState {
    match state {
        EntryStateName::Ready => RowState::Ready,
        EntryStateName::Waiting => RowState::Waiting,
        EntryStateName::Drawing => RowState::Drawing,
        EntryStateName::Failed => RowState::Failed,
        EntryStateName::NotDrawn => RowState::NotDrawn,
    }
}

fn meta_of(e: &EntryInfo, latest: bool) -> String {
    let retry = if latest { " · Retry" } else { "" };
    match e.state {
        EntryStateName::Ready => e.kind.list_meta(e.count),
        EntryStateName::Waiting => "waiting".into(),
        EntryStateName::Drawing => "drawing…".into(),
        EntryStateName::Failed => format!("could not draw{retry}"),
        EntryStateName::NotDrawn => format!("not drawn{retry}"),
    }
}

/// What the ⋯ menu of `e` offers; `None` when it offers nothing.
pub fn menu_of(e: &EntryInfo, latest: bool, busy: bool, open: bool) -> Option<MenuView> {
    let redraw = latest && !busy && e.state == EntryStateName::Ready;
    let dropped = (e.notes > 0).then(|| plural(e.notes, "item dropped", "items dropped"));
    (redraw || dropped.is_some()).then_some(MenuView {
        open,
        redraw,
        dropped,
    })
}

fn tab_notice(notice: Notice) -> DiagramsView {
    DiagramsView {
        list: None,
        pane: Pane::Tab(notice),
    }
}

/// What the Diagrams tab shows.
pub fn diagrams_view(i: &Inputs) -> DiagramsView {
    let default = DiagramsModel::default();
    let m = i.model.unwrap_or(&default);
    let notice = |title: &str, line: String, action: Option<Action>, setup: bool| Notice {
        title: title.into(),
        line,
        action,
        setup,
    };
    if !m.received {
        let line = if i.offline {
            "Diagrams are not available offline."
        } else {
            "Loading diagrams…"
        };
        return tab_notice(notice("Diagrams", line.into(), None, false));
    }
    let Some((info, latest)) = m.shown(i.version) else {
        return tab_notice(match &m.state {
            DiagramsState::Planning | DiagramsState::Drawing { .. } => notice(
                "Choosing which diagrams explain this change…",
                format!("{} is reading the change.", i.agent),
                Some(Action::Stop),
                false,
            ),
            DiagramsState::Failed { message } => notice(
                "The diagrams could not be drawn",
                cut(message, REASON_MAX),
                Some(Action::TryAgain),
                false,
            ),
            DiagramsState::Idle if !i.harness => notice(
                "No diagrams yet",
                "Set up a harness in Config › Harness".into(),
                None,
                true,
            ),
            DiagramsState::Idle => notice(
                "No diagrams yet",
                format!("Each diagram is one turn on your {} account.", i.agent),
                Some(Action::Draw),
                false,
            ),
        });
    };
    let busy = m.busy();
    let key = selected_key(info, i.selected);
    let rows: Vec<RowView> = info
        .entries
        .iter()
        .map(|e| {
            let selected = key.as_deref() == Some(e.key.as_str());
            RowView {
                key: e.key.clone(),
                name: printable(&e.title),
                meta: meta_of(e, latest),
                state: row_state(e.state),
                selected,
                menu: if selected {
                    menu_of(e, latest, busy, i.menu == Some(e.key.as_str()))
                } else {
                    None
                },
            }
        })
        .collect();
    let since = key.as_deref().and_then(|k| m.compare_with(info.number, k));
    let compare = i.compare && since.is_some();
    let first = m.versions.first().map_or(info.number, |v| v.number);
    let behind = [m.head.as_str(), i.head]
        .iter()
        .any(|h| !h.is_empty() && *h != info.commit);
    let footer = FooterView {
        text: format!(
            "Drawn by {} from commit {}. When the PR changes, ask for new diagrams; the old ones stay with their commit.",
            printable(&info.harness),
            short_sha(&info.commit)
        ),
        old: !latest,
        working: match &m.state {
            DiagramsState::Planning => {
                Some("Choosing which diagrams explain this change…".to_string())
            }
            DiagramsState::Drawing { done, total, .. } => {
                Some(format!("Drawing diagram {} of {total}…", done + 1))
            }
            _ => None,
        },
        failure: match &m.state {
            DiagramsState::Failed { message } => Some(format!(
                "The last drawing failed: {}",
                cut(message, REASON_MAX)
            )),
            _ => None,
        },
        stepper: (m.versions.len() > 1).then(|| Stepper {
            number: info.number,
            of: m.latest_number().unwrap_or(info.number),
            prev: info.number > first,
            next: !latest,
        }),
        compare: since.map(|since| Compare {
            since,
            on: i.compare,
        }),
        redraw: (latest && !busy && i.harness && (m.stale || behind)).then(|| {
            m.stale_reason
                .as_deref()
                .map(|r| cut(r, REASON_MAX))
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "The pull request has new commits.".to_string())
        }),
    };
    let entry = key
        .as_deref()
        .and_then(|k| info.entries.iter().find(|e| e.key == k));
    let retry = |key: &str| (latest && !busy && i.harness).then(|| key.to_string());
    let pane = match entry {
        None => Pane::Entry {
            title: "No diagram".into(),
            line: "This version has no diagrams.".into(),
            retry: None,
        },
        Some(e) => match e.state {
            EntryStateName::Ready if m.loaded.contains_key(&info.number) => Pane::Canvas {
                number: info.number,
                key: e.key.clone(),
                compare,
            },
            EntryStateName::Ready => Pane::Loading,
            EntryStateName::Waiting => Pane::Entry {
                title: printable(&e.title),
                line: "Waiting for its turn.".into(),
                retry: None,
            },
            EntryStateName::Drawing => Pane::Entry {
                title: printable(&e.title),
                line: format!("{} is drawing this diagram.", i.agent),
                retry: None,
            },
            EntryStateName::Failed => Pane::Entry {
                title: printable(&e.title),
                line: match e.reason.as_deref().map(|r| cut(r, REASON_MAX)) {
                    Some(reason) if !reason.is_empty() => format!("Could not draw: {reason}"),
                    _ => "Could not draw this diagram.".into(),
                },
                retry: retry(&e.key),
            },
            EntryStateName::NotDrawn => Pane::Entry {
                title: printable(&e.title),
                line: "Drawing stopped before this diagram. Retry to draw it.".into(),
                retry: retry(&e.key),
            },
        },
    };
    DiagramsView {
        list: Some(ListView {
            heading: format!("DIAGRAMS · {}", rows.len()),
            rows,
            footer,
        }),
        pane,
    }
}

// ---- components

/// A row of the list.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DiagramRowButton {
    pub pr: PrRef,
    pub key: String,
}

/// The ⋯ button of the selected row.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct MenuButton {
    pub pr: PrRef,
    pub key: String,
}

/// A button that starts, stops or retries a drawing.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DiagramButton {
    pub pr: PrRef,
    pub action: Action,
}

/// The ◀ and ▶ of the version stepper (`-1` older, `1` newer).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct VersionStep {
    pub pr: PrRef,
    pub step: i32,
}

/// The *Show changes since version n* checkbox.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CompareToggle(pub PrRef);

/// *Redraw* in the ⋯ menu.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RedrawItem {
    pub pr: PrRef,
    pub key: String,
}

/// The scrolling list of rows.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DiagramList(pub PrRef);

/// The left column: rebuilt when its `ListView` changes.
#[derive(Component, Debug)]
pub struct ListRegion {
    pub pr: PrRef,
    built: Option<Option<ListView>>,
}

/// The right side: rebuilt when its `Pane` changes.
#[derive(Component, Debug)]
pub struct PaneRegion {
    pub pr: PrRef,
    built: Option<Pane>,
}

/// The tab's content inside `SectionBody`.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DiagramsRoot(pub PrRef);

/// The window-sized layer of an open ⋯ menu.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct MenuLayer {
    pub pr: PrRef,
    pub key: String,
    pub menu: MenuView,
}

/// The card of the open menu.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct MenuCard {
    pub pr: PrRef,
    pub key: String,
}

const LIST_WIDTH: f32 = 236.0;
const MENU_WIDTH: f32 = 184.0;
const MENU_Z: i32 = 60;

fn list_node(shown: bool) -> Node {
    Node {
        width: px(LIST_WIDTH),
        flex_shrink: 0.0,
        min_height: px(0),
        flex_direction: FlexDirection::Column,
        row_gap: px(2),
        padding: UiRect::axes(px(10), px(12)),
        border: UiRect::right(px(1)),
        display: if shown { Display::Flex } else { Display::None },
        ..default()
    }
}

fn pane_node() -> Node {
    Node {
        flex_grow: 1.0,
        min_width: px(0),
        min_height: px(0),
        flex_direction: FlexDirection::Column,
        ..default()
    }
}

/// Keeps `SectionBody` filled with the Diagrams tab while that section is selected. The other
/// sections' fillers clear it when they take over.
pub(super) fn fill_diagrams(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    diagrams: Res<Diagrams>,
    model: Res<Model>,
    fonts: Res<UiFonts>,
    bodies: Query<(Entity, &SectionBody, Option<&Children>)>,
    roots: Query<(Entity, &DiagramsRoot, Option<&Children>)>,
    mut lists: Query<(Entity, &mut ListRegion)>,
    mut panes: Query<(Entity, &mut PaneRegion)>,
    scrolled: Query<(&DiagramList, &ScrollPosition)>,
) {
    for (entity, body, children) in &bodies {
        let Some(tab) = tabs.0.get(&body.pr) else {
            continue;
        };
        let Phase::Ready(ready) = &tab.phase else {
            continue;
        };
        if tab.ui.section != ReviewSection::Diagrams {
            continue;
        }
        let existing = children
            .into_iter()
            .flatten()
            .copied()
            .find_map(|c| roots.get(c).ok());
        let Some((_, _, root_children)) = existing else {
            commands.entity(entity).despawn_related::<Children>();
            let pr = body.pr.clone();
            commands.entity(entity).with_children(|p| {
                p.spawn((
                    Node {
                        flex_grow: 1.0,
                        width: percent(100),
                        min_height: px(0),
                        ..default()
                    },
                    DiagramsRoot(pr.clone()),
                ))
                .with_children(|r| {
                    r.spawn((
                        list_node(false),
                        BorderColor::default(),
                        Stroke(Swatch::Line),
                        ListRegion {
                            pr: pr.clone(),
                            built: None,
                        },
                    ));
                    r.spawn((pane_node(), PaneRegion { pr, built: None }));
                });
            });
            continue;
        };
        let view = diagrams_view(&Inputs {
            model: diagrams.0.get(&body.pr),
            harness: harness_ready(
                &model.snapshot,
                crate::agent_name::kind_of_tab(&model.snapshot, tab),
            ),
            agent: &crate::agent_name::of_tab(&model.snapshot, tab),
            offline: ready.cached_at.is_some(),
            selected: tab.ui.diagram.as_deref(),
            version: tab.ui.diagram_version,
            compare: tab.ui.diagram_compare,
            menu: tab.ui.diagram_menu.as_deref(),
            head: &ready.view.review.head_sha,
        });
        let pr = body.pr.clone();
        for child in root_children.into_iter().flatten().copied() {
            if let Ok((region, mut list)) = lists.get_mut(child)
                && list.built.as_ref() != Some(&view.list)
            {
                let scroll = scrolled
                    .iter()
                    .find(|(l, _)| l.0 == pr)
                    .map(|(_, at)| at.clone())
                    .unwrap_or_default();
                commands.entity(region).despawn_related::<Children>();
                commands
                    .entity(region)
                    .insert(list_node(view.list.is_some()));
                if let Some(v) = &view.list {
                    commands.entity(region).with_children(|p| {
                        draw_list(p, &fonts, &pr, v, scroll);
                    });
                }
                list.built = Some(view.list.clone());
            }
            if let Ok((region, mut pane)) = panes.get_mut(child)
                && pane.built.as_ref() != Some(&view.pane)
            {
                commands.entity(region).despawn_related::<Children>();
                commands.entity(region).with_children(|p| {
                    draw_pane(p, &fonts, &pr, &view.pane);
                });
                pane.built = Some(view.pane.clone());
            }
        }
    }
}

fn draw_list(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    v: &ListView,
    scroll: ScrollPosition,
) {
    p.spawn(Node {
        padding: UiRect::new(px(8), px(8), px(0), px(6)),
        ..default()
    })
    .with_children(|h| {
        h.spawn(text(fonts, v.heading.clone(), Type::META.size(11.0)));
    });
    p.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            flex_grow: 1.0,
            min_height: px(0),
            overflow: Overflow::scroll_y(),
            ..default()
        },
        ScrollArea,
        scroll,
        DiagramList(pr.clone()),
    ))
    .with_children(|list| {
        for row in &v.rows {
            draw_row(list, fonts, pr, row);
        }
    });
    draw_footer(p, fonts, pr, &v.footer);
}

fn draw_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, row: &RowView) {
    p.spawn((
        Node {
            align_items: AlignItems::Center,
            flex_shrink: 0.0,
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        BackgroundColor::default(),
        Fill(if row.selected {
            Swatch::Selected
        } else {
            Swatch::Clear
        }),
    ))
    .with_children(|r| {
        r.spawn((
            Node {
                flex_grow: 1.0,
                min_width: px(0),
                flex_direction: FlexDirection::Column,
                row_gap: px(1),
                padding: UiRect::axes(px(10), px(7)),
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            WidgetButton,
            Clickable,
            Hovered::default(),
            TabIndex(0),
            BackgroundColor::default(),
            Fill(Swatch::Clear),
            HoverFill(Swatch::Hover),
            DiagramRowButton {
                pr: pr.clone(),
                key: row.key.clone(),
            },
            observe(on_row),
        ))
        .with_children(|c| {
            let name = if row.selected {
                Type::STRONG
            } else {
                Type::MUTED
            };
            c.spawn(text(fonts, row.name.clone(), name));
            let meta = match row.state {
                RowState::Failed | RowState::NotDrawn => Type::META.size(11.0).ink(Swatch::Orange),
                _ => Type::META.size(11.0),
            };
            c.spawn(text(fonts, row.meta.clone(), meta));
        });
        if row.menu.is_some() {
            r.spawn((
                Node {
                    width: px(26),
                    height: px(26),
                    margin: UiRect::right(px(4)),
                    flex_shrink: 0.0,
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    border_radius: BorderRadius::all(px(6)),
                    ..default()
                },
                WidgetButton,
                Clickable,
                Hovered::default(),
                TabIndex(0),
                BackgroundColor::default(),
                Fill(Swatch::Clear),
                HoverFill(Swatch::Hover),
                MenuButton {
                    pr: pr.clone(),
                    key: row.key.clone(),
                },
                observe(on_menu_button),
            ))
            .with_children(|b| {
                // Inter has no ⋯: the mono face does.
                b.spawn(text(fonts, "⋯", Type::MONO));
            });
        }
    });
}

fn draw_footer(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, f: &FooterView) {
    p.spawn(panel(
        Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(8),
            padding: px(10).all(),
            flex_shrink: 0.0,
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        Swatch::Chrome,
    ))
    .with_children(|c| {
        c.spawn(text(fonts, f.text.clone(), Type::META.ink(Swatch::Muted)));
        if f.old {
            c.spawn(text(fonts, "An older version: read-only.", Type::META));
        }
        if let Some(failure) = &f.failure {
            c.spawn(text(fonts, failure.clone(), Type::META.ink(Swatch::Orange)));
        }
        if let Some(step) = &f.stepper {
            c.spawn(Node {
                column_gap: px(8),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|row| {
                step_button(row, fonts, pr, "◀", -1, step.prev);
                row.spawn(text(
                    fonts,
                    format!("Version {} of {}", step.number, step.of),
                    Type::MUTED,
                ));
                step_button(row, fonts, pr, "▶", 1, step.next);
            });
        }
        if let Some(compare) = &f.compare {
            c.spawn(Node {
                column_gap: px(10),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|row| {
                row.spawn((
                    checkbox(compare.on),
                    CompareToggle(pr.clone()),
                    observe(on_compare),
                ));
                row.spawn(text(
                    fonts,
                    format!("Show changes since version {}", compare.since),
                    Type::MUTED,
                ));
            });
        }
        if let Some(working) = &f.working {
            c.spawn(text(fonts, working.clone(), Type::META.ink(Swatch::Muted)));
            action_button(c, fonts, pr, Action::Stop);
        }
        if let Some(why) = &f.redraw {
            c.spawn(text(fonts, why.clone(), Type::META.ink(Swatch::Muted)));
            action_button(c, fonts, pr, Action::DrawAgain);
        }
    });
}

fn step_button(
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
            VersionStep {
                pr: pr.clone(),
                step,
            },
            observe(on_step),
        ));
    } else {
        p.spawn(disabled_button(fonts, label));
    }
}

fn action_button(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, action: Action) {
    p.spawn((
        button(fonts, action.label(), action.variant()),
        DiagramButton {
            pr: pr.clone(),
            action,
        },
        observe(on_button),
    ));
}

fn draw_pane(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, pane: &Pane) {
    match pane {
        Pane::Canvas { .. } => {
            p.spawn((
                Node {
                    flex_grow: 1.0,
                    min_width: px(0),
                    min_height: px(0),
                    overflow: Overflow::clip(),
                    ..default()
                },
                BackgroundColor::default(),
                Fill(Swatch::Surface),
                DiagramCanvas(pr.clone()),
            ));
        }
        Pane::Loading => {
            message(
                p,
                fonts,
                pr,
                "Diagrams",
                "Loading this version…",
                None,
                false,
            );
        }
        Pane::Tab(n) => message(p, fonts, pr, &n.title, &n.line, n.action.clone(), n.setup),
        Pane::Entry { title, line, retry } => {
            message(
                p,
                fonts,
                pr,
                title,
                line,
                retry.clone().map(Action::Retry),
                false,
            );
        }
    }
}

fn message(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    title: &str,
    line: &str,
    action: Option<Action>,
    setup: bool,
) {
    p.spawn(Node {
        flex_grow: 1.0,
        min_width: px(0),
        column_gap: px(12),
        align_items: AlignItems::Start,
        padding: UiRect::axes(px(28), px(20)),
        ..default()
    })
    .with_children(|h| {
        h.spawn(Node {
            flex_grow: 1.0,
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(4),
            ..default()
        })
        .with_children(|t| {
            t.spawn(text(fonts, title.to_string(), Type::HEADING));
            t.spawn(text(fonts, line.to_string(), Type::MUTED));
        });
        if let Some(action) = action {
            action_button(h, fonts, pr, action);
        }
        if setup {
            h.spawn((
                button(fonts, "Set up a harness", Variant::Secondary),
                observe(on_harness),
            ));
        }
    });
}

// ---- observers

fn on_row(activate: On<Activate>, rows: Query<&DiagramRowButton>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(DiagramRowButton { pr, key }) = rows.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(pr) {
        tab.ui.diagram = Some(key.clone());
        tab.ui.diagram_menu = None;
    }
}

fn on_menu_button(
    activate: On<Activate>,
    buttons: Query<&MenuButton>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let Ok(MenuButton { pr, key }) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(pr) {
        tab.ui.diagram_menu = if tab.ui.diagram_menu.as_deref() == Some(key) {
            None
        } else {
            Some(key.clone())
        };
    }
}

fn on_compare(
    activate: On<Activate>,
    toggles: Query<&CompareToggle>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let Ok(CompareToggle(pr)) = toggles.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(pr) {
        tab.ui.diagram_compare = !tab.ui.diagram_compare;
    }
}

/// Moves to the next older or newer version; landing on the latest follows it again.
fn on_step(
    activate: On<Activate>,
    steps: Query<&VersionStep>,
    diagrams: Res<Diagrams>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let Ok(VersionStep { pr, step }) = steps.get(activate.entity) else {
        return;
    };
    let (Some(m), Some(tab)) = (diagrams.0.get(pr), tabs.0.get_mut(pr)) else {
        return;
    };
    let Some((info, _)) = m.shown(tab.ui.diagram_version) else {
        return;
    };
    let now = info.number;
    let target = if *step < 0 {
        m.versions.iter().rev().find(|v| v.number < now)
    } else {
        m.versions.iter().find(|v| v.number > now)
    };
    if let Some(target) = target {
        let latest = m.latest_number() == Some(target.number);
        tab.ui.diagram_version = (!latest).then_some(target.number);
        tab.ui.diagram_menu = None;
    }
}

fn on_button(
    activate: On<Activate>,
    buttons: Query<&DiagramButton>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(DiagramButton { pr, action }) = buttons.get(activate.entity) else {
        return;
    };
    if *action == Action::Stop {
        asks.send(Ask::StopDiagrams(pr.clone()));
        return;
    }
    if let Some(reason) = read_only_reason(&tabs, &model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    asks.send(match action {
        Action::Retry(key) => Ask::DrawDiagram {
            pr: pr.clone(),
            key: key.clone(),
        },
        _ => Ask::DrawDiagrams(pr.clone()),
    });
}

fn on_redraw_item(
    activate: On<Activate>,
    items: Query<&RedrawItem>,
    mut tabs: ResMut<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(RedrawItem { pr, key }) = items.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    if let Some(tab) = tabs.0.get_mut(pr) {
        tab.ui.diagram_menu = None;
    }
    asks.send(Ask::DrawDiagram {
        pr: pr.clone(),
        key: key.clone(),
    });
}

// ---- the ⋯ menu

fn close_on_escape(mut keys: ResMut<ButtonInput<KeyCode>>, mut tabs: ResMut<ReviewTabs>) {
    if !keys.just_pressed(KeyCode::Escape) || !tabs.0.values().any(|t| t.ui.diagram_menu.is_some())
    {
        return;
    }
    keys.clear_just_pressed(KeyCode::Escape);
    for tab in tabs.0.values_mut() {
        tab.ui.diagram_menu = None;
    }
}

fn on_backdrop(
    _press: On<Pointer<Press>>,
    layers: Query<&MenuLayer>,
    mut tabs: ResMut<ReviewTabs>,
) {
    for layer in &layers {
        if let Some(tab) = tabs.0.get_mut(&layer.pr) {
            tab.ui.diagram_menu = None;
        }
    }
}

/// Keeps exactly the menu a tab asks for on screen, as a layer of its own at the root of the
/// window so no scroll area clips it.
pub(super) fn sync_menu(
    mut commands: Commands,
    nav: Res<Nav>,
    tabs: Res<ReviewTabs>,
    diagrams: Res<Diagrams>,
    fonts: Res<UiFonts>,
    layers: Query<(Entity, &MenuLayer)>,
) {
    let wanted = tabs.0.iter().find_map(|(pr, tab)| {
        let key = tab.ui.diagram_menu.clone()?;
        if tab.ui.section != ReviewSection::Diagrams || nav.screen != Screen::Review(pr.clone()) {
            return None;
        }
        let m = diagrams.0.get(pr)?;
        let (info, latest) = m.shown(tab.ui.diagram_version)?;
        let entry = info.entries.iter().find(|e| e.key == key)?;
        let menu = menu_of(entry, latest, m.busy(), true)?;
        Some((pr.clone(), key, menu))
    });
    let mut present = false;
    for (entity, layer) in &layers {
        let same = wanted.as_ref().is_some_and(|(pr, key, menu)| {
            layer.pr == *pr && layer.key == *key && layer.menu == *menu
        });
        if same {
            present = true;
        } else {
            commands.entity(entity).despawn();
        }
    }
    let Some((pr, key, menu)) = wanted else {
        return;
    };
    if present {
        return;
    }
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                width: percent(100),
                height: percent(100),
                ..default()
            },
            GlobalZIndex(MENU_Z),
            Pickable {
                should_block_lower: true,
                is_hoverable: true,
            },
            MenuLayer {
                pr: pr.clone(),
                key: key.clone(),
                menu: menu.clone(),
            },
            observe(on_backdrop),
        ))
        .with_children(|layer| {
            layer
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(0),
                        top: px(0),
                        width: px(MENU_WIDTH),
                        flex_direction: FlexDirection::Column,
                        row_gap: px(2),
                        padding: px(6).all(),
                        border: px(1).all(),
                        border_radius: BorderRadius::all(px(8)),
                        ..default()
                    },
                    BackgroundColor::default(),
                    Fill(Swatch::Surface),
                    BorderColor::default(),
                    Stroke(Swatch::Line),
                    // Shown by `place_menu` once it knows where the card goes.
                    Visibility::Hidden,
                    Pickable {
                        should_block_lower: true,
                        is_hoverable: true,
                    },
                    MenuCard {
                        pr: pr.clone(),
                        key: key.clone(),
                    },
                    observe(|mut press: On<Pointer<Press>>| press.propagate(false)),
                ))
                .with_children(|card| {
                    if menu.redraw {
                        card.spawn((
                            Node {
                                height: px(30),
                                padding: UiRect::horizontal(px(10)),
                                align_items: AlignItems::Center,
                                border_radius: BorderRadius::all(px(6)),
                                ..default()
                            },
                            WidgetButton,
                            Clickable,
                            Hovered::default(),
                            TabIndex(0),
                            BackgroundColor::default(),
                            Fill(Swatch::Clear),
                            HoverFill(Swatch::Hover),
                            RedrawItem {
                                pr: pr.clone(),
                                key: key.clone(),
                            },
                            observe(on_redraw_item),
                        ))
                        .with_children(|i| {
                            i.spawn(text(&fonts, "Redraw", Type::BODY));
                        });
                    }
                    if let Some(dropped) = &menu.dropped {
                        card.spawn(Node {
                            height: px(30),
                            padding: UiRect::horizontal(px(10)),
                            align_items: AlignItems::Center,
                            ..default()
                        })
                        .with_children(|i| {
                            i.spawn(text(
                                &fonts,
                                dropped.clone(),
                                Type::MUTED.ink(Swatch::Faint),
                            ));
                        });
                    }
                });
        });
}

/// Where a menu card of `size` goes for a ⋯ button at `button`, in a window of `window`: under
/// the button with the card's right edge on the button's, above it when there is no room below,
/// and always inside the window.
pub fn menu_spot(button: Rect, size: Vec2, window: Vec2) -> Vec2 {
    const GAP: f32 = 4.0;
    const EDGE: f32 = 8.0;
    let left = (button.max.x - size.x).clamp(EDGE, (window.x - size.x - EDGE).max(EDGE));
    let below = button.max.y + GAP;
    let top = if below + size.y + EDGE <= window.y {
        below
    } else {
        (button.min.y - GAP - size.y).max(EDGE)
    };
    Vec2::new(left, top)
}

fn place_menu(
    buttons: Query<(&MenuButton, &ComputedNode, &UiGlobalTransform)>,
    layers: Query<(&MenuLayer, &Children)>,
    mut cards: Query<(&mut Node, &mut Visibility, &ComputedNode), With<MenuCard>>,
    windows: Query<&Window, With<PrimaryWindow>>,
) {
    let window = windows
        .iter()
        .next()
        .map_or(Vec2::splat(f32::MAX), |w| Vec2::new(w.width(), w.height()));
    for (layer, children) in &layers {
        let Some((_, node, at)) = buttons
            .iter()
            .find(|(b, _, _)| b.pr == layer.pr && b.key == layer.key)
        else {
            continue;
        };
        let scale = node.inverse_scale_factor;
        let button = Rect::from_center_size(at.translation * scale, node.size * scale);
        for child in children {
            if let Ok((mut card, mut visibility, size)) = cards.get_mut(*child) {
                let spot = menu_spot(button, size.size * size.inverse_scale_factor, window);
                let (l, t) = (px(spot.x), px(spot.y));
                if card.left != l || card.top != t {
                    card.left = l;
                    card.top = t;
                } else if *visibility != Visibility::Inherited {
                    *visibility = Visibility::Inherited;
                }
            }
        }
    }
}

pub(super) fn add_systems(app: &mut App) {
    app.add_systems(PreUpdate, close_on_escape.after(InputSystems))
        .add_systems(
            PostUpdate,
            place_menu.after(bevy::ui::UiSystems::PostLayout),
        );
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{DiagramsTell, Tell};
    use crate::fixture;
    use crate::screens::review::diagrams::model::DiagramsModel;
    use crate::testing::{self, NOW};
    use clusia_core::diagram::EntryState;

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn model() -> DiagramsModel {
        let mut m = DiagramsModel::default();
        let Tell::Diagrams(tell) = fixture::demo_diagrams_tell(&pr(), NOW) else {
            panic!("a diagrams tell");
        };
        m.apply(&tell);
        m
    }

    fn inputs(model: Option<&DiagramsModel>) -> Inputs<'_> {
        Inputs {
            model,
            harness: true,
            agent: "Claude Code",
            offline: false,
            selected: None,
            version: None,
            compare: false,
            menu: None,
            head: "",
        }
    }

    fn list_of(v: &DiagramsView) -> &ListView {
        v.list.as_ref().expect("a list")
    }

    fn tab_notice_of(v: &DiagramsView) -> &Notice {
        match &v.pane {
            Pane::Tab(n) => n,
            other => panic!("a whole-tab notice, got {other:?}"),
        }
    }

    #[test]
    fn nothing_is_offered_before_the_daemon_answers() {
        let v = diagrams_view(&inputs(None));
        assert!(v.list.is_none());
        let n = tab_notice_of(&v);
        assert_eq!(
            (n.title.as_str(), n.line.as_str()),
            ("Diagrams", "Loading diagrams…")
        );
        assert_eq!((n.action.clone(), n.setup), (None, false));
        let mut offline = inputs(None);
        offline.offline = true;
        assert_eq!(
            tab_notice_of(&diagrams_view(&offline)).line,
            "Diagrams are not available offline."
        );
    }

    #[test]
    fn an_empty_tab_offers_to_draw_or_to_set_up_a_harness() {
        let mut m = DiagramsModel {
            received: true,
            ..DiagramsModel::default()
        };
        let v = diagrams_view(&inputs(Some(&m)));
        let n = tab_notice_of(&v);
        assert_eq!(n.title, "No diagrams yet");
        assert_eq!(
            n.line,
            "Each diagram is one turn on your Claude Code account."
        );
        assert_eq!((n.action.clone(), n.setup), (Some(Action::Draw), false));
        let mut none = inputs(Some(&m));
        none.harness = false;
        let n = tab_notice_of(&diagrams_view(&none)).clone();
        assert_eq!(n.line, "Set up a harness in Config › Harness");
        assert_eq!((n.action, n.setup), (None, true));
        m.state = DiagramsState::Planning;
        let n = tab_notice_of(&diagrams_view(&inputs(Some(&m)))).clone();
        assert_eq!(n.title, "Choosing which diagrams explain this change…");
        assert_eq!(n.action, Some(Action::Stop));
        m.state = DiagramsState::Failed {
            message: "the agent answered with no plan\u{1b}[31m".into(),
        };
        let n = tab_notice_of(&diagrams_view(&inputs(Some(&m)))).clone();
        assert_eq!(n.title, "The diagrams could not be drawn");
        assert!(n.line.starts_with("the agent answered with no plan"));
        assert!(!n.line.contains('\u{1b}'), "agent text is made printable");
        assert_eq!(n.action, Some(Action::TryAgain));
    }

    #[test]
    fn the_list_matches_the_mockup() {
        let m = model();
        let v = diagrams_view(&inputs(Some(&m)));
        let list = list_of(&v);
        assert_eq!(list.heading, "DIAGRAMS · 8");
        let rows: Vec<(&str, &str, bool)> = list
            .rows
            .iter()
            .map(|r| (r.name.as_str(), r.meta.as_str(), r.selected))
            .collect();
        assert_eq!(
            rows,
            [
                ("Token refresh flow", "Flow · 5 boxes", true),
                ("Module dependencies", "Graph · 6 modules", false),
                ("Refresh call, step by step", "Sequence · 8 messages", false),
                ("Token data model", "Entity–relationship · 4", false),
                ("What people can do", "Use cases · 7", false),
                ("Where the token goes", "Data flow · 2 processes", false),
                ("Token life cycle", "States · 4", false),
                ("Types involved", "Classes · 3", false),
            ]
        );
        let f = &list.footer;
        assert_eq!(
            f.text,
            "Drawn by Claude Code from commit 9be1d07. When the PR changes, ask for new diagrams; the old ones stay with their commit."
        );
        assert_eq!(f.redraw, None, "the head is the latest version's commit");
        assert_eq!(
            f.stepper,
            Some(Stepper {
                number: 2,
                of: 2,
                prev: true,
                next: false
            })
        );
        assert_eq!(
            f.compare,
            Some(Compare {
                since: 1,
                on: false
            })
        );
        assert_eq!(
            v.pane,
            Pane::Canvas {
                number: 2,
                key: "flow".into(),
                compare: false
            }
        );
    }

    #[test]
    fn the_selection_is_kept_and_compare_needs_a_diagram_drawn_before() {
        let m = model();
        let mut i = inputs(Some(&m));
        i.selected = Some("deps");
        i.compare = true;
        let v = diagrams_view(&i);
        let selected: Vec<&str> = list_of(&v)
            .rows
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.key.as_str())
            .collect();
        assert_eq!(selected, ["deps"]);
        assert_eq!(list_of(&v).footer.compare, None, "deps is new in version 2");
        assert_eq!(
            v.pane,
            Pane::Canvas {
                number: 2,
                key: "deps".into(),
                compare: false
            }
        );
        i.selected = Some("sequence");
        let v = diagrams_view(&i);
        assert_eq!(
            v.pane,
            Pane::Canvas {
                number: 2,
                key: "sequence".into(),
                compare: true
            }
        );
        assert_eq!(
            list_of(&v).footer.compare,
            Some(Compare { since: 1, on: true })
        );
    }

    #[test]
    fn an_older_version_is_read_only_and_has_nothing_before_it() {
        let mut m = model();
        m.loaded
            .insert(1, fixture::demo_diagrams(NOW, &pr()).remove(0));
        let mut i = inputs(Some(&m));
        i.version = Some(1);
        i.compare = true;
        let v = diagrams_view(&i);
        let list = list_of(&v);
        assert_eq!(list.rows.len(), 3);
        assert!(list.footer.old);
        assert_eq!(list.footer.compare, None);
        assert_eq!(
            list.footer.stepper,
            Some(Stepper {
                number: 1,
                of: 2,
                prev: false,
                next: true
            })
        );
        assert!(list.footer.text.contains("commit 3f2a1c0"));
        assert_eq!(list.footer.redraw, None);
        assert!(
            list.rows
                .iter()
                .all(|r| r.menu.is_none() || !r.menu.as_ref().unwrap().redraw)
        );
        let mut unloaded = model();
        unloaded.loaded.remove(&1);
        let mut i = inputs(Some(&unloaded));
        i.version = Some(1);
        assert_eq!(diagrams_view(&i).pane, Pane::Loading);
    }

    #[test]
    fn a_new_commit_offers_a_redraw_on_the_latest_version_only() {
        let mut m = model();
        m.head = "feedfacefeedfacefeedfacefeedfacefeedface".into();
        m.stale = true;
        m.stale_reason = Some("The refresh code moved to a new module.".into());
        let v = diagrams_view(&inputs(Some(&m)));
        assert_eq!(
            list_of(&v).footer.redraw.as_deref(),
            Some("The refresh code moved to a new module.")
        );
        m.stale_reason = None;
        assert_eq!(
            list_of(&diagrams_view(&inputs(Some(&m))))
                .footer
                .redraw
                .as_deref(),
            Some("The pull request has new commits.")
        );
        let mut older = inputs(Some(&m));
        older.version = Some(1);
        assert_eq!(list_of(&diagrams_view(&older)).footer.redraw, None);
        let mut none = inputs(Some(&m));
        none.harness = false;
        assert_eq!(list_of(&diagrams_view(&none)).footer.redraw, None);
        m.state = DiagramsState::Planning;
        assert_eq!(
            list_of(&diagrams_view(&inputs(Some(&m)))).footer.redraw,
            None
        );
    }

    #[test]
    fn a_push_the_tab_knows_of_offers_the_redraw_before_the_daemon_is_asked_again() {
        let m = model();
        assert!(!m.stale && m.head == m.versions[1].commit);
        let mut i = inputs(Some(&m));
        i.head = "feedfacefeedfacefeedfacefeedfacefeedface";
        let v = diagrams_view(&i);
        assert_eq!(
            list_of(&v).footer.redraw.as_deref(),
            Some("The pull request has new commits.")
        );
        i.head = &m.versions[1].commit;
        assert_eq!(list_of(&diagrams_view(&i)).footer.redraw, None);
        i.head = "";
        assert_eq!(list_of(&diagrams_view(&i)).footer.redraw, None);
    }

    #[test]
    fn a_run_shows_its_progress_with_stop_and_no_redraw() {
        let mut m = model();
        m.state = DiagramsState::Drawing {
            done: 1,
            total: 5,
            key: "deps".into(),
        };
        m.versions[1].entries[1].state = EntryStateName::Drawing;
        m.versions[1].entries[2].state = EntryStateName::Waiting;
        let v = diagrams_view(&inputs(Some(&m)));
        let list = list_of(&v);
        assert_eq!(
            list.footer.working.as_deref(),
            Some("Drawing diagram 2 of 5…")
        );
        assert_eq!(list.rows[1].meta, "drawing…");
        assert_eq!(list.rows[2].meta, "waiting");
        assert!(
            list.rows
                .iter()
                .all(|r| !r.menu.as_ref().is_some_and(|m| m.redraw)),
            "no redraw while a run is going"
        );
        m.state = DiagramsState::Planning;
        assert_eq!(
            list_of(&diagrams_view(&inputs(Some(&m))))
                .footer
                .working
                .as_deref(),
            Some("Choosing which diagrams explain this change…")
        );
        m.state = DiagramsState::Failed {
            message: "the harness stopped".into(),
        };
        assert_eq!(
            list_of(&diagrams_view(&inputs(Some(&m))))
                .footer
                .failure
                .as_deref(),
            Some("The last drawing failed: the harness stopped")
        );
    }

    #[test]
    fn an_entry_that_is_not_ready_says_why_and_offers_a_retry_on_the_latest_version() {
        let mut m = model();
        m.versions[1].entries[0].state = EntryStateName::Failed;
        m.versions[1].entries[0].reason = Some("the answer was not JSON".into());
        m.versions[1].entries[1].state = EntryStateName::NotDrawn;
        m.versions[1].entries[2].state = EntryStateName::Waiting;
        m.versions[1].entries[3].state = EntryStateName::Drawing;
        let mut i = inputs(Some(&m));
        i.selected = Some("flow");
        let v = diagrams_view(&i);
        assert_eq!(list_of(&v).rows[0].meta, "could not draw · Retry");
        assert_eq!(list_of(&v).rows[1].meta, "not drawn · Retry");
        assert_eq!(list_of(&v).rows[0].state, RowState::Failed);
        assert_eq!(
            v.pane,
            Pane::Entry {
                title: "Token refresh flow".into(),
                line: "Could not draw: the answer was not JSON".into(),
                retry: Some("flow".into())
            }
        );
        for (key, line, retry) in [
            (
                "deps",
                "Drawing stopped before this diagram. Retry to draw it.",
                Some("deps"),
            ),
            ("sequence", "Waiting for its turn.", None),
            ("er", "Claude Code is drawing this diagram.", None),
        ] {
            i.selected = Some(key);
            let Pane::Entry {
                line: got,
                retry: r,
                ..
            } = diagrams_view(&i).pane
            else {
                panic!("an entry pane for {key}");
            };
            assert_eq!((got.as_str(), r.as_deref()), (line, retry), "{key}");
        }
    }

    #[test]
    fn a_failed_entry_has_no_retry_on_an_older_version_or_while_a_run_goes() {
        let mut m = model();
        m.versions[0].entries[0].state = EntryStateName::Failed;
        let mut i = inputs(Some(&m));
        i.version = Some(1);
        i.selected = Some("flow");
        let v = diagrams_view(&i);
        assert_eq!(list_of(&v).rows[0].meta, "could not draw");
        let Pane::Entry { retry, .. } = v.pane else {
            panic!("an entry pane");
        };
        assert_eq!(retry, None);
        let mut m = model();
        m.versions[1].entries[0].state = EntryStateName::Failed;
        m.state = DiagramsState::Drawing {
            done: 0,
            total: 2,
            key: "deps".into(),
        };
        let mut i = inputs(Some(&m));
        i.selected = Some("flow");
        let Pane::Entry { retry, .. } = diagrams_view(&i).pane else {
            panic!("an entry pane");
        };
        assert_eq!(retry, None);
    }

    #[test]
    fn the_menu_offers_redraw_and_the_dropped_items() {
        let m = model();
        let e = &m.versions[1].entries[0];
        assert_eq!(
            menu_of(e, true, false, false),
            Some(MenuView {
                open: false,
                redraw: true,
                dropped: None
            })
        );
        assert_eq!(menu_of(e, false, false, false), None, "an older version");
        assert_eq!(menu_of(e, true, true, false), None, "a run is going");
        let mut noted = e.clone();
        noted.notes = 3;
        assert_eq!(
            menu_of(&noted, false, false, true),
            Some(MenuView {
                open: true,
                redraw: false,
                dropped: Some("3 items dropped".into())
            })
        );
        noted.notes = 1;
        assert_eq!(
            menu_of(&noted, true, false, false)
                .unwrap()
                .dropped
                .as_deref(),
            Some("1 item dropped")
        );
    }

    #[test]
    fn the_menu_goes_under_its_button_or_above_it_and_stays_in_the_window() {
        let window = Vec2::new(1280.0, 800.0);
        let size = Vec2::new(184.0, 64.0);
        let button = Rect::from_corners(Vec2::new(220.0, 200.0), Vec2::new(246.0, 226.0));
        assert_eq!(menu_spot(button, size, window), Vec2::new(62.0, 230.0));
        let low = Rect::from_corners(Vec2::new(220.0, 760.0), Vec2::new(246.0, 786.0));
        assert_eq!(menu_spot(low, size, window), Vec2::new(62.0, 692.0));
        let left = Rect::from_corners(Vec2::new(4.0, 10.0), Vec2::new(30.0, 36.0));
        assert_eq!(menu_spot(left, size, window).x, 8.0);
    }

    fn open(app: &mut App) -> PrRef {
        let pr = testing::open_ready(app, false);
        testing::open_diagrams(app, &pr);
        testing::set_section(app, &pr, ReviewSection::Diagrams);
        testing::recorded(app);
        pr
    }

    fn ids(app: &mut App) -> Vec<Entity> {
        let mut q = app.world_mut().query::<(Entity, &DiagramCanvas)>();
        q.iter(app.world()).map(|(e, _)| e).collect()
    }

    #[test]
    fn the_tab_lists_the_diagrams_and_holds_a_canvas_for_the_selected_one() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open(&mut app);
        assert_eq!(testing::count::<DiagramRowButton>(&mut app), 8);
        assert_eq!(testing::count::<DiagramCanvas>(&mut app), 1);
        assert!(testing::shows(&mut app, "DIAGRAMS · 8"));
        assert!(testing::shows(&mut app, "Token refresh flow"));
        assert!(testing::shows(
            &mut app,
            "Drawn by Claude Code from commit 9be1d07"
        ));
        assert!(testing::shows(&mut app, "Version 2 of 2"));
        assert!(testing::shows(&mut app, "Show changes since version 1"));
        let before = ids(&mut app);
        let row = testing::find::<DiagramRowButton>(&mut app, |r| r.key == "sequence");
        testing::activate(&mut app, row);
        testing::settle(&mut app);
        assert_eq!(
            testing::tab(&app, &pr).ui.diagram.as_deref(),
            Some("sequence")
        );
        assert_eq!(testing::count::<DiagramCanvas>(&mut app), 1);
        assert_ne!(ids(&mut app), before, "another diagram is another canvas");
    }

    #[test]
    fn a_running_draw_changes_the_footer_and_keeps_the_canvas() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open(&mut app);
        let before = ids(&mut app);
        testing::tell(
            &mut app,
            Tell::Diagrams(DiagramsTell::State {
                pr: pr.clone(),
                state: DiagramsState::Drawing {
                    done: 1,
                    total: 5,
                    key: "deps".into(),
                },
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Drawing diagram 2 of 5…"));
        assert_eq!(ids(&mut app), before);
        let stop = testing::find::<DiagramButton>(&mut app, |b| b.action == Action::Stop);
        testing::activate(&mut app, stop);
        assert_eq!(testing::recorded(&mut app), [Ask::StopDiagrams(pr.clone())]);
    }

    #[test]
    fn the_stepper_walks_the_versions_and_compare_toggles() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open(&mut app);
        let step = |app: &mut App, n: i32| {
            let e = testing::find::<VersionStep>(app, |s| s.step == n);
            testing::activate(app, e);
            testing::settle(app);
        };
        step(&mut app, -1);
        assert_eq!(testing::tab(&app, &pr).ui.diagram_version, Some(1));
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::GetDiagramVersion {
                pr: pr.clone(),
                number: 1
            }],
            "the older version is asked for once it is shown"
        );
        assert!(testing::shows(&mut app, "Loading this version…"));
        testing::tell(
            &mut app,
            Tell::Diagrams(DiagramsTell::Version {
                pr: pr.clone(),
                version: Box::new(fixture::demo_diagrams(NOW, &pr).remove(0)),
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "An older version: read-only."));
        assert!(testing::shows(&mut app, "Version 1 of 2"));
        assert_eq!(testing::count::<DiagramCanvas>(&mut app), 1);
        assert_eq!(testing::count::<DiagramRowButton>(&mut app), 3);
        assert!(testing::recorded(&mut app).is_empty(), "asked only once");
        step(&mut app, 1);
        assert_eq!(
            testing::tab(&app, &pr).ui.diagram_version,
            None,
            "the latest version is followed again"
        );
        let toggle = testing::find::<CompareToggle>(&mut app, |_| true);
        testing::activate(&mut app, toggle);
        assert!(testing::tab(&app, &pr).ui.diagram_compare);
        testing::settle(&mut app);
        let toggle = testing::find::<CompareToggle>(&mut app, |_| true);
        testing::activate(&mut app, toggle);
        assert!(!testing::tab(&app, &pr).ui.diagram_compare);
    }

    #[test]
    fn the_buttons_send_their_asks() {
        let mut app = testing::app(fixture::demo(NOW));
        // The demo has no harness program; the tab offers a draw only once Claude Code is set up.
        testing::set_config_locally(&mut app, "harness.program", "/opt/homebrew/bin/claude");
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, ReviewSection::Diagrams);
        testing::tell(
            &mut app,
            Tell::Diagrams(DiagramsTell::Loaded {
                pr: pr.clone(),
                versions: Vec::new(),
                latest: None,
                state: DiagramsState::Idle,
                head: String::new(),
                stale: false,
                stale_reason: None,
            }),
        );
        testing::settle(&mut app);
        testing::recorded(&mut app);
        assert!(testing::shows(&mut app, "No diagrams yet"));
        let draw = testing::find::<DiagramButton>(&mut app, |b| b.action == Action::Draw);
        testing::activate(&mut app, draw);
        assert_eq!(testing::recorded(&mut app), [Ask::DrawDiagrams(pr.clone())]);
        testing::open_diagrams(&mut app, &pr);
        let tab = &mut app.world_mut().resource_mut::<ReviewTabs>().0;
        tab.get_mut(&pr).unwrap().ui.diagram = Some("er".into());
        testing::tell(
            &mut app,
            Tell::Diagrams(DiagramsTell::Entry {
                pr: pr.clone(),
                number: 2,
                key: "er".into(),
                state: EntryState::Failed {
                    reason: "timed out".into(),
                },
            }),
        );
        testing::settle(&mut app);
        let retry =
            testing::find::<DiagramButton>(&mut app, |b| b.action == Action::Retry("er".into()));
        testing::activate(&mut app, retry);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::DrawDiagram {
                pr,
                key: "er".into()
            }]
        );
    }

    #[test]
    fn a_lost_connection_refuses_to_draw() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::set_config_locally(&mut app, "harness.program", "/opt/homebrew/bin/claude");
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, ReviewSection::Diagrams);
        testing::tell(
            &mut app,
            Tell::Diagrams(DiagramsTell::Loaded {
                pr: pr.clone(),
                versions: Vec::new(),
                latest: None,
                state: DiagramsState::Idle,
                head: String::new(),
                stale: false,
                stale_reason: None,
            }),
        );
        testing::settle(&mut app);
        testing::recorded(&mut app);
        app.world_mut()
            .resource_mut::<crate::bridge::Model>()
            .connection = crate::bridge::Connection::Lost("gone".into());
        let draw = testing::find::<DiagramButton>(&mut app, |b| b.action == Action::Draw);
        testing::activate(&mut app, draw);
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn the_menu_redraws_one_entry_and_closes() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open(&mut app);
        assert_eq!(
            testing::count::<MenuButton>(&mut app),
            1,
            "only the selected row has one"
        );
        assert_eq!(testing::count::<MenuLayer>(&mut app), 0);
        let button = testing::find::<MenuButton>(&mut app, |b| b.key == "flow");
        testing::activate(&mut app, button);
        testing::settle(&mut app);
        assert_eq!(
            testing::tab(&app, &pr).ui.diagram_menu.as_deref(),
            Some("flow")
        );
        assert_eq!(testing::count::<MenuLayer>(&mut app), 1);
        assert!(testing::shows(&mut app, "Redraw"));
        let item = testing::find::<RedrawItem>(&mut app, |i| i.key == "flow");
        testing::activate(&mut app, item);
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::DrawDiagram {
                pr: pr.clone(),
                key: "flow".into()
            }]
        );
        assert_eq!(testing::tab(&app, &pr).ui.diagram_menu, None);
        assert_eq!(testing::count::<MenuLayer>(&mut app), 0);
    }

    #[test]
    fn escape_and_another_row_close_the_menu() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open(&mut app);
        let button = testing::find::<MenuButton>(&mut app, |b| b.key == "flow");
        testing::activate(&mut app, button);
        testing::settle(&mut app);
        assert_eq!(testing::count::<MenuLayer>(&mut app), 1);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        testing::settle(&mut app);
        assert_eq!(testing::tab(&app, &pr).ui.diagram_menu, None);
        assert_eq!(testing::count::<MenuLayer>(&mut app), 0);
        let button = testing::find::<MenuButton>(&mut app, |b| b.key == "flow");
        testing::activate(&mut app, button);
        testing::settle(&mut app);
        let row = testing::find::<DiagramRowButton>(&mut app, |r| r.key == "er");
        testing::activate(&mut app, row);
        testing::settle(&mut app);
        assert_eq!(testing::count::<MenuLayer>(&mut app), 0);
    }

    #[test]
    fn the_menu_goes_with_the_section() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open(&mut app);
        let button = testing::find::<MenuButton>(&mut app, |b| b.key == "flow");
        testing::activate(&mut app, button);
        testing::settle(&mut app);
        assert_eq!(testing::count::<MenuLayer>(&mut app), 1);
        testing::set_section(&mut app, &pr, ReviewSection::Diff);
        assert_eq!(testing::count::<MenuLayer>(&mut app), 0);
    }

    #[test]
    fn the_diff_takes_the_body_back_from_the_diagrams() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open(&mut app);
        assert_eq!(testing::count::<DiagramsRoot>(&mut app), 1);
        testing::set_section(&mut app, &pr, ReviewSection::Diff);
        assert_eq!(testing::count::<DiagramsRoot>(&mut app), 0);
        assert_eq!(testing::count::<DiagramCanvas>(&mut app), 0);
        testing::set_section(&mut app, &pr, ReviewSection::Diagrams);
        assert_eq!(testing::count::<DiagramsRoot>(&mut app), 1);
        assert_eq!(testing::count::<DiagramCanvas>(&mut app), 1);
    }
}
