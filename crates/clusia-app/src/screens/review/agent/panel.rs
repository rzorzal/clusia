//! The right column of a review: the Agent and Draft tabs, and the agent's chat (mockup
//! `AgentChat.png`).
//!
//! The shell spawns the column empty (`PanelColumn` with `PanelTabs`, the draft body and
//! `AgentRegion`); the first frame it is on screen `AgentRegion` gets a header, a scrolling
//! transcript and a footer with the input. Each part keeps the view it was built from and
//! rebuilds only when that changes, so streaming text never rebuilds the input.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::Pickable;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_protocol::{HarnessKind, SessionStateKind};

use super::model::{ChatLine, Chats, DEFAULT_WIDTH, PanelTab, display_text};
use crate::bridge::{Ask, Asks, Model};
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen};
use crate::review_state::ReviewTabs;
use crate::screens::review::ReviewSystems;
use crate::screens::review::editor::read_only_reason;
use crate::screens::review::shell::{HarnessButton, RightPanel, on_harness};
use crate::snapshot::Snapshot;
use crate::theme::{Swatch, Theme};
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Stroke, Tone, Type, Variant, badge, button, panel, text,
};
use crate::ui::markdown::parse::parse;
use crate::ui::markdown::{RenderOpts, markdown};
use crate::ui::text_area::{Caret, growing_line_area, set_text};

/// The `TextArea::id` of the chat input.
pub const CHAT_INPUT: u64 = 2;

/// The column the shell puts right of the section body. Its width and visibility come from
/// the review's `ChatModel`.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PanelColumn {
    pub pr: PrRef,
}

impl PanelColumn {
    pub fn new(pr: PrRef) -> Self {
        Self { pr }
    }
}

/// The header of the column: the **Agent** and **Draft** tabs.
#[derive(Component, Debug)]
pub struct PanelTabs {
    pr: PrRef,
    built: Option<TabsView>,
}

impl PanelTabs {
    pub fn new(pr: PrRef) -> Self {
        Self { pr, built: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TabsView {
    tab: PanelTab,
    drafts: usize,
}

/// One of the two tabs.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PanelTabButton {
    pub pr: PrRef,
    pub tab: PanelTab,
}

/// The agent's side of the column. Spawned by the shell.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct AgentRegion {
    pub pr: PrRef,
    filled: bool,
}

impl AgentRegion {
    pub fn new(pr: PrRef) -> Self {
        Self { pr, filled: false }
    }
}

/// **Hide panel** / **Show panel** in the status bar.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PanelToggle(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ChatInput(pub PrRef);

/// The hint inside an empty input.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ChatPlaceholder(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ChatSend(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ChatStop(pub PrRef);

/// The `Draft · 3 items` of the footer: it opens the Draft tab.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ChatDraftLink(pub PrRef);

/// The edge the user drags to resize the column.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ChatResize(pub PrRef);

#[derive(Debug, Clone, PartialEq)]
struct HeaderView {
    state: SessionStateKind,
    resumed: bool,
    /// Claude Code is set up; without it the header leads to Config › Harness.
    ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HintsView {
    draft: usize,
    waiting: usize,
}

#[derive(Component)]
struct ChatHeader {
    pr: PrRef,
    built: Option<HeaderView>,
}

#[derive(Component)]
struct Transcript {
    pr: PrRef,
    built: Option<Vec<ChatLine>>,
}

#[derive(Component)]
struct Hints {
    pr: PrRef,
    built: Option<HintsView>,
}

pub struct AgentPanel;

impl Plugin for AgentPanel {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                drop_closed_chats,
                open_chats,
                toggle_with_shortcut,
                layout,
                rebuild_tabs,
                fill_region,
                apply_prefill,
                rebuild_header,
                rebuild_transcript,
                rebuild_hints,
                send_on_enter,
                placeholder_visibility,
            )
                .chain()
                .after(ReviewSystems),
        );
    }
}

/// Whether Claude Code is set up: a program is configured, or the first-run scan found one.
pub fn harness_ready(snap: &Snapshot) -> bool {
    snap.config.harness.program.is_some()
        || snap.first_run.as_ref().is_some_and(|f| {
            f.harnesses
                .iter()
                .any(|h| h.kind == HarnessKind::ClaudeCode && h.path.is_some())
        })
}

/// The text the denied line shows.
pub fn denied_text(tool: &str, detail: &str) -> String {
    if tool == "Bash" {
        format!("Wanted to run `{detail}` — needs permission, coming soon")
    } else {
        format!("Wanted to use {tool} ({detail}) — needs permission, coming soon")
    }
}

/// Sends `text` as a question to `pr`'s agent. Returns whether an ask went out; a review that
/// cannot be asked (not live, the cached copy) gets an error line and keeps the user's text.
pub fn send_message(
    pr: &PrRef,
    text: &str,
    tabs: &ReviewTabs,
    model: &Model,
    chats: &mut Chats,
    asks: &mut Asks,
) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let chat = chats.entry(pr);
    if let Some(reason) = read_only_reason(tabs, model, pr) {
        chat.push_error(reason.to_string());
        return false;
    }
    chat.push_me(text.to_string());
    asks.send(Ask::AgentSend {
        pr: pr.clone(),
        text: text.to_string(),
    });
    true
}

/// A chat is forgotten with its tab.
fn drop_closed_chats(tabs: Res<ReviewTabs>, mut chats: ResMut<Chats>) {
    if chats.0.keys().any(|pr| !tabs.0.contains_key(pr)) {
        chats.0.retain(|pr, _| tabs.0.contains_key(pr));
    }
}

/// Every review that is ready (not the cached copy) gets a chat, its first tab (Agent when a
/// harness is set up, Draft otherwise), and its log is asked once. The first tab follows the
/// harness until the user picks a tab or the chat has a line: `claude` found on the PATH
/// arrives with a scan that can end after the review opened.
fn open_chats(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut chats: ResMut<Chats>,
    mut asks: ResMut<Asks>,
) {
    for (pr, tab) in &tabs.0 {
        let Some(ready) = tab.ready() else { continue };
        if ready.cached_at.is_some() {
            continue;
        }
        let chat = chats.entry(pr);
        if !chat.tab_set && chat.lines.is_empty() {
            let first = if harness_ready(&model.snapshot) {
                PanelTab::Agent
            } else {
                PanelTab::Draft
            };
            if chat.tab != first {
                chat.tab = first;
            }
        }
        if !chat.log_asked {
            chat.log_asked = true;
            asks.send(Ask::AgentLog { pr: pr.clone() });
        }
    }
}

/// ⌘\ hides or shows the column of the review on screen.
fn toggle_with_shortcut(
    keys: Res<ButtonInput<KeyCode>>,
    nav: Res<Nav>,
    tabs: Res<ReviewTabs>,
    mut chats: ResMut<Chats>,
) {
    let cmd = keys.any_pressed([KeyCode::SuperLeft, KeyCode::SuperRight]);
    if !(cmd && keys.just_pressed(KeyCode::Backslash)) {
        return;
    }
    if let Screen::Review(pr) = &nav.screen
        && tabs.0.get(pr).is_some_and(|t| t.ready().is_some())
    {
        chats.entry(pr).toggle();
    }
}

fn flex_if(show: bool) -> Display {
    if show { Display::Flex } else { Display::None }
}

/// The column's visibility and width, and which tab's body is shown.
fn layout(
    chats: Res<Chats>,
    mut columns: Query<(&PanelColumn, &mut Node)>,
    mut regions: Query<(&AgentRegion, &mut Node), Without<PanelColumn>>,
    mut drafts: Query<(&RightPanel, &mut Node), (Without<PanelColumn>, Without<AgentRegion>)>,
) {
    let tab_of = |pr: &PrRef| chats.0.get(pr).map_or(PanelTab::Draft, |c| c.tab);
    for (column, mut node) in &mut columns {
        let (open, width) = chats
            .0
            .get(&column.pr)
            .map_or((true, DEFAULT_WIDTH), |c| (c.open, c.width));
        if node.display != flex_if(open) {
            node.display = flex_if(open);
        }
        if node.width != px(width) {
            node.width = px(width);
        }
    }
    for (region, mut node) in &mut regions {
        let display = flex_if(tab_of(&region.pr) == PanelTab::Agent);
        if node.display != display {
            node.display = display;
        }
    }
    for (draft, mut node) in &mut drafts {
        let display = flex_if(tab_of(&draft.pr) == PanelTab::Draft);
        if node.display != display {
            node.display = display;
        }
    }
}

fn rebuild_tabs(
    mut commands: Commands,
    chats: Res<Chats>,
    tabs: Res<ReviewTabs>,
    fonts: Res<UiFonts>,
    mut headers: Query<(Entity, &mut PanelTabs)>,
) {
    for (entity, mut header) in &mut headers {
        let Some(chat) = chats.0.get(&header.pr) else {
            continue;
        };
        let drafts = tabs
            .0
            .get(&header.pr)
            .and_then(|t| t.ready())
            .map_or(0, |r| r.view.review.draft.items.len());
        let want = TabsView {
            tab: chat.tab,
            drafts,
        };
        if header.built.as_ref() == Some(&want) {
            continue;
        }
        let pr = header.pr.clone();
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|row| {
            for (tab, label) in [
                (PanelTab::Agent, "Agent".to_string()),
                (
                    PanelTab::Draft,
                    if want.drafts > 0 {
                        format!("Draft {}", want.drafts)
                    } else {
                        "Draft".to_string()
                    },
                ),
            ] {
                let on = tab == want.tab;
                row.spawn((
                    Node {
                        height: px(38),
                        padding: UiRect::horizontal(px(10)),
                        align_items: AlignItems::Center,
                        border: UiRect::bottom(px(2)),
                        ..default()
                    },
                    BorderColor::default(),
                    Stroke(if on { Swatch::Green } else { Swatch::Clear }),
                    (WidgetButton, Clickable),
                    Hovered::default(),
                    TabIndex(0),
                    BackgroundColor::default(),
                    Fill(Swatch::Clear),
                    HoverFill(Swatch::Hover),
                    PanelTabButton {
                        pr: pr.clone(),
                        tab,
                    },
                    observe(on_tab),
                    children![text(
                        &fonts,
                        label,
                        if on { Type::STRONG } else { Type::MUTED }
                    )],
                ));
            }
        });
        header.built = Some(want);
    }
}

/// Gives a new region its header, transcript and footer (once).
fn fill_region(
    mut commands: Commands,
    fonts: Res<UiFonts>,
    mut regions: Query<(Entity, &mut AgentRegion)>,
) {
    for (entity, mut region) in &mut regions {
        if region.filled {
            continue;
        }
        region.filled = true;
        let pr = region.pr.clone();
        commands.entity(entity).with_children(|p| {
            p.spawn((
                Node {
                    flex_shrink: 0.0,
                    align_items: AlignItems::Center,
                    column_gap: px(8),
                    padding: UiRect::axes(px(16), px(10)),
                    border: UiRect::bottom(px(1)),
                    ..default()
                },
                BorderColor::default(),
                Stroke(Swatch::Line),
                ChatHeader {
                    pr: pr.clone(),
                    built: None,
                },
            ));
            p.spawn((
                Node {
                    flex_grow: 1.0,
                    min_height: px(0),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(10),
                    padding: px(16).all(),
                    overflow: Overflow::scroll_y(),
                    ..default()
                },
                ScrollArea,
                ScrollPosition::default(),
                Transcript {
                    pr: pr.clone(),
                    built: None,
                },
            ));
            footer(p, &fonts, &pr);
        });
    }
}

/// The column's left edge, which the user drags to resize it (on either tab).
pub fn resize_edge(column: &mut ChildSpawnerCommands, pr: &PrRef) {
    column.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(0),
            top: px(0),
            bottom: px(0),
            width: px(6),
            ..default()
        },
        ChatResize(pr.clone()),
        observe(on_resize),
    ));
}

fn footer(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef) {
    p.spawn((
        Node {
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            row_gap: px(8),
            padding: px(12).all(),
            border: UiRect::top(px(1)),
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|f| {
        f.spawn(Node {
            column_gap: px(8),
            align_items: AlignItems::End,
            ..default()
        })
        .with_children(|row| {
            row.spawn(Node {
                flex_grow: 1.0,
                min_width: px(0),
                ..default()
            })
            .with_children(|w| {
                w.spawn((
                    growing_line_area(fonts, "", 1.0, 6.0, Caret::End, CHAT_INPUT),
                    ChatInput(pr.clone()),
                ));
                w.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(11),
                        top: px(9),
                        ..default()
                    },
                    Pickable::IGNORE,
                    ChatPlaceholder(pr.clone()),
                    children![text(fonts, "Ask about this pull request…", Type::MUTED)],
                ));
            });
            row.spawn((
                Node {
                    width: px(32),
                    height: px(32),
                    flex_shrink: 0.0,
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    border_radius: BorderRadius::MAX,
                    ..default()
                },
                (WidgetButton, Clickable),
                Hovered::default(),
                TabIndex(0),
                BackgroundColor::default(),
                Fill(Swatch::Green),
                HoverFill(Swatch::GreenHover),
                ChatSend(pr.clone()),
                observe(on_send),
                children![text(fonts, "↑", Type::STRONG.ink(Swatch::OnGreen))],
            ));
        });
        f.spawn((
            Node {
                column_gap: px(12),
                justify_content: JustifyContent::SpaceBetween,
                ..default()
            },
            Hints {
                pr: pr.clone(),
                built: None,
            },
        ));
    });
}

fn rebuild_header(
    mut commands: Commands,
    chats: Res<Chats>,
    model: Res<Model>,
    fonts: Res<UiFonts>,
    mut headers: Query<(Entity, &mut ChatHeader)>,
) {
    for (entity, mut header) in &mut headers {
        let Some(chat) = chats.0.get(&header.pr).filter(|c| c.tab == PanelTab::Agent) else {
            continue;
        };
        let want = HeaderView {
            state: chat.state,
            resumed: chat.resumed,
            ready: harness_ready(&model.snapshot),
        };
        if header.built.as_ref() == Some(&want) {
            continue;
        }
        let pr = header.pr.clone();
        commands.entity(entity).despawn_related::<Children>();
        if !want.ready {
            commands
                .entity(entity)
                .with_children(|h| not_set_up(h, &fonts));
            header.built = Some(want);
            continue;
        }
        commands.entity(entity).with_children(|h| {
            h.spawn(panel(
                Node {
                    width: px(8),
                    height: px(8),
                    border_radius: BorderRadius::MAX,
                    ..default()
                },
                if want.state == SessionStateKind::None {
                    Swatch::Faint
                } else {
                    Swatch::Green
                },
            ));
            h.spawn(text(&fonts, "Claude Code", Type::STRONG));
            h.spawn(text(
                &fonts,
                if want.resumed {
                    "session resumed · knows this review"
                } else {
                    "knows this review"
                },
                Type::META,
            ));
            h.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            if matches!(
                want.state,
                SessionStateKind::Running | SessionStateKind::Queued
            ) {
                h.spawn((
                    button(&fonts, "Stop", Variant::Ghost),
                    ChatStop(pr.clone()),
                    observe(on_stop),
                ));
            }
            h.spawn((
                button(&fonts, "Settings", Variant::Ghost),
                HarnessButton,
                observe(on_harness),
            ));
        });
        header.built = Some(want);
    }
}

/// The header without a harness: what the tab is for, and the way to set one up.
fn not_set_up(h: &mut ChildSpawnerCommands, fonts: &UiFonts) {
    h.spawn(text(fonts, "Claude Code", Type::STRONG));
    h.spawn(badge(fonts, "not set up", Tone::Neutral));
    h.spawn(Node {
        flex_grow: 1.0,
        ..default()
    });
    h.spawn((
        button(fonts, "Set up a harness", Variant::Secondary),
        HarnessButton,
        observe(on_harness),
    ));
}

fn rebuild_transcript(
    mut commands: Commands,
    chats: Res<Chats>,
    fonts: Res<UiFonts>,
    theme: Res<Theme>,
    model: Res<Model>,
    mut parts: Query<(
        Entity,
        &mut Transcript,
        &mut ScrollPosition,
        &ComputedNode,
        Option<&Children>,
    )>,
) {
    let opts = RenderOpts::from_config(theme.code_size, &model.snapshot.config);
    for (entity, mut part, mut scroll, node, children) in &mut parts {
        let Some(chat) = chats.0.get(&part.pr).filter(|c| c.tab == PanelTab::Agent) else {
            continue;
        };
        if part.built.as_ref() == Some(&chat.lines) {
            continue;
        }
        let pr = part.pr.clone();
        let follow = part.built.is_none() || at_bottom(node);
        let last = children.and_then(|c| c.last()).copied();
        match (last, part.built.as_deref()) {
            (Some(last), Some(built)) if only_the_answer_grew(built, &chat.lines) => {
                // Each line is one child, so the answer's line is the last one.
                commands.entity(last).despawn();
                commands.entity(entity).with_children(|t| {
                    if let Some(line) = chat.lines.last() {
                        chat_line(t, &fonts, &pr, line, &opts);
                    }
                });
            }
            _ => {
                commands.entity(entity).despawn_related::<Children>();
                commands.entity(entity).with_children(|t| {
                    for line in &chat.lines {
                        chat_line(t, &fonts, &pr, line, &opts);
                    }
                });
            }
        }
        if follow {
            // The layout clamps this to the end of the content: the newest line stays in view.
            scroll.y = f32::MAX;
        }
        part.built = Some(chat.lines.clone());
    }
}

/// The lines differ only in the text of the last one, an answer still arriving.
fn only_the_answer_grew(built: &[ChatLine], lines: &[ChatLine]) -> bool {
    let n = lines.len();
    n > 0
        && built.len() == n
        && built[..n - 1] == lines[..n - 1]
        && matches!(
            (&built[n - 1], &lines[n - 1]),
            (ChatLine::Text(_), ChatLine::Text(_))
        )
}

/// The view shows the end of its content (or has nothing to scroll), as of the last layout.
fn at_bottom(node: &ComputedNode) -> bool {
    let end = (node.content_size.y - node.size.y + node.scrollbar_size.y).max(0.0);
    node.scroll_position.y >= end.floor() - 1.0
}

fn chat_line(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    line: &ChatLine,
    opts: &RenderOpts,
) {
    match line {
        ChatLine::Me(body) => {
            p.spawn(panel(
                Node {
                    align_self: AlignSelf::FlexEnd,
                    max_width: percent(85),
                    padding: UiRect::axes(px(14), px(10)),
                    border_radius: BorderRadius::all(px(12)),
                    ..default()
                },
                Swatch::GreenSoft,
            ))
            .with_children(|b| {
                b.spawn(text(fonts, body.clone(), Type::BODY));
            });
        }
        ChatLine::Text(raw) => {
            let shown = display_text(raw);
            if shown.is_empty() {
                // Every line is one child, even one with nothing to show yet.
                p.spawn(Node {
                    display: Display::None,
                    ..default()
                });
            } else {
                markdown(p, fonts, &parse(&shown), opts);
            }
        }
        ChatLine::Tool(summary) => pill(
            p,
            fonts,
            "✓",
            Swatch::Green,
            summary,
            Swatch::Chrome,
            Swatch::Muted,
        ),
        ChatLine::Denied { tool, detail } => pill(
            p,
            fonts,
            "⊘",
            Swatch::Orange,
            &denied_text(tool, detail),
            Swatch::OrangeSoft,
            Swatch::Orange,
        ),
        ChatLine::Error(message) => {
            p.spawn(text(fonts, message.clone(), Type::BODY.ink(Swatch::Orange)));
        }
        ChatLine::Suggestion { suggestion, state } => {
            super::suggestion::card(p, fonts, pr, suggestion, *state, opts);
        }
    }
}

/// A one-line chip: a mark and a short text.
pub(super) fn pill(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    mark: &str,
    mark_ink: Swatch,
    body: &str,
    fill: Swatch,
    ink: Swatch,
) {
    p.spawn(panel(
        Node {
            align_self: AlignSelf::FlexStart,
            max_width: percent(100),
            column_gap: px(6),
            align_items: AlignItems::Center,
            padding: UiRect::axes(px(10), px(5)),
            border_radius: BorderRadius::all(px(14)),
            ..default()
        },
        fill,
    ))
    .with_children(|r| {
        r.spawn(text(fonts, mark.to_string(), Type::META.ink(mark_ink)));
        r.spawn(text(fonts, body.to_string(), Type::META.ink(ink)));
    });
}

fn rebuild_hints(
    mut commands: Commands,
    chats: Res<Chats>,
    tabs: Res<ReviewTabs>,
    fonts: Res<UiFonts>,
    mut parts: Query<(Entity, &mut Hints)>,
) {
    for (entity, mut hints) in &mut parts {
        let Some(chat) = chats.0.get(&hints.pr).filter(|c| c.tab == PanelTab::Agent) else {
            continue;
        };
        let draft = tabs
            .0
            .get(&hints.pr)
            .and_then(|t| t.ready())
            .map_or(0, |r| r.view.review.draft.items.len());
        let want = HintsView {
            draft,
            waiting: chat.waiting(),
        };
        if hints.built.as_ref() == Some(&want) {
            continue;
        }
        let mut left = format!(
            "Draft · {} item{}",
            want.draft,
            if want.draft == 1 { "" } else { "s" }
        );
        if want.waiting > 0 {
            left.push_str(&format!(
                " · {} suggestion{} waiting",
                want.waiting,
                if want.waiting == 1 { "" } else { "s" }
            ));
        }
        let pr = hints.pr.clone();
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|h| {
            h.spawn((
                Node {
                    max_width: percent(50),
                    padding: UiRect::axes(px(4), px(2)),
                    border_radius: BorderRadius::all(px(4)),
                    ..default()
                },
                (WidgetButton, Clickable),
                Hovered::default(),
                TabIndex(0),
                BackgroundColor::default(),
                Fill(Swatch::Clear),
                HoverFill(Swatch::Hover),
                ChatDraftLink(pr),
                observe(on_draft_link),
                children![text(&fonts, left, Type::META)],
            ));
            h.spawn((
                Node {
                    max_width: percent(50),
                    ..default()
                },
                children![text(
                    &fonts,
                    "Can read the worktree · can't run commands yet",
                    Type::META
                )],
            ));
        });
        hints.built = Some(want);
    }
}

/// Puts the question an "Ask the agent about this line" started in the input, after what is
/// already typed there, and focuses the input.
fn apply_prefill(
    mut chats: ResMut<Chats>,
    mut inputs: Query<(Entity, &ChatInput, &mut EditableText)>,
    mut focus: ResMut<bevy::input_focus::InputFocus>,
) {
    for (entity, ChatInput(pr), mut editable) in &mut inputs {
        if !chats.0.get(pr).is_some_and(|c| c.prefill.is_some()) {
            continue;
        }
        let Some(prompt) = chats.0.get_mut(pr).and_then(|c| c.prefill.take()) else {
            continue;
        };
        let typed = editable.value().to_string();
        let text = if typed.trim().is_empty() {
            prompt
        } else {
            format!("{}\n{prompt}", typed.trim_end_matches('\n'))
        };
        set_text(&mut editable, &text);
        focus.set(entity, bevy::input_focus::FocusCause::Navigated);
    }
}

/// Enter sends what is in the focused input; Shift+Enter breaks the line.
fn send_on_enter(
    keys: Res<ButtonInput<KeyCode>>,
    focus: Res<bevy::input_focus::InputFocus>,
    mut inputs: Query<(&ChatInput, &mut EditableText)>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut chats: ResMut<Chats>,
    mut asks: ResMut<Asks>,
) {
    if !keys.just_pressed(KeyCode::Enter) {
        return;
    }
    let Some(entity) = focus.get() else { return };
    let Ok((ChatInput(pr), mut editable)) = inputs.get_mut(entity) else {
        return;
    };
    if editable.is_composing() {
        return;
    }
    if keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]) {
        editable.queue_edit(bevy::text::TextEdit::Insert("\n".into()));
        return;
    }
    let value = editable.value().to_string();
    if send_message(pr, &value, &tabs, &model, &mut chats, &mut asks) {
        set_text(&mut editable, "");
    }
}

/// The hint is shown while the input is empty.
fn placeholder_visibility(
    inputs: Query<(&ChatInput, &EditableText)>,
    mut holders: Query<(&ChatPlaceholder, &mut Node)>,
) {
    for (ChatPlaceholder(pr), mut node) in &mut holders {
        let empty = inputs
            .iter()
            .find(|(i, _)| &i.0 == pr)
            .is_none_or(|(_, e)| e.value().to_string().is_empty());
        let display = flex_if(empty);
        if node.display != display {
            node.display = display;
        }
    }
}

fn on_tab(activate: On<Activate>, buttons: Query<&PanelTabButton>, mut chats: ResMut<Chats>) {
    if let Ok(PanelTabButton { pr, tab }) = buttons.get(activate.entity) {
        chats.entry(pr).show(*tab);
    }
}

fn on_draft_link(activate: On<Activate>, links: Query<&ChatDraftLink>, mut chats: ResMut<Chats>) {
    if let Ok(ChatDraftLink(pr)) = links.get(activate.entity) {
        chats.entry(pr).show(PanelTab::Draft);
    }
}

/// The status bar's **Hide panel** / **Show panel**.
pub fn on_panel_toggle(
    activate: On<Activate>,
    buttons: Query<&PanelToggle>,
    mut chats: ResMut<Chats>,
) {
    if let Ok(PanelToggle(pr)) = buttons.get(activate.entity) {
        chats.entry(pr).toggle();
    }
}

fn on_send(
    activate: On<Activate>,
    buttons: Query<&ChatSend>,
    mut inputs: Query<(&ChatInput, &mut EditableText)>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut chats: ResMut<Chats>,
    mut asks: ResMut<Asks>,
) {
    let Ok(ChatSend(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some((_, mut editable)) = inputs.iter_mut().find(|(i, _)| &i.0 == pr) {
        let value = editable.value().to_string();
        if send_message(pr, &value, &tabs, &model, &mut chats, &mut asks) {
            set_text(&mut editable, "");
        }
    }
}

fn on_stop(activate: On<Activate>, buttons: Query<&ChatStop>, mut asks: ResMut<Asks>) {
    if let Ok(ChatStop(pr)) = buttons.get(activate.entity) {
        asks.send(Ask::AgentCancel { pr: pr.clone() });
    }
}

fn on_resize(drag: On<Pointer<Drag>>, edges: Query<&ChatResize>, mut chats: ResMut<Chats>) {
    if let Ok(ChatResize(pr)) = edges.get(drag.entity) {
        chats.entry(pr).resize(drag.event.delta.x);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{AgentTell, Ask, Connection, Model, Tell};
    use crate::fixture;
    use crate::nav::{Nav, Screen, Section};
    use crate::screens::review::agent::PanelTab;
    use crate::screens::review::shell::{HarnessButton, RightPanel};
    use crate::snapshot::Snapshot;
    use crate::testing::{self, NOW};
    use bevy::input::ButtonInput;
    use bevy::input_focus::{FocusCause, InputFocus};
    use bevy::text::EditableText;
    use bevy::ui::ComputedNode;
    use clusia_protocol::{AgentErrorKind, AgentLogEntry, SessionStateKind, Suggestion};

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    /// The demo review, with the Agent tab selected.
    fn open_chat() -> App {
        let mut snap = fixture::demo(NOW);
        snap.config.harness.program = Some("/opt/homebrew/bin/claude".into());
        let mut app = testing::app(snap);
        testing::open_ready(&mut app, false);
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .show(PanelTab::Agent);
        testing::settle(&mut app);
        testing::recorded(&mut app);
        app
    }

    fn say(app: &mut App, tell: AgentTell) {
        testing::tell(app, Tell::Agent(tell));
    }

    fn press(app: &mut App, keys: &[KeyCode]) {
        let mut input = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
        input.reset_all();
        for k in keys {
            input.press(*k);
        }
    }

    fn input(app: &mut App) -> Entity {
        testing::find::<ChatInput>(app, |i| i.0 == pr())
    }

    fn focus(app: &mut App, entity: Entity) {
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(entity, FocusCause::Navigated);
    }

    fn display_of<C: Component>(app: &mut App, want: impl Fn(&C) -> bool) -> Display {
        let e = testing::find::<C>(app, want);
        app.world().get::<Node>(e).unwrap().display
    }

    fn tab_of(app: &App) -> PanelTab {
        app.world().resource::<Chats>().0[&pr()].tab
    }

    fn suggestion() -> Suggestion {
        Suggestion {
            id: "sug-0123456789ab".into(),
            file: "src/auth/refresh.rs".into(),
            line: Some(44),
            start_line: None,
            end_line: None,
            body: "Re-check `expires_at` after taking the lock.".into(),
        }
    }

    #[test]
    fn the_column_has_agent_and_draft_tabs_and_starts_on_the_draft_without_a_harness() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        assert_eq!(testing::count::<PanelTabButton>(&mut app), 2);
        assert_eq!(tab_of(&app), PanelTab::Draft);
        assert_eq!(display_of::<AgentRegion>(&mut app, |_| true), Display::None);
        assert_eq!(display_of::<RightPanel>(&mut app, |_| true), Display::Flex);
        assert!(testing::shows(&mut app, "Agent"));
        assert!(testing::shows(&mut app, "Draft 3"));

        let agent = testing::find::<PanelTabButton>(&mut app, |b| b.tab == PanelTab::Agent);
        testing::activate(&mut app, agent);
        testing::settle(&mut app);
        assert_eq!(tab_of(&app), PanelTab::Agent);
        assert!(app.world().resource::<Chats>().0[&pr()].tab_set);
        assert_eq!(display_of::<AgentRegion>(&mut app, |_| true), Display::Flex);
        assert_eq!(display_of::<RightPanel>(&mut app, |_| true), Display::None);

        let draft = testing::find::<PanelTabButton>(&mut app, |b| b.tab == PanelTab::Draft);
        testing::activate(&mut app, draft);
        testing::settle(&mut app);
        assert_eq!(display_of::<RightPanel>(&mut app, |_| true), Display::Flex);
    }

    #[test]
    fn without_a_harness_the_agent_tab_says_it_is_not_set_up() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        let agent = testing::find::<PanelTabButton>(&mut app, |b| b.tab == PanelTab::Agent);
        testing::activate(&mut app, agent);
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "not set up"));
        assert!(!testing::shows(&mut app, "knows this review"));
        assert_eq!(testing::count::<HarnessButton>(&mut app), 1);
        assert!(testing::shows(&mut app, "Set up a harness"));
        let set_up = testing::find::<HarnessButton>(&mut app, |_| true);
        testing::activate(&mut app, set_up);
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Config(Section::Harness)
        );
    }

    #[test]
    fn a_set_up_harness_starts_on_the_agent_tab() {
        let mut snap = fixture::demo(NOW);
        snap.config.harness.program = Some("/opt/homebrew/bin/claude".into());
        let mut app = testing::app(snap);
        testing::open_ready(&mut app, false);
        assert_eq!(tab_of(&app), PanelTab::Agent);
        assert!(
            !app.world().resource::<Chats>().0[&pr()].tab_set,
            "the default is not a choice"
        );

        let mut found = Snapshot::default();
        assert!(!harness_ready(&found));
        found.first_run = Some(fixture::demo_first_run());
        assert!(harness_ready(&found), "claude was found on the PATH");
    }

    #[test]
    fn the_agent_speaking_selects_its_tab_unless_the_user_chose_the_draft() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        say(
            &mut app,
            AgentTell::Chunk {
                pr: pr(),
                turn: 1,
                text: "Summary of the pull request.".into(),
            },
        );
        testing::settle(&mut app);
        assert_eq!(tab_of(&app), PanelTab::Agent);
        assert!(testing::shows(&mut app, "Summary of the pull request."));

        let mut chosen = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut chosen, false);
        let draft = testing::find::<PanelTabButton>(&mut chosen, |b| b.tab == PanelTab::Draft);
        testing::activate(&mut chosen, draft);
        say(
            &mut chosen,
            AgentTell::Chunk {
                pr: pr(),
                turn: 1,
                text: "Summary.".into(),
            },
        );
        testing::settle(&mut chosen);
        assert_eq!(tab_of(&chosen), PanelTab::Draft);
    }

    #[test]
    fn the_draft_count_in_the_footer_opens_the_draft_tab() {
        let mut app = open_chat();
        let link = testing::find::<ChatDraftLink>(&mut app, |l| l.0 == pr());
        testing::activate(&mut app, link);
        testing::settle(&mut app);
        assert_eq!(tab_of(&app), PanelTab::Draft);
        assert_eq!(display_of::<RightPanel>(&mut app, |_| true), Display::Flex);
    }

    #[test]
    fn opening_a_review_asks_for_its_log_once() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::app_show_review(&mut app);
        let (view, _) = fixture::demo_review(NOW);
        testing::tell(
            &mut app,
            Tell::Opened {
                pr: pr(),
                view: Box::new(view),
                news: Vec::new(),
            },
        );
        testing::settle(&mut app);
        let logs: Vec<Ask> = testing::recorded(&mut app)
            .into_iter()
            .filter(|a| matches!(a, Ask::AgentLog { .. }))
            .collect();
        assert_eq!(logs, [Ask::AgentLog { pr: pr() }]);
        testing::settle(&mut app);
        assert!(
            testing::recorded(&mut app)
                .iter()
                .all(|a| !matches!(a, Ask::AgentLog { .. })),
            "asked once"
        );
    }

    #[test]
    fn the_log_fills_the_transcript() {
        let mut app = open_chat();
        testing::tell(
            &mut app,
            Tell::AgentLog {
                pr: pr(),
                entries: vec![
                    AgentLogEntry::User {
                        at: 1,
                        turn: 1,
                        text: "Is the new lock needed at all?".into(),
                    },
                    AgentLogEntry::Text {
                        at: 2,
                        turn: 1,
                        text: "The lock is needed: two refreshes would race.".into(),
                    },
                    AgentLogEntry::Done {
                        at: 3,
                        turn: 1,
                        duration_ms: 900,
                    },
                ],
            },
        );
        // The daemon tells the session's state after the log.
        say(
            &mut app,
            AgentTell::State {
                pr: pr(),
                state: SessionStateKind::Ready,
            },
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Is the new lock needed at all?"));
        assert!(testing::shows(&mut app, "two refreshes would race"));
        assert!(testing::shows(&mut app, "session resumed"));
        assert!(testing::shows(&mut app, "Claude Code · ready"));
    }

    #[test]
    fn every_line_type_is_drawn() {
        let mut app = open_chat();
        {
            let mut chats = app.world_mut().resource_mut::<Chats>();
            chats
                .0
                .get_mut(&pr())
                .unwrap()
                .push_me("Is the lock needed?".into());
        }
        for tell in [
            AgentTell::ToolUse {
                pr: pr(),
                turn: 1,
                summary: "Read src/auth/store.rs".into(),
            },
            AgentTell::Chunk {
                pr: pr(),
                turn: 1,
                text: "The lock is needed.\n\n```clusia-suggestion\n{\"file\":\"src/auth/refresh.rs\",\"line\":44,\"body\":\"x\"}\n```\n".into(),
            },
            AgentTell::Denied {
                pr: pr(),
                turn: 1,
                tool: "Bash".into(),
                detail: "cargo test".into(),
            },
            AgentTell::Suggestion {
                pr: pr(),
                turn: 1,
                suggestion: suggestion(),
            },
            AgentTell::Error {
                pr: pr(),
                turn: 1,
                kind: AgentErrorKind::UsageLimit,
                message: "You hit your limit".into(),
            },
        ] {
            say(&mut app, tell);
        }
        testing::settle(&mut app);
        for needle in [
            "Is the lock needed?",
            "Read src/auth/store.rs",
            "The lock is needed.",
            "Wanted to run `cargo test` — needs permission, coming soon",
            "Suggested comment",
            "refresh.rs:44",
            "Re-check",
            "You hit your limit",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert!(
            !testing::shows(&mut app, "clusia-suggestion"),
            "the block is hidden from the answer"
        );
    }

    #[test]
    fn other_tools_are_named_in_their_denied_line() {
        assert_eq!(
            denied_text("Bash", "cargo test"),
            "Wanted to run `cargo test` — needs permission, coming soon"
        );
        assert_eq!(
            denied_text("Edit", "src/lib.rs"),
            "Wanted to use Edit (src/lib.rs) — needs permission, coming soon"
        );
    }

    #[test]
    fn enter_sends_and_shift_enter_breaks_the_line() {
        let mut app = open_chat();
        let area = input(&mut app);
        testing::type_into(&mut app, area, "Is the lock needed?");
        focus(&mut app, area);
        press(&mut app, &[KeyCode::ShiftLeft, KeyCode::Enter]);
        app.update();
        testing::apply_edits(&mut app, area);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "Shift+Enter only breaks the line"
        );
        assert_eq!(
            app.world()
                .get::<EditableText>(area)
                .unwrap()
                .value()
                .to_string(),
            "Is the lock needed?\n"
        );
        testing::type_into(&mut app, area, "And the timeout?");
        press(&mut app, &[KeyCode::Enter]);
        app.update();
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AgentSend {
                pr: pr(),
                text: "Is the lock needed?\nAnd the timeout?".into()
            }]
        );
        assert_eq!(
            app.world()
                .get::<EditableText>(area)
                .unwrap()
                .value()
                .to_string(),
            "",
            "the box is empty again"
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "And the timeout?"));
    }

    #[test]
    fn the_send_button_sends_too_and_empty_text_sends_nothing() {
        let mut app = open_chat();
        let button = testing::find::<ChatSend>(&mut app, |_| true);
        testing::activate(&mut app, button);
        assert!(testing::recorded(&mut app).is_empty());
        let area = input(&mut app);
        testing::type_into(&mut app, area, "  Why a lock?  ");
        testing::activate(&mut app, button);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AgentSend {
                pr: pr(),
                text: "Why a lock?".into()
            }]
        );
    }

    #[test]
    fn nothing_is_sent_without_a_live_connection_and_the_text_stays() {
        let mut app = open_chat();
        app.world_mut().resource_mut::<Model>().connection = Connection::Lost("gone".into());
        let area = input(&mut app);
        testing::type_into(&mut app, area, "Why a lock?");
        focus(&mut app, area);
        press(&mut app, &[KeyCode::Enter]);
        app.update();
        // The headless app has no input plugin to end the press after one frame.
        press(&mut app, &[]);
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty());
        let refused = app.world().resource::<Chats>().0[&pr()]
            .lines
            .iter()
            .filter(|l| matches!(l, ChatLine::Error(_)))
            .count();
        assert_eq!(refused, 1, "one refusal for one Enter");
        assert_eq!(
            app.world()
                .get::<EditableText>(area)
                .unwrap()
                .value()
                .to_string(),
            "Why a lock?"
        );
        assert!(testing::shows(&mut app, "Not connected to clusiad"));
    }

    #[test]
    fn streaming_does_not_rebuild_the_input() {
        let mut app = open_chat();
        let area = input(&mut app);
        testing::type_into(&mut app, area, "a half-typed question");
        for i in 0..5 {
            say(
                &mut app,
                AgentTell::Chunk {
                    pr: pr(),
                    turn: 1,
                    text: format!("word{i} "),
                },
            );
        }
        testing::settle(&mut app);
        assert_eq!(input(&mut app), area, "the same input entity");
        assert_eq!(
            app.world()
                .get::<EditableText>(area)
                .unwrap()
                .value()
                .to_string(),
            "a half-typed question"
        );
        assert!(testing::shows(&mut app, "word4"));
    }

    #[test]
    fn stop_shows_while_the_agent_works_and_cancels() {
        let mut app = open_chat();
        assert_eq!(testing::count::<ChatStop>(&mut app), 0);
        say(
            &mut app,
            AgentTell::State {
                pr: pr(),
                state: SessionStateKind::Running,
            },
        );
        testing::settle(&mut app);
        let stop = testing::find::<ChatStop>(&mut app, |_| true);
        testing::activate(&mut app, stop);
        assert_eq!(testing::recorded(&mut app), [Ask::AgentCancel { pr: pr() }]);
        say(
            &mut app,
            AgentTell::State {
                pr: pr(),
                state: SessionStateKind::Ready,
            },
        );
        testing::settle(&mut app);
        assert_eq!(testing::count::<ChatStop>(&mut app), 0);
    }

    #[test]
    fn the_shortcut_and_the_status_bar_hide_and_show_the_column() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        assert_eq!(display_of::<PanelColumn>(&mut app, |_| true), Display::Flex);
        press(&mut app, &[KeyCode::SuperLeft, KeyCode::Backslash]);
        app.update();
        // The headless app has no input plugin to end the press after one frame.
        press(&mut app, &[]);
        testing::settle(&mut app);
        assert_eq!(display_of::<PanelColumn>(&mut app, |_| true), Display::None);
        assert!(testing::shows(&mut app, "Show panel"));
        let toggle = testing::find::<PanelToggle>(&mut app, |t| t.0 == pr());
        testing::activate(&mut app, toggle);
        testing::settle(&mut app);
        assert_eq!(display_of::<PanelColumn>(&mut app, |_| true), Display::Flex);
        assert!(testing::shows(&mut app, "Hide panel"));
    }

    #[test]
    fn the_footer_counts_the_draft_and_the_waiting_suggestions() {
        let mut app = open_chat();
        assert!(testing::shows(&mut app, "Draft · 3 items"));
        say(
            &mut app,
            AgentTell::Suggestion {
                pr: pr(),
                turn: 1,
                suggestion: suggestion(),
            },
        );
        testing::settle(&mut app);
        assert!(testing::shows(
            &mut app,
            "Draft · 3 items · 1 suggestion waiting"
        ));
        assert!(testing::shows(
            &mut app,
            "Can read the worktree · can't run commands yet"
        ));
    }

    #[test]
    fn the_header_leads_to_the_harness_settings() {
        let mut app = open_chat();
        let settings = testing::find::<HarnessButton>(&mut app, |_| true);
        testing::activate(&mut app, settings);
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Config(Section::Harness)
        );
    }

    #[test]
    fn the_status_bar_follows_the_session() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        assert!(testing::shows(&mut app, "Claude Code · no session yet"));
        for (state, text) in [
            (SessionStateKind::Running, "Claude Code · thinking…"),
            (
                SessionStateKind::Queued,
                "Claude Code · waiting for its turn",
            ),
            (SessionStateKind::Ready, "Claude Code · ready"),
        ] {
            say(&mut app, AgentTell::State { pr: pr(), state });
            testing::settle(&mut app);
            assert!(testing::shows(&mut app, text), "{text}");
        }
    }

    #[test]
    fn the_marks_the_chat_draws_exist_in_the_bundled_font() {
        for mark in ['✓', '⊘', '↑', '·', '…', '—', '–'] {
            assert!(crate::fonts::covers(crate::fonts::INTER, mark), "{mark}");
        }
    }

    #[test]
    fn the_chat_is_forgotten_with_its_tab() {
        let mut app = open_chat();
        app.world_mut()
            .resource_mut::<crate::nav::Nav>()
            .close_review(&pr());
        testing::settle(&mut app);
        assert!(app.world().resource::<Chats>().0.is_empty());
    }

    #[test]
    fn a_turn_running_when_the_log_was_read_asks_the_log_again_when_it_ends() {
        let mut app = open_chat();
        testing::tell(
            &mut app,
            Tell::AgentLog {
                pr: pr(),
                entries: vec![
                    AgentLogEntry::User {
                        at: 1,
                        turn: 1,
                        text: "Is the new lock needed at all?".into(),
                    },
                    AgentLogEntry::ToolUse {
                        at: 2,
                        turn: 1,
                        summary: "Read src/auth/store.rs".into(),
                    },
                ],
            },
        );
        say(
            &mut app,
            AgentTell::Chunk {
                pr: pr(),
                turn: 1,
                text: "the tail of the answer".into(),
            },
        );
        say(
            &mut app,
            AgentTell::Done {
                pr: pr(),
                turn: 1,
                duration_ms: 900,
            },
        );
        testing::settle(&mut app);
        assert_eq!(testing::recorded(&mut app), [Ask::AgentLog { pr: pr() }]);
    }

    fn transcript(app: &mut App) -> Entity {
        testing::find::<Transcript>(app, |t| t.pr == pr())
    }

    fn first_line(app: &mut App) -> Entity {
        let t = transcript(app);
        app.world().get::<Children>(t).unwrap()[0]
    }

    #[test]
    fn a_growing_answer_rebuilds_only_its_own_line() {
        let mut app = open_chat();
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .push_me("Is the lock needed?".into());
        say(
            &mut app,
            AgentTell::Chunk {
                pr: pr(),
                turn: 1,
                text: "The lock ".into(),
            },
        );
        testing::settle(&mut app);
        let question = first_line(&mut app);
        say(
            &mut app,
            AgentTell::Chunk {
                pr: pr(),
                turn: 1,
                text: "is needed.".into(),
            },
        );
        testing::settle(&mut app);
        assert_eq!(
            first_line(&mut app),
            question,
            "the question was not redrawn"
        );
        let t = transcript(&mut app);
        assert_eq!(app.world().get::<Children>(t).unwrap().len(), 2);
        assert!(testing::shows(&mut app, "The lock is needed."));
    }

    #[test]
    fn new_lines_follow_the_bottom_only_for_a_reader_already_there() {
        let mut app = open_chat();
        say(
            &mut app,
            AgentTell::Chunk {
                pr: pr(),
                turn: 1,
                text: "The lock ".into(),
            },
        );
        testing::settle(&mut app);
        let t = transcript(&mut app);
        assert_eq!(app.world().get::<ScrollPosition>(t).unwrap().y, f32::MAX);

        // The reader scrolled up to 100 of 800 px.
        app.world_mut().get_mut::<ScrollPosition>(t).unwrap().y = 100.0;
        let mut node = app.world_mut().get_mut::<ComputedNode>(t).unwrap();
        node.size = Vec2::new(400.0, 200.0);
        node.content_size = Vec2::new(400.0, 1000.0);
        node.scroll_position = Vec2::new(0.0, 100.0);
        say(
            &mut app,
            AgentTell::Chunk {
                pr: pr(),
                turn: 1,
                text: "is needed.".into(),
            },
        );
        testing::settle(&mut app);
        assert_eq!(app.world().get::<ScrollPosition>(t).unwrap().y, 100.0);

        // Back at the bottom, the next line is followed again.
        app.world_mut()
            .get_mut::<ComputedNode>(t)
            .unwrap()
            .scroll_position = Vec2::new(0.0, 800.0);
        say(
            &mut app,
            AgentTell::ToolUse {
                pr: pr(),
                turn: 1,
                summary: "Read src/auth/store.rs".into(),
            },
        );
        testing::settle(&mut app);
        assert_eq!(app.world().get::<ScrollPosition>(t).unwrap().y, f32::MAX);
    }

    #[test]
    fn a_harness_found_after_the_review_opened_still_picks_the_first_tab() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        assert_eq!(tab_of(&app), PanelTab::Draft);
        app.world_mut().resource_mut::<Model>().snapshot.first_run =
            Some(fixture::demo_first_run());
        testing::settle(&mut app);
        assert_eq!(tab_of(&app), PanelTab::Agent);
    }

    #[test]
    fn the_column_resizes_from_either_tab() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        assert_eq!(tab_of(&app), PanelTab::Draft);
        let edge = testing::find::<ChatResize>(&mut app, |r| r.0 == pr());
        let parent = app.world().get::<ChildOf>(edge).unwrap().parent();
        assert!(app.world().get::<PanelColumn>(parent).is_some());
    }
}
