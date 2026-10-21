//! The right column of a review: the Agent and Draft tabs, and the agent's chat (mockup
//! `AgentChat.png`).
//!
//! The shell spawns the column empty (`PanelColumn` with `PanelTabs`, the draft body and
//! `AgentRegion`); the first frame it is on screen `AgentRegion` gets a header, a scrolling
//! transcript and a footer with the input. Each part keeps the view it was built from and
//! rebuilds only when that changes, so streaming text never rebuilds the input.

use std::sync::Arc;
use std::time::Duration;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::Pickable;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::printable::printable;
use clusia_protocol::SessionStateKind;

use super::model::{ChatLine, Chats, DEFAULT_WIDTH, PanelTab, display_text};
use crate::bridge::{Ask, Asks, Model};
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen};
use crate::review_state::ReviewTabs;
use crate::screens::review::ReviewSystems;
use crate::screens::review::activity::elapsed_label;
use crate::screens::review::editor::read_only_reason;
use crate::screens::review::shell::{
    HarnessButton, OpenSetting, RightPanel, on_harness, on_open_setting,
};
use crate::snapshot::Snapshot;
use crate::theme::{Swatch, Theme};
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Icon, Stroke, Tone, Type, Variant, badge, button, icon_button,
    panel, text, wrapped_text,
};
use crate::ui::markdown::cache::parse_in;
use crate::ui::markdown::parse::{self, Block};
use crate::ui::markdown::{MdBody, RenderOpts, markdown_body};
use crate::ui::pace::{Pace, Wake};
use crate::ui::spinner::{Motion, beat, spinner};
use crate::ui::text_area::{Caret, growing_line_area, set_text};
use crate::ui::tween::{Ease, Look, Shroud, Tween, Veil, shroud, veil};

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
    /// A turn runs: the Agent tab carries a beating dot.
    working: bool,
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
    /// The agent is set up; without it the header leads to Config › Harness.
    ready: bool,
    /// The agent the review is pinned to.
    name: String,
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
    /// What each line child was drawn from: the line's stamp and whether it drew as the call
    /// that runs. `None` until the first draw.
    built: Option<Vec<(u64, bool)>>,
    /// The chat's `version` when the transcript was last brought up to date.
    version: u64,
    /// `Time::elapsed_secs_f64` of the last time the answer still arriving was redrawn.
    drawn_at: f64,
    /// The chat `version` whose redraw is waiting for `LIVE_REDRAW` to pass.
    held: Option<u64>,
    /// How many times the lines were diffed against the chat.
    #[cfg(test)]
    diffs: u32,
    /// The transcript ends with the live row (a turn runs).
    live: bool,
    /// `Time::elapsed_secs_f64` when this window first drew the live row of the running turn, for
    /// its elapsed time. The daemon does not tell when a turn began, so a turn already running
    /// when the window opened counts from then.
    since: Option<f64>,
    /// The last line's markdown column as drawn, when the last line is an answer.
    answer: Option<Answer>,
}

/// The markdown column of an answer and the blocks it was built from: its children are those
/// blocks, one each, in order.
struct Answer {
    body: Entity,
    blocks: Arc<Vec<Block>>,
    /// The options its blocks were drawn with, `cached` aside (it changes no node). A kept block
    /// drawn with other options would not match the rest of the column.
    opts: RenderOpts,
}

/// The column of the answer that a redraw replaces: what it shows and its children.
#[derive(Clone, Copy)]
struct Reuse<'a> {
    blocks: &'a [Block],
    children: &'a [Entity],
}

/// The last row of the transcript while a turn runs: a spinner, who works, for how long, and
/// **Stop**.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LiveRow(pub PrRef);

/// The elapsed time of the live row; `shown` is the whole seconds it says, so the text is
/// written once a second and not every frame.
#[derive(Component, Debug)]
struct LiveElapsed {
    since: f64,
    shown: Option<u64>,
}

#[derive(Component)]
struct Hints {
    pr: PrRef,
    built: Option<HintsView>,
}

/// The footer's **Allowed for this review** list.
#[derive(Component)]
pub struct RulesPart {
    pr: PrRef,
    built: Option<Vec<String>>,
}

/// × on a rule's chip.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RevokeButton {
    pub pr: PrRef,
    pub rule: String,
}

pub struct AgentPanel;

impl Plugin for AgentPanel {
    fn build(&self, app: &mut App) {
        app.add_systems(PreUpdate, super::name_chats.before(crate::bridge::pump));
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
                tick_elapsed,
                rebuild_hints,
                rebuild_rules,
                send_on_enter,
                placeholder_visibility,
            )
                .chain()
                .after(ReviewSystems),
        );
    }
}

/// Whether the agent `kind` (the one a review is pinned to) is set up: the configured program
/// serves it (it is the configured kind's), or the daemon's scan found it on the PATH (the first-run answer stands in only until the scan has arrived). The
/// configured program counts only for the configured kind: a review still pinned to the other
/// agent reads "not set up" until the scan has found that agent. A configured custom program
/// counts as the configured kind's.
pub fn harness_ready(snap: &Snapshot, kind: clusia_core::config::HarnessKind) -> bool {
    let found = |harnesses: &[clusia_protocol::Harness]| {
        harnesses
            .iter()
            .any(|h| clusia_core::config::HarnessKind::from(h.kind) == kind && h.path.is_some())
    };
    (snap.config.harness.program.is_some() && snap.config.harness.kind == kind)
        || snap.scanned().is_some_and(found)
}

/// The text the denied line shows: a tool the user's own `deny` rules refuse.
pub fn denied_text(tool: &str, detail: &str) -> String {
    let (tool, detail) = (printable(tool), printable(detail));
    if tool == "Bash" {
        format!("Wanted to run `{detail}` — not allowed")
    } else {
        format!("Wanted to use {tool} ({detail}) — not allowed")
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

/// Every review that is ready (not the cached copy) gets a chat, its first tab (Agent when the
/// agent the review is pinned to is set up, Draft otherwise), and its log is asked once. The
/// first tab follows the harness until the user picks a tab or the chat has a line: the agent
/// found on the PATH arrives with a scan that can end after the review opened.
fn open_chats(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut chats: ResMut<Chats>,
    mut asks: ResMut<Asks>,
) {
    // Every frame looks at every chat; only a write is a change.
    let mut written = false;
    let held = chats.bypass_change_detection();
    for (pr, tab) in &tabs.0 {
        let Some(ready) = tab.ready() else { continue };
        if ready.cached_at.is_some() {
            continue;
        }
        written |= !held.0.contains_key(pr);
        let chat = held.entry(pr);
        if !chat.tab_set && chat.lines.is_empty() {
            let first = if harness_ready(
                &model.snapshot,
                crate::agent_name::kind_of_tab(&model.snapshot, tab),
            ) {
                PanelTab::Agent
            } else {
                PanelTab::Draft
            };
            if chat.tab != first {
                chat.tab = first;
                written = true;
            }
        }
        if !chat.log_asked {
            chat.log_asked = true;
            written = true;
            asks.send(Ask::AgentLog { pr: pr.clone() });
        }
    }
    if written {
        chats.set_changed();
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

/// How long the column takes to slide in and out, and from how far right it comes.
pub const PANEL_IN_MS: u64 = 180;
pub const PANEL_OUT_MS: u64 = 120;
pub const PANEL_SLIDE: f32 = 24.0;

/// A column sliding out: it is hidden once its tween ends.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelClosing;

/// The column's visibility and width, and which tab's body is shown. Showing or hiding the
/// column slides it (its width is set at once, so it is laid out only when it appears and when
/// it goes); the first frame of a column and Reduce Motion just show or hide it.
fn layout(
    mut commands: Commands,
    chats: Res<Chats>,
    motion: Res<Motion>,
    mut columns: Query<(
        Entity,
        Ref<PanelColumn>,
        &mut Node,
        Has<PanelClosing>,
        Has<Tween>,
        &UiTransform,
    )>,
    layers: Query<
        (Entity, &ChildOf, Has<Shroud>, &BackgroundColor),
        Or<(With<Veil>, With<Shroud>)>,
    >,
    mut regions: Query<(&AgentRegion, &mut Node), Without<PanelColumn>>,
    mut drafts: Query<(&RightPanel, &mut Node), (Without<PanelColumn>, Without<AgentRegion>)>,
) {
    let tab_of = |pr: &PrRef| chats.0.get(pr).map_or(PanelTab::Draft, |c| c.tab);
    for (entity, column, mut node, closing, sliding, transform) in &mut columns {
        let (open, width) = chats
            .0
            .get(&column.pr)
            .map_or((true, DEFAULT_WIDTH), |c| (c.open, c.width));
        if node.width != px(width) {
            node.width = px(width);
        }
        let shown = node.display == Display::Flex;
        let still = column.is_added() || motion.reduced;
        let clear_layers = |commands: &mut Commands| {
            for (layer, child_of, ..) in &layers {
                if child_of.parent() == entity {
                    commands.entity(layer).despawn();
                }
            }
        };
        if open && (!shown || closing) {
            if !shown {
                node.display = Display::Flex;
            }
            commands.entity(entity).remove::<PanelClosing>();
            if still {
                clear_layers(&mut commands);
                continue;
            }
            // Shown again while it slides out, it turns back from where it is, and its shroud
            // clears from how thick it got.
            let from = match transform.translation.x {
                Val::Px(x) if closing => x,
                _ => PANEL_SLIDE,
            };
            commands.entity(entity).insert(Tween::new(
                PANEL_IN_MS,
                Ease::OutQuart,
                Look::REST.shifted(from, 0.0),
                Look::REST,
            ));
            let mut turned = false;
            for (layer, child_of, shrouded, color) in &layers {
                if child_of.parent() != entity {
                    continue;
                }
                if closing && shrouded && !turned {
                    turned = true;
                    let alpha = color.0.alpha();
                    commands.entity(layer).remove::<Shroud>().insert((
                        Veil,
                        Tween::new(
                            PANEL_IN_MS,
                            Ease::OutQuart,
                            Look::REST.faded(alpha),
                            Look::REST.faded(0.0),
                        ),
                    ));
                } else {
                    commands.entity(layer).despawn();
                }
            }
            if !turned {
                commands.spawn((
                    veil(Swatch::Bg, PANEL_IN_MS, Ease::OutQuart),
                    ChildOf(entity),
                ));
            }
        } else if !open && shown && !closing {
            if still {
                node.display = Display::None;
            } else {
                commands.entity(entity).insert((
                    PanelClosing,
                    Tween::new(
                        PANEL_OUT_MS,
                        Ease::InCubic,
                        Look::REST,
                        Look::REST.shifted(PANEL_SLIDE, 0.0),
                    ),
                ));
                commands.spawn((
                    shroud(Swatch::Bg, PANEL_OUT_MS, Ease::InCubic),
                    ChildOf(entity),
                ));
            }
        } else if !open && closing && !sliding {
            node.display = Display::None;
            clear_layers(&mut commands);
            commands.entity(entity).remove::<PanelClosing>();
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
            working: chat.busy(),
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
                        column_gap: px(6),
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
                ))
                .with_children(|t| {
                    t.spawn(text(
                        &fonts,
                        label,
                        if on { Type::STRONG } else { Type::MUTED },
                    ));
                    if want.working && tab == PanelTab::Agent {
                        t.spawn(beat());
                    }
                });
            }
            // The live row is on the Agent tab only; a turn can be stopped from either.
            if want.working {
                row.spawn(Node {
                    flex_grow: 1.0,
                    ..default()
                });
                row.spawn((
                    button(&fonts, "Stop", Variant::Ghost),
                    ChatStop(pr.clone()),
                    observe(on_stop),
                ))
                .entry::<Node>()
                .and_modify(|mut node| node.align_self = AlignSelf::Center);
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
                    flex_wrap: FlexWrap::Wrap,
                    row_gap: px(4),
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
                    version: 0,
                    held: None,
                    drawn_at: f64::NEG_INFINITY,
                    #[cfg(test)]
                    diffs: 0,
                    live: false,
                    since: None,
                    answer: None,
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
                display: Display::None,
                flex_direction: FlexDirection::Column,
                row_gap: px(6),
                ..default()
            },
            RulesPart {
                pr: pr.clone(),
                built: None,
            },
        ));
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
    tabs: Res<ReviewTabs>,
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
            ready: harness_ready(
                &model.snapshot,
                tabs.0
                    .get(&header.pr)
                    .map_or(model.snapshot.config.harness.kind, |t| {
                        crate::agent_name::kind_of_tab(&model.snapshot, t)
                    }),
            ),
            name: tabs.0.get(&header.pr).map_or_else(
                || crate::agent_name::active(&model.snapshot),
                |t| crate::agent_name::of_tab(&model.snapshot, t),
            ),
        };
        if header.built.as_ref() == Some(&want) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        if !want.ready {
            commands
                .entity(entity)
                .with_children(|h| not_set_up(h, &fonts, &want.name));
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
            h.spawn((
                text(&fonts, want.name.clone(), Type::STRONG),
                TextLayout::no_wrap(),
                Node {
                    flex_shrink: 0.0,
                    ..default()
                },
            ));
            // The status text takes the width left over and wraps under the name's row.
            h.spawn(Node {
                flex_grow: 1.0,
                flex_shrink: 1.0,
                flex_basis: px(0),
                min_width: px(60),
                ..default()
            })
            .with_children(|s| {
                s.spawn(wrapped_text(
                    &fonts,
                    if want.resumed {
                        "session resumed · knows this review"
                    } else {
                        "knows this review"
                    },
                    Type::META,
                ));
            });
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
fn not_set_up(h: &mut ChildSpawnerCommands, fonts: &UiFonts, name: &str) {
    h.spawn(text(fonts, name.to_string(), Type::STRONG));
    h.spawn(badge(fonts, "not set up", Tone::Neutral));
    h.spawn(Node {
        flex_grow: 1.0,
        ..default()
    });
    h.spawn((
        button(fonts, "Set up a harness", Variant::Secondary),
        OpenSetting("harness.program"),
        observe(on_open_setting),
    ));
}

/// The answer still arriving is parsed again no sooner than this many seconds after the last time.
const LIVE_REDRAW: f64 = 0.1;

fn rebuild_transcript(
    mut commands: Commands,
    chats: Res<Chats>,
    fonts: Res<UiFonts>,
    theme: Res<Theme>,
    model: Res<Model>,
    time: Res<Time>,
    mut pace: ResMut<Pace>,
    mut parts: Query<(
        Entity,
        &mut Transcript,
        &mut ScrollPosition,
        &ComputedNode,
        Option<&Children>,
    )>,
    bodies: Query<&Children, With<MdBody>>,
) {
    let opts = RenderOpts::from_config(theme.code_size, &model.snapshot.config);
    for (entity, mut part, mut scroll, node, children) in &mut parts {
        let Some(chat) = chats.0.get(&part.pr).filter(|c| c.tab == PanelTab::Agent) else {
            continue;
        };
        let live = chat.busy();
        if part.built.is_some() && part.version == chat.version() && part.live == live {
            continue;
        }
        let now = time.elapsed_secs_f64();
        // The redraw of the answer is held back and nothing came since: wake for its deadline.
        if part.held == Some(chat.version())
            && part.live == live
            && chat.streaming()
            && now - part.drawn_at < LIVE_REDRAW
        {
            pace.ask(Wake::steady(Duration::from_secs_f64(
                LIVE_REDRAW - (now - part.drawn_at),
            )));
            continue;
        }
        #[cfg(test)]
        {
            part.diffs += 1;
        }
        let wanted: Vec<(u64, bool)> = chat
            .lines
            .iter()
            .zip(chat.stamps())
            .enumerate()
            .map(|(i, (line, stamp))| {
                // Only the last line can be the call that is still running.
                let running =
                    live && i + 1 == chat.lines.len() && matches!(line, ChatLine::Tool(_));
                (*stamp, running)
            })
            .collect();
        let built = part.built.as_deref().unwrap_or(&[]);
        let same = built
            .iter()
            .zip(&wanted)
            .take_while(|(a, b)| a == b)
            .count();
        let drawn_lines = built.len();
        // Only the answer still arriving differs: it is parsed again at most `LIVE_REDRAW`
        // apart, and at once when it is whole.
        let growing = same + 1 == built.len()
            && built.len() == wanted.len()
            && part.live == live
            && live
            && chat.streaming()
            && matches!(chat.lines.last(), Some(ChatLine::Text(_)));
        if growing && now - part.drawn_at < LIVE_REDRAW {
            part.held = Some(chat.version());
            // A wake at the deadline: without one the loop may sleep with the tail unseen.
            pace.ask(Wake::steady(Duration::from_secs_f64(
                LIVE_REDRAW - (now - part.drawn_at),
            )));
            continue;
        }
        part.held = None;
        let pr = part.pr.clone();
        let follow = part.built.is_none() || at_bottom(node);
        let since = match (live, part.since) {
            (true, since) => since.unwrap_or(now),
            (false, _) => now,
        };
        // Each line is one child; the live row, when there was one, is the last.
        let kept = same.min(children.map_or(0, |c| c.len()));
        // The answer drawn last is drawn again: its new column takes over the children of the
        // leading blocks that still parse the same, so a growing answer spawns only its changed
        // tail and a selection in a finished block survives.
        let old = part.answer.take();
        let old_body = old.as_ref().map(|a| a.body);
        let reuse = old
            .as_ref()
            .filter(|a| {
                kept + 1 == drawn_lines
                    && children.and_then(|c| c.get(kept)) == Some(&a.body)
                    && a.opts == opts
            })
            .and_then(|a| {
                let drawn = bodies.get(a.body).map_or(&[][..], |c| &c[..]);
                (drawn.len() == a.blocks.len()).then_some(Reuse {
                    blocks: &a.blocks,
                    children: drawn,
                })
            });
        if let Some(children) = children {
            for child in &children[kept..] {
                // Despawned below, once the blocks it hands over have moved.
                if reuse.is_none() || Some(*child) != old_body {
                    commands.entity(*child).despawn();
                }
            }
        }
        let last = chat.lines.len().saturating_sub(1);
        // The answer still arriving is parsed with no cache: each of its prefixes would be an
        // entry never asked for again. Once whole, every later build of it uses the cache.
        let streaming = live && chat.streaming();
        let mut answer = None;
        commands.entity(entity).with_children(|t| {
            for (i, (line, (_, running))) in chat.lines.iter().zip(&wanted).enumerate().skip(kept) {
                let growing = streaming && i == last;
                let reuse = reuse.filter(|_| i == kept);
                let drawn = chat_line(t, &fonts, &pr, line, &opts, *running, growing, reuse);
                if i == last {
                    answer = drawn;
                }
            }
            if live {
                live_row(t, &fonts, &pr, &chat.agent_name(), since);
            }
        });
        if reuse.is_some()
            && let Some(body) = old_body
        {
            commands.entity(body).despawn();
        }
        // Every child from `kept` on was despawned: an answer among them is gone, even when no
        // line replaced it.
        let gone = old_body.is_some_and(|b| children.is_some_and(|c| c[kept..].contains(&b)));
        part.answer = if kept > last && !gone { old } else { answer };
        part.drawn_at = now;
        if follow {
            // The layout clamps this to the end of the content: the newest line stays in view.
            scroll.y = f32::MAX;
        }
        part.since = live.then_some(since);
        part.live = live;
        part.version = chat.version();
        part.built = Some(wanted);
    }
}

/// The row that ends the transcript while a turn runs.
fn live_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, name: &str, since: f64) {
    p.spawn((
        Node {
            align_items: AlignItems::Center,
            column_gap: px(8),
            flex_shrink: 0.0,
            ..default()
        },
        LiveRow(pr.clone()),
    ))
    .with_children(|r| {
        r.spawn(spinner());
        r.spawn(text(fonts, format!("{name} is working…"), Type::MUTED));
        r.spawn((
            text(fonts, elapsed_label(0), Type::META),
            LiveElapsed { since, shown: None },
        ));
        r.spawn(Node {
            flex_grow: 1.0,
            ..default()
        });
        r.spawn((
            button(fonts, "Stop", Variant::Ghost),
            ChatStop(pr.clone()),
            observe(on_stop),
        ));
    });
}

/// Writes each live row's elapsed time when its whole seconds change.
fn tick_elapsed(time: Res<Time>, mut rows: Query<(&mut Text, &mut LiveElapsed)>) {
    let now = time.elapsed_secs_f64();
    for (mut text, mut elapsed) in &mut rows {
        let secs = (now - elapsed.since).max(0.0) as u64;
        if elapsed.shown != Some(secs) {
            elapsed.shown = Some(secs);
            text.0 = elapsed_label(secs);
        }
    }
}

/// The view shows the end of its content (or has nothing to scroll), as of the last layout.
fn at_bottom(node: &ComputedNode) -> bool {
    let end = (node.content_size.y - node.size.y + node.scrollbar_size.y).max(0.0);
    node.scroll_position.y >= end.floor() - 1.0
}

/// Draws one line of the transcript; answers the markdown column of an answer.
#[allow(clippy::too_many_arguments)]
fn chat_line(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    line: &ChatLine,
    opts: &RenderOpts,
    running: bool,
    growing: bool,
    reuse: Option<Reuse>,
) -> Option<Answer> {
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
                b.spawn(wrapped_text(fonts, body.clone(), Type::BODY));
            });
        }
        ChatLine::Text(raw) => return answer_line(p, fonts, pr, raw, opts, growing, reuse),
        // A call is over once anything follows it; the last one of a running turn is not.
        ChatLine::Tool(summary) if running => working_pill(p, fonts, summary),
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
        ChatLine::Permission {
            tool,
            summary,
            outcome,
        } => {
            let (mark, went_through, body) =
                super::permission::permission_line(tool, summary, *outcome);
            if went_through {
                pill(
                    p,
                    fonts,
                    mark,
                    Swatch::Green,
                    &body,
                    Swatch::Chrome,
                    Swatch::Muted,
                );
            } else {
                pill(
                    p,
                    fonts,
                    mark,
                    Swatch::Orange,
                    &body,
                    Swatch::OrangeSoft,
                    Swatch::Orange,
                );
            }
        }
        ChatLine::Error(message) => {
            p.spawn(wrapped_text(
                fonts,
                message.clone(),
                Type::BODY.ink(Swatch::Orange),
            ));
        }
        ChatLine::Suggestion { suggestion, state } => {
            super::suggestion::card(p, fonts, pr, suggestion, *state, opts);
        }
    }
    None
}

/// Draws an answer as one markdown column. The children of `reuse` that show the same leading
/// blocks move into the new column, before the blocks spawned for the rest.
fn answer_line(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    raw: &str,
    opts: &RenderOpts,
    growing: bool,
    reuse: Option<Reuse>,
) -> Option<Answer> {
    let shown = display_text(raw);
    if shown.is_empty() {
        // Every line is one child, even one with nothing to show yet.
        p.spawn(Node {
            display: Display::None,
            ..default()
        });
        return None;
    }
    let drawn_with = RenderOpts {
        cached: true,
        ..opts.clone()
    };
    let uncached;
    let (blocks, opts) = if growing {
        uncached = RenderOpts {
            cached: false,
            ..opts.clone()
        };
        let blocks = parse::parse_in(&shown, &uncached.github_host, pr);
        (Arc::new(blocks), &uncached)
    } else {
        (parse_in(&shown, &opts.github_host, pr), opts)
    };
    let same = reuse.map_or(0, |r| {
        r.blocks
            .iter()
            .zip(blocks.iter())
            .take_while(|(a, b)| a == b)
            .count()
    });
    let body = markdown_body(p, fonts, &blocks[same..], opts);
    if let Some(r) = reuse
        && same > 0
    {
        p.commands_mut()
            .entity(body)
            .insert_children(0, &r.children[..same]);
    }
    Some(Answer {
        body,
        blocks,
        opts: drawn_with,
    })
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
    p.spawn(panel(pill_node(), fill)).with_children(|r| {
        r.spawn(text(fonts, mark.to_string(), Type::META.ink(mark_ink)));
        r.spawn(wrapped_text(fonts, body.to_string(), Type::META.ink(ink)));
    });
}

/// A tool call that has not ended: the chip of a finished one, with a spinner for its ✓.
fn working_pill(p: &mut ChildSpawnerCommands, fonts: &UiFonts, body: &str) {
    p.spawn(panel(pill_node(), Swatch::Chrome))
        .with_children(|r| {
            r.spawn(spinner());
            r.spawn(wrapped_text(
                fonts,
                body.to_string(),
                Type::META.ink(Swatch::Muted),
            ));
        });
}

fn pill_node() -> Node {
    Node {
        align_self: AlignSelf::FlexStart,
        max_width: percent(100),
        column_gap: px(6),
        align_items: AlignItems::FlexStart,
        padding: UiRect::axes(px(10), px(5)),
        border_radius: BorderRadius::all(px(14)),
        ..default()
    }
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
                children![wrapped_text(
                    &fonts,
                    "Can read the worktree · asks before running commands",
                    Type::META
                )],
            ));
        });
        hints.built = Some(want);
    }
}

fn rebuild_rules(
    mut commands: Commands,
    chats: Res<Chats>,
    fonts: Res<UiFonts>,
    mut parts: Query<(Entity, &mut RulesPart, &mut Node)>,
) {
    for (entity, mut part, mut node) in &mut parts {
        let Some(chat) = chats.0.get(&part.pr).filter(|c| c.tab == PanelTab::Agent) else {
            continue;
        };
        if part.built.as_ref() == Some(&chat.rules) {
            continue;
        }
        node.display = flex_if(!chat.rules.is_empty());
        part.built = Some(chat.rules.clone());
        commands.entity(entity).despawn_related::<Children>();
        if chat.rules.is_empty() {
            continue;
        }
        let pr = part.pr.clone();
        commands.entity(entity).with_children(|c| {
            c.spawn(text(&fonts, "Allowed for this review", Type::META));
            c.spawn(Node {
                flex_wrap: FlexWrap::Wrap,
                column_gap: px(6),
                row_gap: px(6),
                ..default()
            })
            .with_children(|chips| {
                for rule in &chat.rules {
                    chips
                        .spawn(panel(
                            Node {
                                column_gap: px(2),
                                align_items: AlignItems::Center,
                                padding: UiRect::new(px(10), px(4), px(3), px(3)),
                                border_radius: BorderRadius::all(px(12)),
                                ..default()
                            },
                            Swatch::Chrome,
                        ))
                        .with_children(|chip| {
                            chip.spawn(text(
                                &fonts,
                                super::permission::rule_label(rule),
                                Type::META,
                            ));
                            chip.spawn((
                                icon_button(Icon::Close, Variant::Ghost, 18.0),
                                RevokeButton {
                                    pr: pr.clone(),
                                    rule: rule.clone(),
                                },
                                observe(on_revoke),
                            ))
                            .entry::<Node>()
                            .and_modify(|mut node| {
                                node.align_self = AlignSelf::Center;
                                node.border_radius = BorderRadius::all(px(8));
                            });
                        });
                }
            });
        });
    }
}

/// × takes the rule back: its chip goes at once, and the daemon's `RulesChanged` confirms.
fn on_revoke(
    activate: On<Activate>,
    buttons: Query<&RevokeButton>,
    mut chats: ResMut<Chats>,
    mut asks: ResMut<Asks>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(chat) = chats.0.get_mut(&b.pr) {
        chat.rules.retain(|r| r != &b.rule);
    }
    asks.send(Ask::RevokeRule {
        pr: b.pr.clone(),
        rule: b.rule.clone(),
    });
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
    use crate::screens::review::shell::{FinalizeButton, HarnessButton, RightPanel};
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

    #[test]
    fn the_name_keeps_one_line_and_the_toggle_sits_before_finalize() {
        let mut app = open_chat();
        testing::settle(&mut app);
        let world = app.world_mut();
        let mut names = world.query::<(&Text, &TextLayout)>();
        let (_, layout) = names
            .iter(world)
            .find(|(t, _)| t.0 == "Claude Code")
            .expect("the header name");
        assert_eq!(layout.linebreak, LineBreak::NoWrap);

        let toggle = testing::find::<PanelToggle>(&mut app, |_| true);
        let finalize = testing::find::<FinalizeButton>(&mut app, |_| true);
        let world = app.world();
        let parent = world.get::<ChildOf>(toggle).unwrap().parent();
        assert_eq!(world.get::<ChildOf>(finalize).unwrap().parent(), parent);
        let siblings = world.get::<Children>(parent).unwrap();
        let at = |e: Entity| siblings.iter().position(|c| c == e).unwrap();
        assert_eq!(at(toggle) + 1, at(finalize));
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
    fn an_error_in_the_frame_the_pin_changes_is_worded_for_the_new_agent() {
        use clusia_core::config::HarnessKind as Kind;
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        let pr = pr();
        assert_eq!(
            app.world().resource::<Chats>().0[&pr].kind,
            Kind::ClaudeCode
        );
        match &mut app
            .world_mut()
            .resource_mut::<crate::review_state::ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .phase
        {
            crate::review_state::Phase::Ready(ready) => {
                ready.view.review.harness_session = Some("thr-1".into());
                ready.view.review.harness_session_kind = Some(Kind::Codex);
            }
            other => panic!("not ready: {other:?}"),
        }
        testing::tell(
            &mut app,
            Tell::Agent(AgentTell::Error {
                pr: pr.clone(),
                turn: 1,
                kind: clusia_protocol::AgentErrorKind::NotSignedIn,
                message: String::new(),
            }),
        );
        let chat = &app.world().resource::<Chats>().0[&pr];
        assert!(
            chat.lines.iter().any(|l| matches!(
                l,
                crate::screens::review::agent::ChatLine::Error(m) if m.contains("codex login")
            )),
            "{:?}",
            chat.lines
        );
    }

    #[test]
    fn a_first_run_harness_counts_only_when_it_is_the_configured_kind() {
        use clusia_core::config::HarnessKind as Kind;
        use clusia_protocol::HarnessKind;
        let found = |kind| clusia_protocol::FirstRun {
            github: clusia_protocol::GithubLogin::SignedOut,
            folders: Vec::new(),
            harnesses: vec![clusia_protocol::Harness {
                kind,
                path: Some("/opt/homebrew/bin/x".into()),
                version: None,
            }],
        };
        let mut snap = Snapshot {
            first_run: Some(found(HarnessKind::Codex)),
            ..Snapshot::default()
        };
        assert!(
            !harness_ready(&snap, Kind::ClaudeCode),
            "Claude Code was not found, only Codex"
        );
        assert!(harness_ready(&snap, Kind::Codex));
        snap.first_run = Some(found(HarnessKind::ClaudeCode));
        assert!(!harness_ready(&snap, Kind::Codex));
        assert!(harness_ready(&snap, Kind::ClaudeCode));
    }

    #[test]
    fn the_scan_after_the_first_run_makes_a_path_agent_ready() {
        use clusia_core::config::HarnessKind as Kind;
        let mut snap = Snapshot::default();
        assert!(snap.config.harness.program.is_none() && snap.first_run.is_none());
        assert!(!harness_ready(&snap, Kind::ClaudeCode), "nothing scanned");
        snap.harnesses = Some(Vec::new());
        assert!(
            !harness_ready(&snap, Kind::ClaudeCode),
            "the scan found nothing"
        );
        snap.harnesses = Some(fixture::demo_first_run().harnesses);
        assert!(harness_ready(&snap, Kind::ClaudeCode));
        assert!(!harness_ready(&snap, Kind::Codex));
    }

    #[test]
    fn the_newer_scan_overrides_the_first_run_answer() {
        use clusia_core::config::HarnessKind as Kind;
        let mut snap = Snapshot {
            first_run: Some(fixture::demo_first_run()),
            ..Snapshot::default()
        };
        assert!(
            harness_ready(&snap, Kind::ClaudeCode),
            "only the first run so far"
        );
        snap.harnesses = Some(Vec::new());
        assert!(
            !harness_ready(&snap, Kind::ClaudeCode),
            "the scan after the first run found nothing"
        );
    }

    #[test]
    fn a_program_only_serves_the_configured_kind() {
        use clusia_core::config::HarnessKind as Kind;
        let mut snap = Snapshot::default();
        snap.config.harness.program = Some("/opt/homebrew/bin/claude".into());
        assert!(harness_ready(&snap, Kind::ClaudeCode));
        assert!(
            !harness_ready(&snap, Kind::Codex),
            "a review pinned to Codex is not served by Claude Code's program"
        );
    }

    #[test]
    fn a_configured_custom_program_makes_the_agent_ready() {
        use clusia_core::config::HarnessKind as Kind;
        let mut snap = Snapshot::default();
        snap.config.harness.kind = Kind::Custom;
        assert!(
            !harness_ready(&snap, Kind::Custom),
            "no program: a custom command is not set up"
        );
        snap.config.harness.program = Some("/usr/local/bin/echo-harness".into());
        assert!(harness_ready(&snap, Kind::Custom));
        assert!(
            !harness_ready(&snap, Kind::Codex),
            "a custom program does not serve Codex"
        );
    }

    #[test]
    fn the_header_of_an_unset_custom_command_names_it() {
        let mut snap = fixture::demo(NOW);
        snap.config.harness.kind = clusia_core::config::HarnessKind::Custom;
        snap.config.harness.custom_name = "Echo".into();
        let mut app = testing::app(snap);
        testing::open_ready(&mut app, false);
        let agent = testing::find::<PanelTabButton>(&mut app, |b| b.tab == PanelTab::Agent);
        testing::activate(&mut app, agent);
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Echo"));
        assert!(testing::shows(&mut app, "not set up"));
        assert!(!testing::shows(&mut app, "Claude Code"));
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
        assert_eq!(
            testing::count::<crate::screens::review::shell::OpenSetting>(&mut app),
            1
        );
        assert!(testing::shows(&mut app, "Set up a harness"));
        testing::open_setting(&mut app, "harness.program");
        assert_eq!(
            testing::config_focus(&app),
            (Screen::Config(Section::Harness), Some("harness.program"))
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
        assert!(!harness_ready(
            &found,
            clusia_core::config::HarnessKind::ClaudeCode
        ));
        found.first_run = Some(fixture::demo_first_run());
        assert!(
            harness_ready(&found, clusia_core::config::HarnessKind::ClaudeCode),
            "claude was found on the PATH"
        );
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
            "Wanted to run `cargo test` — not allowed",
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
            "Wanted to run `cargo test` — not allowed"
        );
        assert_eq!(
            denied_text("Edit", "src/lib.rs"),
            "Wanted to use Edit (src/lib.rs) — not allowed"
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

    fn column(app: &mut App) -> Entity {
        testing::find::<PanelColumn>(app, |_| true)
    }

    fn shift_of(app: &App, e: Entity) -> Val2 {
        app.world().get::<UiTransform>(e).unwrap().translation
    }

    fn shrouds_over(app: &mut App, parent: Entity) -> usize {
        let mut q = app.world_mut().query_filtered::<&ChildOf, With<Shroud>>();
        q.iter(app.world()).filter(|c| c.parent() == parent).count()
    }

    #[test]
    fn the_column_slides_out_then_hides_and_slides_back_in() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        testing::run_tweens(&mut app);
        let col = column(&mut app);
        assert!(
            app.world().get::<Tween>(col).is_none(),
            "a new column just shows"
        );
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .toggle();
        app.update();
        assert_eq!(display_of::<PanelColumn>(&mut app, |_| true), Display::Flex);
        assert!(app.world().get::<Tween>(col).is_some());
        assert_eq!(shrouds_over(&mut app, col), 1);
        app.update();
        let x = shift_of(&app, col).x;
        assert!(x != Val::Px(0.0) && x != Val::Px(PANEL_SLIDE), "{x:?}");
        let frames = testing::run_tweens(&mut app);
        assert!(frames * 16 <= PANEL_OUT_MS as usize + 48, "{frames} frames");
        assert_eq!(display_of::<PanelColumn>(&mut app, |_| true), Display::None);
        assert!(app.world().get::<PanelClosing>(col).is_none());
        assert_eq!(shrouds_over(&mut app, col), 0);

        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .toggle();
        app.update();
        assert_eq!(display_of::<PanelColumn>(&mut app, |_| true), Display::Flex);
        assert_eq!(shift_of(&app, col), Val2::px(PANEL_SLIDE, 0.0));
        assert!(testing::veil_over(&mut app, col).is_some());
        testing::run_tweens(&mut app);
        assert_eq!(shift_of(&app, col), Val2::ZERO);
        assert!(testing::veil_over(&mut app, col).is_none());
        assert_eq!(testing::count::<Tween>(&mut app), 0);
    }

    #[test]
    fn a_column_shown_again_while_it_slides_out_comes_back() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        testing::run_tweens(&mut app);
        let col = column(&mut app);
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .toggle();
        app.update();
        app.update();
        app.update();
        let Val::Px(gone) = shift_of(&app, col).x else {
            panic!("a shift in pixels")
        };
        let shroud = {
            let mut q = app
                .world_mut()
                .query_filtered::<&BackgroundColor, With<Shroud>>();
            q.single(app.world()).unwrap().0.alpha()
        };
        assert!(
            gone > 0.0 && shroud > 0.0 && shroud < 1.0,
            "{gone} {shroud}"
        );
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .toggle();
        app.update();
        assert_eq!(shrouds_over(&mut app, col), 0);
        assert!(app.world().get::<PanelClosing>(col).is_none());
        // It turns back from where it was, under a layer no thicker than it was: no jump to the
        // far edge and no blink to the background.
        assert_eq!(shift_of(&app, col), Val2::px(gone, 0.0));
        let layer = testing::veil_over(&mut app, col).expect("a layer clearing");
        let alpha = app.world().get::<BackgroundColor>(layer).unwrap().0.alpha();
        assert!(alpha <= shroud + 1e-4, "{alpha} over {shroud}");
        testing::run_tweens(&mut app);
        assert_eq!(display_of::<PanelColumn>(&mut app, |_| true), Display::Flex);
        assert_eq!(shift_of(&app, col), Val2::ZERO);
    }

    #[test]
    fn with_reduce_motion_the_column_hides_and_shows_at_once() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        app.insert_resource(Motion { reduced: true });
        testing::run_tweens(&mut app);
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .toggle();
        app.update();
        assert_eq!(display_of::<PanelColumn>(&mut app, |_| true), Display::None);
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .toggle();
        app.update();
        assert_eq!(display_of::<PanelColumn>(&mut app, |_| true), Display::Flex);
        assert_eq!(testing::count::<Tween>(&mut app), 0);
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
        // The column slides out before it is hidden.
        testing::run_tweens(&mut app);
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
            "Can read the worktree · asks before running commands"
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
        for mark in ['✓', '⊘', '↑', '·', '…', '—', '–', '×', '“', '”'] {
            assert!(crate::fonts::covers(crate::fonts::INTER, mark), "{mark}");
        }
    }

    fn run_state(app: &mut App, state: SessionStateKind) {
        say(app, AgentTell::State { pr: pr(), state });
        testing::settle(app);
    }

    /// Some line under `root` is a finished call's tick.
    fn ticks_in(app: &mut App, root: Entity) -> bool {
        let mut stack = vec![root];
        while let Some(e) = stack.pop() {
            if app.world().get::<Text>(e).is_some_and(|t| t.0 == "✓") {
                return true;
            }
            if let Some(children) = app.world().get::<Children>(e) {
                stack.extend(children.iter());
            }
        }
        false
    }

    fn live_rows(app: &mut App) -> usize {
        testing::count::<LiveRow>(app)
    }

    fn marks_of<C: Component>(app: &mut App, want: impl Fn(&C) -> bool) -> usize {
        let e = testing::find::<C>(app, want);
        testing::marks_in(app, e)
    }

    #[test]
    fn a_running_turn_ends_the_transcript_with_a_live_row_that_goes_with_the_turn() {
        let mut app = open_chat();
        assert_eq!(live_rows(&mut app), 0);
        run_state(&mut app, SessionStateKind::Running);
        assert_eq!(live_rows(&mut app), 1);
        assert!(testing::shows(&mut app, "Claude Code is working…"));
        assert_eq!(marks_of::<LiveRow>(&mut app, |_| true), 1, "its spinner");
        let row = testing::find::<LiveRow>(&mut app, |_| true);
        let transcript = app.world().get::<ChildOf>(row).unwrap().parent();
        assert_eq!(
            app.world().get::<Children>(transcript).unwrap().last(),
            Some(&row),
            "the last row"
        );
        // A turn that waits for its turn is not over either.
        run_state(&mut app, SessionStateKind::Queued);
        assert_eq!(live_rows(&mut app), 1);
        run_state(&mut app, SessionStateKind::Ready);
        assert_eq!(live_rows(&mut app), 0, "the turn ended");
    }

    #[test]
    fn a_failed_or_stopped_turn_takes_the_live_row_away() {
        for end in ["failed", "stopped"] {
            let mut app = open_chat();
            run_state(&mut app, SessionStateKind::Running);
            assert_eq!(live_rows(&mut app), 1);
            if end == "failed" {
                say(
                    &mut app,
                    AgentTell::Error {
                        pr: pr(),
                        turn: 1,
                        kind: AgentErrorKind::Crashed,
                        message: "It stopped".into(),
                    },
                );
            } else {
                let stop = testing::find::<ChatStop>(&mut app, |_| true);
                testing::activate(&mut app, stop);
                assert_eq!(testing::recorded(&mut app), [Ask::AgentCancel { pr: pr() }]);
            }
            run_state(&mut app, SessionStateKind::Ready);
            assert_eq!(live_rows(&mut app), 0, "{end}");
            assert_eq!(testing::count::<ChatStop>(&mut app), 0, "{end}");
        }
    }

    #[test]
    fn the_live_row_stays_last_while_the_answer_streams() {
        let mut app = open_chat();
        run_state(&mut app, SessionStateKind::Running);
        for text in ["The ", "lock ", "is needed."] {
            say(
                &mut app,
                AgentTell::Chunk {
                    pr: pr(),
                    turn: 1,
                    text: text.into(),
                },
            );
            testing::settle(&mut app);
        }
        assert_eq!(live_rows(&mut app), 1, "one row, not one per chunk");
        // The answer is redrawn at most ten times a second: let the pace pass.
        for _ in 0..8 {
            app.update();
        }
        assert!(testing::shows(&mut app, "The lock is needed."));
        let row = testing::find::<LiveRow>(&mut app, |_| true);
        let transcript = app.world().get::<ChildOf>(row).unwrap().parent();
        let children = app.world().get::<Children>(transcript).unwrap();
        assert_eq!(children.len(), 2, "the answer and the row");
        assert_eq!(children.last(), Some(&row));
    }

    #[test]
    fn the_call_that_runs_shows_a_spinner_in_place_of_its_tick() {
        let mut app = open_chat();
        run_state(&mut app, SessionStateKind::Running);
        say(
            &mut app,
            AgentTell::ToolUse {
                pr: pr(),
                turn: 1,
                summary: "Read src/auth/refresh.rs".into(),
            },
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Read src/auth/refresh.rs"));
        let transcript = testing::find::<Transcript>(&mut app, |_| true);
        assert!(!ticks_in(&mut app, transcript), "still running");
        assert_eq!(
            testing::marks_in(&app, transcript),
            2,
            "the call and the row"
        );
        // The next thing the agent does ends the call.
        say(
            &mut app,
            AgentTell::ToolUse {
                pr: pr(),
                turn: 1,
                summary: "Read src/auth/store.rs".into(),
            },
        );
        testing::settle(&mut app);
        assert!(ticks_in(&mut app, transcript), "the first call is over");
        assert_eq!(
            testing::marks_in(&app, transcript),
            2,
            "the second call and the row"
        );
        // So does the end of the turn.
        say(
            &mut app,
            AgentTell::Done {
                pr: pr(),
                turn: 1,
                duration_ms: 900,
            },
        );
        run_state(&mut app, SessionStateKind::Ready);
        assert_eq!(testing::marks_in(&app, transcript), 0);
    }

    #[test]
    fn the_live_rows_elapsed_text_follows_the_clock_in_whole_seconds() {
        use bevy::time::TimeUpdateStrategy;
        use std::time::Duration;
        let mut app = open_chat();
        run_state(&mut app, SessionStateKind::Running);
        assert!(testing::shows(&mut app, "0s"));
        // `Time` hands a frame at most 250 ms.
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            250,
        )));
        for _ in 0..24 {
            app.update();
        }
        let now = app.world().resource::<Time>().elapsed_secs_f64();
        let e = testing::find::<LiveElapsed>(&mut app, |_| true);
        let since = app.world().get::<LiveElapsed>(e).unwrap().since;
        let want = (now - since) as u64;
        assert!((5..=6).contains(&want), "about 6 s passed: {want}");
        assert_eq!(app.world().get::<LiveElapsed>(e).unwrap().shown, Some(want));
        assert_eq!(app.world().get::<Text>(e).unwrap().0, elapsed_label(want));
        assert_eq!(live_rows(&mut app), 1, "the row was not rebuilt to say it");
    }

    #[test]
    fn stop_is_in_reach_on_the_draft_tab_too() {
        let mut app = open_chat();
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .show(PanelTab::Draft);
        run_state(&mut app, SessionStateKind::Running);
        let tabs = testing::find::<PanelTabs>(&mut app, |_| true);
        let stop = testing::find::<ChatStop>(&mut app, |s| s.0 == pr());
        assert!(
            is_under(&app, stop, tabs),
            "the tab strip holds a Stop whatever tab is shown"
        );
        testing::activate(&mut app, stop);
        assert_eq!(testing::recorded(&mut app), [Ask::AgentCancel { pr: pr() }]);
        run_state(&mut app, SessionStateKind::Ready);
        assert_eq!(testing::count::<ChatStop>(&mut app), 0);
    }

    fn is_under(app: &App, mut e: Entity, ancestor: Entity) -> bool {
        while let Some(parent) = app.world().get::<ChildOf>(e) {
            e = parent.parent();
            if e == ancestor {
                return true;
            }
        }
        false
    }

    #[test]
    fn the_agent_tab_beats_while_a_turn_runs() {
        let mut app = open_chat();
        let agent = |t: &PanelTabButton| t.tab == PanelTab::Agent;
        let draft = |t: &PanelTabButton| t.tab == PanelTab::Draft;
        assert_eq!(marks_of(&mut app, agent), 0);
        run_state(&mut app, SessionStateKind::Running);
        assert_eq!(marks_of(&mut app, agent), 1);
        assert_eq!(marks_of(&mut app, draft), 0, "only the Agent tab");
        // The dot is there on the other tab as well: the turn goes on.
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .show(PanelTab::Draft);
        testing::settle(&mut app);
        assert_eq!(marks_of(&mut app, agent), 1);
        run_state(&mut app, SessionStateKind::Ready);
        assert_eq!(marks_of(&mut app, agent), 0);
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

    fn lines_of(app: &mut App) -> Vec<Entity> {
        let t = transcript(app);
        app.world().get::<Children>(t).unwrap().to_vec()
    }

    fn chunk_of(app: &mut App, text: &str) {
        say(
            app,
            AgentTell::Chunk {
                pr: pr(),
                turn: 1,
                text: text.into(),
            },
        );
    }

    #[test]
    fn the_answer_still_arriving_stays_out_of_the_caches() {
        use crate::ui::markdown::cache::counts;
        let marker = "streamprobe";
        let code = "let streamprobe_a = 1;\nlet streamprobe_b = 2;";
        let mut app = open_chat();
        run_state(&mut app, SessionStateKind::Running);
        let chunks = [
            "Use a ",
            "streamprobe lock.\n\n",
            "```rust\nlet ",
            "streamprobe_a = 1;\n",
            "let streamprobe_b = 2;\n",
            "```\n",
        ];
        let mut raw = String::new();
        for chunk in chunks {
            raw.push_str(chunk);
            chunk_of(&mut app, chunk);
            // Past the ten-a-second pace, so every prefix is drawn.
            for _ in 0..8 {
                app.update();
            }
            assert_eq!(counts::cached_containing(marker), (0, 0), "after {chunk:?}");
        }
        assert!(testing::shows(&mut app, "Use a streamprobe lock."));
        assert!(
            counts::highlights(code) >= 1,
            "the growing code was highlighted"
        );
        say(
            &mut app,
            AgentTell::Done {
                pr: pr(),
                turn: 1,
                duration_ms: 900,
            },
        );
        run_state(&mut app, SessionStateKind::Ready);
        testing::settle(&mut app);
        // A build of the whole answer goes through the caches.
        let t = transcript(&mut app);
        app.world_mut().get_mut::<Transcript>(t).unwrap().built = None;
        testing::settle(&mut app);
        assert_eq!(
            counts::cached_containing(marker),
            (1, 1),
            "the whole answer only"
        );
        let whole = display_text(&raw);
        let (parses, highlights) = (counts::parses(&whole), counts::highlights(code));
        app.world_mut().get_mut::<Transcript>(t).unwrap().built = None;
        testing::settle(&mut app);
        assert_eq!(counts::parses(&whole), parses, "served from the cache");
        assert_eq!(counts::highlights(code), highlights);
    }

    #[test]
    fn a_rebuilt_transcript_parses_no_line_again() {
        use crate::ui::markdown::cache::counts;
        let mut app = open_chat();
        let answer = "Chat respawn probe: the **lock** is needed.";
        chunk_of(&mut app, answer);
        testing::settle(&mut app);
        let parses = counts::parses(answer);
        assert!(parses >= 1, "the answer was built");
        let before = lines_of(&mut app);
        // As a panel opened again: nothing of the transcript is built yet.
        let t = transcript(&mut app);
        app.world_mut().get_mut::<Transcript>(t).unwrap().built = None;
        testing::settle(&mut app);
        let after = lines_of(&mut app);
        assert!(
            !after.is_empty() && after != before,
            "the lines were respawned"
        );
        assert_eq!(counts::parses(answer), parses, "no parse on a respawn");
    }

    #[test]
    fn frames_with_nothing_new_do_not_even_diff_the_transcript() {
        let mut app = open_chat();
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .push_me("Is the lock needed?".into());
        chunk_of(&mut app, "Yes.");
        testing::settle(&mut app);
        let t = transcript(&mut app);
        let before = lines_of(&mut app);
        let passes = app.world().get::<Transcript>(t).unwrap().diffs;
        assert!(passes > 0, "the chat was diffed while it changed");
        for _ in 0..30 {
            app.update();
        }
        assert_eq!(app.world().get::<Transcript>(t).unwrap().diffs, passes);
        assert_eq!(lines_of(&mut app), before);
    }

    #[test]
    fn a_new_line_is_appended_without_respawning_the_earlier_ones() {
        let mut app = open_chat();
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .push_me("Is the lock needed?".into());
        chunk_of(&mut app, "Let me look.");
        testing::settle(&mut app);
        let before = lines_of(&mut app);
        say(
            &mut app,
            AgentTell::ToolUse {
                pr: pr(),
                turn: 1,
                summary: "Read src/auth/store.rs".into(),
            },
        );
        testing::settle(&mut app);
        let after = lines_of(&mut app);
        assert_eq!(after.len(), 3);
        assert_eq!(after[..2], before[..2], "the question and the answer stay");
    }

    #[test]
    fn a_streaming_answer_is_reparsed_at_most_ten_times_a_second_and_ends_exact() {
        let mut app = open_chat();
        run_state(&mut app, SessionStateKind::Running);
        let mut seen = std::collections::HashSet::new();
        let mut want = String::new();
        // Two frames of 16 ms per chunk: 30 chunks in about a second.
        for i in 0..30 {
            let word = format!("word{i} ");
            want.push_str(&word);
            chunk_of(&mut app, &word);
            app.update();
            seen.extend(lines_of(&mut app).first().copied());
        }
        assert!(seen.len() <= 11, "{} parses in 30 chunks", seen.len());
        assert!(seen.len() >= 2, "it still moves");
        // The turn ends before another tick of the throttle: the tail is not lost.
        say(
            &mut app,
            AgentTell::Done {
                pr: pr(),
                turn: 1,
                duration_ms: 1000,
            },
        );
        run_state(&mut app, SessionStateKind::Ready);
        assert!(testing::shows(&mut app, want.trim()), "{want}");
    }

    #[test]
    fn a_selection_in_an_earlier_line_survives_a_new_line_and_a_chunk() {
        use crate::ui::code_select::{At, CodeLine, CodeSelection};
        let mut app = open_chat();
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .push_me("Show it".into());
        chunk_of(&mut app, "```rust\nlet a = 1;\n```");
        testing::settle(&mut app);
        let t = transcript(&mut app);
        let block: Vec<(Entity, Entity)> = {
            let world = app.world_mut();
            let mut lines = world.query::<(Entity, &CodeLine)>();
            lines.iter(world).map(|(e, l)| (e, l.key.group)).collect()
        };
        let (_, group) = *block
            .iter()
            .find(|(e, _)| is_under(&app, *e, t))
            .expect("the code block is in the transcript");
        let at = |offset| At { order: 0, offset };
        app.world_mut()
            .resource_mut::<CodeSelection>()
            .select(group, 0, at(0), at(3));
        testing::settle(&mut app);
        assert!(!app.world().resource::<CodeSelection>().is_empty());
        say(
            &mut app,
            AgentTell::ToolUse {
                pr: pr(),
                turn: 1,
                summary: "Read src/auth/store.rs".into(),
            },
        );
        chunk_of(&mut app, "The ");
        testing::settle(&mut app);
        chunk_of(&mut app, "lock.");
        testing::settle(&mut app);
        assert!(
            !app.world().resource::<CodeSelection>().is_empty(),
            "the selected block was not respawned"
        );
    }

    /// Every entity under `root`, `root` included.
    fn tree_of(app: &App, root: Entity) -> std::collections::HashSet<Entity> {
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![root];
        while let Some(e) = stack.pop() {
            seen.insert(e);
            if let Some(children) = app.world().get::<Children>(e) {
                stack.extend(children.iter());
            }
        }
        seen
    }

    /// The block children of the answer line, the first line of the transcript.
    fn answer_blocks(app: &mut App) -> Vec<Entity> {
        let answer = first_line(app);
        app.world()
            .get::<Children>(answer)
            .map(|c| c.to_vec())
            .unwrap_or_default()
    }

    /// Ends the running turn and checks that the answer line is what a build of the whole
    /// answer draws.
    fn ends_like_a_whole_build(app: &mut App) {
        say(
            app,
            AgentTell::Done {
                pr: pr(),
                turn: 1,
                duration_ms: 1000,
            },
        );
        run_state(app, SessionStateKind::Ready);
        let line = first_line(app);
        let streamed = testing::tree_signature(app, line);
        let t = transcript(app);
        app.world_mut().get_mut::<Transcript>(t).unwrap().built = None;
        testing::settle(app);
        let line = first_line(app);
        assert_eq!(testing::tree_signature(app, line), streamed);
    }

    #[test]
    fn a_streaming_answer_respawns_only_the_blocks_that_changed() {
        let mut app = open_chat();
        run_state(&mut app, SessionStateKind::Running);
        let t = transcript(&mut app);
        let mut most = 0;
        for i in 0..40 {
            let mut parts = vec![
                format!("Paragraph {i} says the lock "),
                "is held while the token ".to_string(),
                "refreshes.\n\n".to_string(),
            ];
            if i % 10 == 9 {
                parts.push(format!("```rust\nlet held_{i} = "));
                parts.push("lock.held();\n```\n\n".to_string());
            }
            for part in parts {
                let before = tree_of(&app, t);
                chunk_of(&mut app, &part);
                // Past the ten-a-second pace, so every chunk is drawn.
                for _ in 0..8 {
                    app.update();
                }
                let spawned = tree_of(&app, t).difference(&before).count();
                most = most.max(spawned);
            }
        }
        assert!(testing::shows(&mut app, "Paragraph 39 says"));
        let whole = tree_of(&app, t).len();
        assert!(whole > 400, "a long answer: {whole} nodes");
        assert!(most <= 30, "{most} nodes spawned in one redraw of {whole}");
        ends_like_a_whole_build(&mut app);
    }

    #[test]
    fn a_block_that_changes_kind_is_respawned_from_there_on_and_ends_exact() {
        let mut app = open_chat();
        run_state(&mut app, SessionStateKind::Running);
        let step = |app: &mut App, text: &str| {
            chunk_of(app, text);
            for _ in 0..8 {
                app.update();
            }
            answer_blocks(app)
        };
        let first = step(&mut app, "The lock is held.\n\nThe token");
        assert_eq!(first.len(), 2);
        // The paragraph becomes a heading: the block before it stays.
        let heading = step(&mut app, " store\n---\n\nIt ");
        assert_eq!(heading.len(), 3);
        assert_eq!(heading[0], first[0], "the finished paragraph was kept");
        assert_ne!(heading[1], first[1], "the paragraph that became a heading");
        // An open fence turns everything after it into code.
        let fenced = step(&mut app, "refreshes.\n\n```rust\nlet a = 1;\n");
        assert_eq!(fenced.len(), 4);
        assert_eq!(fenced[..2], heading[..2], "the finished blocks stay");
        assert_ne!(fenced[2], heading[2], "the paragraph that grew");
        let inside = step(&mut app, "Still code.\n\n- not a list\n");
        assert_eq!(inside.len(), 4, "the fence holds it all");
        assert_eq!(inside[..3], fenced[..3], "the blocks before the fence stay");
        assert_ne!(inside[3], fenced[3], "the code that grew");
        let closed = step(&mut app, "```\n\n1. After the fence.");
        assert_eq!(closed.len(), 5);
        assert_eq!(closed[..3], fenced[..3]);
        assert!(testing::shows(&mut app, "After the fence."));
        let t = transcript(&mut app);
        let mut code: Vec<(usize, String)> = {
            let world = app.world_mut();
            let mut lines = world.query::<(Entity, &crate::ui::code_select::CodeLine)>();
            lines
                .iter(world)
                .map(|(e, l)| (e, l.key.order, l.text.clone()))
                .collect::<Vec<_>>()
        }
        .into_iter()
        .filter(|(e, ..)| is_under(&app, *e, t))
        .map(|(_, order, text)| (order, text))
        .collect();
        code.sort();
        let code: Vec<String> = code.into_iter().map(|(_, text)| text).collect();
        assert_eq!(code, ["let a = 1;", "Still code.", "", "- not a list"]);
        ends_like_a_whole_build(&mut app);
    }

    #[test]
    fn a_selection_in_a_finished_block_of_the_live_answer_survives_a_redraw() {
        use crate::ui::code_select::{At, CodeLine, CodeSelection};
        let mut app = open_chat();
        run_state(&mut app, SessionStateKind::Running);
        chunk_of(&mut app, "```rust\nlet a = 1;\n```\n\nThe ");
        testing::settle(&mut app);
        let t = transcript(&mut app);
        let group = {
            let world = app.world_mut();
            let mut lines = world.query::<(Entity, &CodeLine)>();
            lines
                .iter(world)
                .map(|(e, l)| (e, l.key.group))
                .collect::<Vec<_>>()
        }
        .into_iter()
        .find(|(e, _)| is_under(&app, *e, t))
        .map(|(_, group)| group)
        .expect("the code block is in the answer");
        let at = |offset| At { order: 0, offset };
        app.world_mut()
            .resource_mut::<CodeSelection>()
            .select(group, 0, at(0), at(3));
        testing::settle(&mut app);
        for chunk in ["lock ", "is held.\n\nThe token ", "refreshes."] {
            chunk_of(&mut app, chunk);
            for _ in 0..8 {
                app.update();
            }
        }
        assert!(testing::shows(&mut app, "The lock is held."));
        assert!(app.world().get_entity(group).is_ok(), "the block was kept");
        assert!(
            !app.world().resource::<CodeSelection>().is_empty(),
            "the selection survived the redraws"
        );
    }

    #[test]
    fn new_render_options_mid_answer_redraw_its_finished_blocks() {
        let mut app = open_chat();
        run_state(&mut app, SessionStateKind::Running);
        chunk_of(&mut app, "```rust\nlet a = 1;\n```\n\nThe ");
        for _ in 0..8 {
            app.update();
        }
        let before = answer_blocks(&mut app);
        assert_eq!(before.len(), 2);
        app.world_mut().resource_mut::<Theme>().code_size = 19.0;
        chunk_of(&mut app, "lock.");
        for _ in 0..8 {
            app.update();
        }
        let after = answer_blocks(&mut app);
        assert_eq!(after.len(), 2);
        assert_ne!(
            after[0], before[0],
            "the code block is drawn at the new size"
        );
        let sized = tree_of(&app, after[0]).into_iter().any(|e| {
            app.world()
                .get::<TextFont>(e)
                .is_some_and(|f| f.font_size == FontSize::Rem(19.0))
        });
        assert!(sized, "the kept code would still be at the old size");
    }

    #[test]
    fn a_call_that_arrives_while_the_tail_is_held_keeps_the_finished_blocks() {
        let mut app = open_chat();
        run_state(&mut app, SessionStateKind::Running);
        chunk_of(&mut app, "The lock is held.\n\nThe token");
        for _ in 0..8 {
            app.update();
        }
        // Drawn at once: the last redraw is long past. The next chunk is then held back.
        chunk_of(&mut app, " refreshes");
        let before = answer_blocks(&mut app);
        assert_eq!(before.len(), 2);
        chunk_of(&mut app, " now.");
        assert!(testing::shows(&mut app, "The token refreshes"));
        assert!(!testing::shows(&mut app, "refreshes now."), "held back");
        say(
            &mut app,
            AgentTell::ToolUse {
                pr: pr(),
                turn: 1,
                summary: "Read src/auth/store.rs".into(),
            },
        );
        testing::settle(&mut app);
        let after = answer_blocks(&mut app);
        assert_eq!(after.len(), 2);
        assert_eq!(after[0], before[0], "the finished paragraph was kept");
        assert!(testing::shows(&mut app, "The token refreshes now."));
        let lines = lines_of(&mut app);
        assert_eq!(lines.len(), 3, "the answer, the call and the live row");
        assert!(testing::shows(&mut app, "Read src/auth/store.rs"));
        let t = transcript(&mut app);
        assert!(
            app.world().get::<Transcript>(t).unwrap().answer.is_none(),
            "the call is the last line now"
        );
    }

    #[test]
    fn a_chunk_keeps_the_scroll_of_a_reader_who_scrolled_up() {
        let mut app = open_chat();
        chunk_of(&mut app, "The ");
        testing::settle(&mut app);
        let t = transcript(&mut app);
        app.world_mut().get_mut::<ScrollPosition>(t).unwrap().y = 100.0;
        let mut node = app.world_mut().get_mut::<ComputedNode>(t).unwrap();
        node.size = Vec2::new(400.0, 200.0);
        node.content_size = Vec2::new(400.0, 1000.0);
        node.scroll_position = Vec2::new(0.0, 100.0);
        chunk_of(&mut app, "lock.");
        testing::settle(&mut app);
        assert_eq!(app.world().get::<ScrollPosition>(t).unwrap().y, 100.0);
    }

    #[test]
    fn chunks_that_stop_without_a_done_still_wake_the_loop_for_the_tail() {
        let mut app = open_chat();
        // Still marks need no wake of their own, so any ask is the transcript's.
        app.insert_resource(crate::ui::spinner::Motion { reduced: true });
        app.insert_resource(bevy::winit::WinitSettings::desktop_app());
        run_state(&mut app, SessionStateKind::Running);
        chunk_of(&mut app, "The ");
        for _ in 0..10 {
            app.update();
        }
        // Drawn at once: the last redraw is long past.
        chunk_of(&mut app, "lock.");
        assert_eq!(
            app.world().resource::<Pace>().written,
            None,
            "nothing is pending before a chunk is held back"
        );
        chunk_of(&mut app, " Next.");
        let wait = app
            .world()
            .resource::<Pace>()
            .written
            .expect("a wake is asked while the redraw waits");
        assert!(wait <= std::time::Duration::from_millis(100), "{wait:?}");
        assert!(!testing::shows(&mut app, "Next."), "held back");
        for _ in 0..10 {
            app.update();
        }
        assert!(testing::shows(&mut app, "The lock. Next."), "no Done came");
        assert_eq!(
            app.world().resource::<Pace>().written,
            None,
            "the ask ends with the redraw"
        );
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
