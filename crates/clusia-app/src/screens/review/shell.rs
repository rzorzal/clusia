//! The review shell (mockup `Review.png`): the pull request header, the section tabs, the
//! center `SectionBody` (Diff and Comments fill it; the four sections that arrive later show a
//! placeholder), the right panel (agent empty state and the draft) and the status bar.
//!
//! The shell is read-only while the connection is lost or when it shows the cached copy.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};
use clusia_core::{DraftItem, DraftKind, ItemStatus, PrRef};
use clusia_protocol::ReviewView;

use crate::bridge::{Ask, Asks, Connection, Model, Toasts};
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::nav::{Nav, ReviewScreen, Screen, Section};
use crate::platform_open::{OpenUrls, visit};
use crate::review_state::{
    EditTarget, Editor, Modal, Phase, Ready, ReviewSection, ReviewTabs, Tab,
};
use crate::screens::home::long_age;
use crate::screens::review::ReviewSystems;
use crate::screens::review::editor::{editor_box, not_sent, read_only_reason, sync_editor_text};
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Stroke, Tone, Type, Variant, badge, button, disabled_button, panel,
    text,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderView {
    /// `rzorzal/clusia`
    pub repo: String,
    /// `#123`
    pub number: String,
    pub title: String,
    /// `@octo wants to merge`
    pub author: String,
    /// `octo:auth-refresh → main`
    pub branches: String,
    /// `+120`
    pub additions: String,
    /// `−34`
    pub deletions: String,
    /// `· 7 files`
    pub files: String,
    /// `✓ Checks passing` and its tone; `None` without checks.
    pub checks: Option<(String, Tone)>,
    pub worktree: Option<String>,
    pub url: String,
}

/// Where a draft card's click goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardTarget {
    /// The Diff section, on this file.
    Diff(String),
    Comments,
    /// General notes have no place in the diff.
    Stay,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftCard {
    pub id: String,
    /// `refresh.rs:44`, `General note`, `Reply to @mona · refresh.rs:41`, `Resolve thread by @ana`
    pub title: String,
    pub badge: Option<(String, Tone)>,
    /// The first words of the body (empty for a resolve).
    pub body: String,
    pub obsolete: bool,
    pub target: CardTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusView {
    pub agent: String,
    /// `Draft saved · 3 items`
    pub draft: String,
    pub finalize: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellView {
    pub header: HeaderView,
    /// (section, label with count, selected)
    pub sections: Vec<(ReviewSection, String, bool)>,
    pub draft: Vec<DraftCard>,
    pub status: StatusView,
    pub read_only: Option<String>,
    pub closed: Option<String>,
}

/// The center region of a ready review. The section's filler despawns its children and fills
/// it when the section changes (Diff: Task 11, Comments: Task 12, the rest: `fill_placeholders`).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SectionBody {
    pub pr: PrRef,
}

/// A placeholder section's content inside `SectionBody`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placeholder(pub ReviewSection);

/// What `mount` put in a `ReviewScreen`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mounted {
    Opening,
    Shell,
}

/// The region shown while the tab is loading or failed to load (filled by `loading`).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct OpeningRegion {
    pub pr: PrRef,
}

/// A root modal entity spawned for `pr`. It is despawned as soon as the tab no longer wants
/// `modal`, or the tab is not on screen.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ModalFor {
    pub pr: PrRef,
    pub modal: Modal,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SectionTab {
    pub pr: PrRef,
    pub section: ReviewSection,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct OpenInEditorButton(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ViewOnGitHubButton(pub PrRef);

/// Opens the review again (cached copy or failed load).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct TryAgain(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct HarnessButton;

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct GeneralNoteButton(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DraftCardButton {
    pub pr: PrRef,
    pub target: CardTarget,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FinalizeButton(pub PrRef);

/// **Remove** on a draft card (the only way out for an obsolete item outside the diff).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DraftCardRemove {
    pub pr: PrRef,
    pub id: String,
}

#[derive(Component)]
struct ShellTop {
    pr: PrRef,
    built: Option<TopView>,
}

#[derive(Component)]
struct RightPanel {
    pr: PrRef,
    built: Option<RightView>,
}

#[derive(Component)]
struct StatusBar {
    pr: PrRef,
    built: Option<StatusView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TopView {
    header: HeaderView,
    sections: Vec<(ReviewSection, String, bool)>,
    read_only: Option<String>,
    retry: bool,
    closed: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RightView {
    cards: Vec<DraftCard>,
    comments: bool,
    read_only: bool,
    /// The general-note editor: (error, waiting for the daemon).
    general: Option<(Option<String>, bool)>,
}

fn is_placeholder(section: ReviewSection) -> bool {
    !matches!(section, ReviewSection::Diff | ReviewSection::Comments)
}

/// What a placeholder section says: (heading, text).
pub fn placeholder_text(section: ReviewSection) -> (&'static str, &'static str) {
    match section {
        ReviewSection::Diagrams => (
            "Diagrams",
            "Arrives with SP3 (#8): class, sequence and data-flow diagrams of this change.",
        ),
        ReviewSection::Security => (
            "Security",
            "Arrives with the harness (SP2, #7): your agent checks this change for security issues.",
        ),
        ReviewSection::Audits => (
            "Audits",
            "Arrives with the harness (SP2, #7): audits your agent runs on this change.",
        ),
        ReviewSection::Tests => (
            "Tests",
            "Arrives with SP4 (#9): run and read the tests this change touches.",
        ),
        ReviewSection::Diff | ReviewSection::Comments => ("", ""),
    }
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The first `max` characters of `body` on one line, with `…` when cut.
fn preview(body: &str, max: usize) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let kept: String = flat.chars().take(max).collect();
    format!("{}…", kept.trim_end())
}

pub fn header_view(view: &ReviewView) -> HeaderView {
    let pr = &view.pr;
    let s = &pr.summary;
    let author = if pr.merged {
        format!("Merged · @{}", s.author)
    } else if pr.closed {
        format!("Closed · @{}", s.author)
    } else {
        format!("@{} wants to merge", s.author)
    };
    let checks = view.checks.and_then(|c| match c.label() {
        "passed" => Some(("✓ Checks passing".to_string(), Tone::Green)),
        "failed" => Some(("× Checks failing".to_string(), Tone::Orange)),
        "pending" => Some(("• Checks running".to_string(), Tone::Neutral)),
        _ => None,
    });
    HeaderView {
        repo: s.pr.slug(),
        number: format!("#{}", s.pr.number),
        title: s.title.clone(),
        author,
        branches: format!("{} → {}", pr.head_ref, pr.base_ref),
        additions: format!("+{}", pr.additions),
        deletions: format!("−{}", pr.deletions),
        files: format!("· {}", plural(view.files.len(), "file")),
        checks,
        worktree: view.worktree.clone(),
        url: s.url.clone(),
    }
}

/// Review threads, issue comments and reviews with a body.
pub fn comment_count(view: &ReviewView) -> usize {
    view.conversation.as_ref().map_or(0, |c| {
        c.review_threads.len()
            + c.comments.len()
            + c.reviews
                .iter()
                .filter(|r| !r.body.trim().is_empty())
                .count()
    })
}

pub fn draft_card(item: &DraftItem) -> DraftCard {
    let place = |path: &str, line: u32| format!("{}:{line}", file_name(path));
    let (title, target) = match item.kind {
        DraftKind::LineComment => match &item.anchor {
            Some(a) => {
                let lines = match a.start_line {
                    Some(start) if start != a.line => format!("{start}–{}", a.line),
                    _ => a.line.to_string(),
                };
                (
                    format!("{}:{lines}", file_name(&a.path)),
                    CardTarget::Diff(a.path.clone()),
                )
            }
            None => ("Line comment".to_string(), CardTarget::Stay),
        },
        DraftKind::General => ("General note".to_string(), CardTarget::Stay),
        DraftKind::Reply => {
            let mut t = match &item.thread {
                Some(th) => format!("Reply to @{}", th.author),
                None => "Reply".to_string(),
            };
            if let Some(th) = &item.thread
                && let (Some(path), Some(line)) = (&th.path, th.line)
            {
                t.push_str(&format!(" · {}", place(path, line)));
            }
            (t, CardTarget::Comments)
        }
        DraftKind::Resolve => (
            match &item.thread {
                Some(th) => format!("Resolve thread by @{}", th.author),
                None => "Resolve thread".to_string(),
            },
            CardTarget::Comments,
        ),
    };
    let badge = match &item.status {
        ItemStatus::Ok => None,
        ItemStatus::Moved {
            from_path,
            from_line,
        } => {
            let same = item.anchor.as_ref().is_some_and(|a| &a.path == from_path);
            Some((
                if same {
                    format!("moved from {from_line}")
                } else {
                    format!("moved from {}", place(from_path, *from_line))
                },
                Tone::Green,
            ))
        }
        ItemStatus::Obsolete { .. } => Some(("obsolete".to_string(), Tone::Orange)),
    };
    DraftCard {
        id: item.id.clone(),
        title,
        badge,
        body: if item.kind == DraftKind::Resolve {
            String::new()
        } else {
            preview(&item.body, 80)
        },
        obsolete: matches!(item.status, ItemStatus::Obsolete { .. }),
        target,
    }
}

/// Why the review cannot be changed right now, if it cannot.
fn read_only(ready: &Ready, connection: &Connection, now: i64) -> Option<String> {
    if *connection != Connection::Live {
        return Some(
            "Not connected to clusiad: showing the last data. Changes are off until it reconnects."
                .into(),
        );
    }
    ready
        .cached_at
        .map(|at| format!("Showing the cached copy from {} ago.", long_age(now - at)))
}

pub fn shell_view(
    tab: &Tab,
    snap: &Snapshot,
    connection: &Connection,
    now: i64,
) -> Option<ShellView> {
    let Phase::Ready(ready) = &tab.phase else {
        return None;
    };
    let view = &ready.view;
    let mut header = header_view(view);
    if header.url.is_empty() {
        let pr = &view.pr.summary.pr;
        header.url = format!(
            "https://{}/{}/{}/pull/{}",
            snap.config.github.host, pr.owner, pr.repo, pr.number
        );
    }
    let sections = ReviewSection::ALL
        .iter()
        .map(|&s| {
            let count = match s {
                ReviewSection::Diff => view.files.len(),
                ReviewSection::Comments => comment_count(view),
                _ => 0,
            };
            let label = if count > 0 {
                format!("{} {count}", s.label())
            } else {
                s.label().to_string()
            };
            (s, label, s == tab.ui.section)
        })
        .collect();
    let items = &view.review.draft.items;
    let read_only = read_only(ready, connection, now);
    let closed = if view.pr.merged {
        Some("This pull request is merged — publishing is off; you can still discard the review.")
    } else if view.pr.closed {
        Some("This pull request is closed — publishing is off; you can still discard the review.")
    } else {
        None
    };
    Some(ShellView {
        header,
        sections,
        draft: items.iter().map(draft_card).collect(),
        status: StatusView {
            agent: "Agent not connected".into(),
            draft: if items.is_empty() {
                "Draft empty".into()
            } else {
                format!("Draft saved · {}", plural(items.len(), "item"))
            },
            finalize: read_only.is_none(),
        },
        read_only,
        closed: closed.map(String::from),
    })
}

pub struct ShellPlugin;

impl Plugin for ShellPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                sync_editor_text,
                mount,
                rebuild_top,
                fill_placeholders,
                rebuild_right,
                rebuild_status,
                drop_stale_modals,
            )
                .chain()
                .in_set(ReviewSystems),
        );
    }
}

/// Fills each `ReviewScreen` with the opening region or the shell, and swaps them when the
/// tab's phase crosses between loading and ready.
fn mount(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    fonts: Res<UiFonts>,
    screens: Query<(Entity, &ReviewScreen, Option<&Mounted>)>,
) {
    for (entity, ReviewScreen(pr), mounted) in &screens {
        let Some(tab) = tabs.0.get(pr) else { continue };
        let want = if matches!(tab.phase, Phase::Ready(_)) {
            Mounted::Shell
        } else {
            Mounted::Opening
        };
        if mounted == Some(&want) {
            continue;
        }
        commands
            .entity(entity)
            .despawn_related::<Children>()
            .insert(want);
        commands.entity(entity).with_children(|p| match want {
            Mounted::Shell => shell_frame(p, pr),
            Mounted::Opening => {
                p.spawn((
                    Node {
                        flex_grow: 1.0,
                        width: percent(100),
                        min_height: px(0),
                        flex_direction: FlexDirection::Column,
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    OpeningRegion { pr: pr.clone() },
                ))
                .with_children(|o| {
                    o.spawn(text(
                        &fonts,
                        format!("Opening {}/{} #{}…", pr.owner, pr.repo, pr.number),
                        Type::HEADING,
                    ));
                });
            }
        });
    }
}

/// The empty regions of the shell; the rebuild systems fill them.
fn shell_frame(p: &mut ChildSpawnerCommands, pr: &PrRef) {
    p.spawn(Node {
        flex_grow: 1.0,
        width: percent(100),
        min_height: px(0),
        flex_direction: FlexDirection::Column,
        ..default()
    })
    .with_children(|c| {
        c.spawn((
            Node {
                flex_direction: FlexDirection::Column,
                flex_shrink: 0.0,
                ..default()
            },
            ShellTop {
                pr: pr.clone(),
                built: None,
            },
        ));
        c.spawn((
            Node {
                flex_grow: 1.0,
                min_height: px(0),
                border: UiRect::top(px(1)),
                ..default()
            },
            BorderColor::default(),
            Stroke(Swatch::Line),
        ))
        .with_children(|body| {
            body.spawn((
                Node {
                    flex_grow: 1.0,
                    min_width: px(0),
                    min_height: px(0),
                    ..default()
                },
                SectionBody { pr: pr.clone() },
            ));
            body.spawn((
                Node {
                    width: px(300),
                    flex_shrink: 0.0,
                    flex_direction: FlexDirection::Column,
                    border: UiRect::left(px(1)),
                    overflow: Overflow::scroll_y(),
                    ..default()
                },
                BorderColor::default(),
                Stroke(Swatch::Line),
                RightPanel {
                    pr: pr.clone(),
                    built: None,
                },
            ));
        });
        c.spawn((
            panel(
                Node {
                    height: px(48),
                    flex_shrink: 0.0,
                    align_items: AlignItems::Center,
                    column_gap: px(16),
                    padding: UiRect::horizontal(px(20)),
                    border: UiRect::top(px(1)),
                    ..default()
                },
                Swatch::Bg,
            ),
            BorderColor::default(),
            Stroke(Swatch::Line),
            StatusBar {
                pr: pr.clone(),
                built: None,
            },
        ));
    });
}

fn refill(commands: &mut Commands, entity: Entity, build: impl FnOnce(&mut ChildSpawnerCommands)) {
    commands.entity(entity).despawn_related::<Children>();
    commands.entity(entity).with_children(build);
}

/// The shell view of `pr`'s ready tab, and whether it shows the cached copy.
fn current_view(
    tabs: &ReviewTabs,
    model: &Model,
    pr: &PrRef,
    now: i64,
) -> Option<(ShellView, bool)> {
    let tab = tabs.0.get(pr)?;
    let view = shell_view(tab, &model.snapshot, &model.connection, now)?;
    let cached = tab.ready().is_some_and(|r| r.cached_at.is_some());
    Some((view, cached))
}

fn rebuild_top(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    clock: Res<Clock>,
    fonts: Res<UiFonts>,
    mut tops: Query<(Entity, &mut ShellTop)>,
) {
    for (entity, mut top) in &mut tops {
        let Some((v, cached)) = current_view(&tabs, &model, &top.pr, clock.now()) else {
            continue;
        };
        let want = TopView {
            header: v.header,
            sections: v.sections,
            retry: v.read_only.is_some() && cached && model.connection == Connection::Live,
            read_only: v.read_only,
            closed: v.closed,
        };
        if top.built.as_ref() == Some(&want) {
            continue;
        }
        let pr = top.pr.clone();
        refill(&mut commands, entity, |p| top_region(p, &fonts, &pr, &want));
        top.built = Some(want);
    }
}

/// The header block (also drawn, inert, under the loading card).
pub fn header_block(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, h: &HeaderView) {
    p.spawn(Node {
        padding: UiRect {
            left: px(20),
            right: px(20),
            top: px(16),
            bottom: px(8),
        },
        align_items: AlignItems::Center,
        column_gap: px(12),
        ..default()
    })
    .with_children(|row| {
        row.spawn(Node {
            flex_grow: 1.0,
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            ..default()
        })
        .with_children(|c| {
            c.spawn(Node {
                column_gap: px(10),
                align_items: AlignItems::Baseline,
                ..default()
            })
            .with_children(|t| {
                t.spawn(text(fonts, h.repo.clone(), Type::MUTED));
                t.spawn(text(fonts, h.number.clone(), Type::MUTED));
                t.spawn(text(fonts, h.title.clone(), Type::TITLE.size(17.0)));
            });
            c.spawn(Node {
                column_gap: px(10),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|m| {
                m.spawn(text(fonts, h.author.clone(), Type::META));
                m.spawn(panel(
                    Node {
                        padding: UiRect::axes(px(6), px(2)),
                        border_radius: BorderRadius::all(px(4)),
                        ..default()
                    },
                    Swatch::Chrome,
                ))
                .with_children(|chip| {
                    chip.spawn(text(fonts, h.branches.clone(), Type::MONO));
                });
                m.spawn(Node {
                    column_gap: px(4),
                    ..default()
                })
                .with_children(|n| {
                    n.spawn(text(
                        fonts,
                        h.additions.clone(),
                        Type::STRONG.ink(Swatch::Green),
                    ));
                    n.spawn(text(
                        fonts,
                        h.deletions.clone(),
                        Type::STRONG.ink(Swatch::Orange),
                    ));
                    n.spawn(text(fonts, h.files.clone(), Type::META));
                });
                if let Some((label, tone)) = &h.checks {
                    let ink = match tone {
                        Tone::Green => Swatch::Green,
                        Tone::Orange => Swatch::Orange,
                        Tone::Neutral => Swatch::Muted,
                    };
                    m.spawn(text(fonts, label.clone(), Type::BODY.ink(ink)));
                }
            });
        });
        match &h.worktree {
            Some(_) => {
                row.spawn((
                    button(fonts, "Open in editor", Variant::Secondary),
                    OpenInEditorButton(pr.clone()),
                    observe(on_open_in_editor),
                ));
            }
            None => {
                row.spawn(disabled_button(fonts, "Open in editor"));
            }
        }
        row.spawn((
            button(fonts, "View on GitHub", Variant::Secondary),
            ViewOnGitHubButton(pr.clone()),
            observe(on_view_on_github),
        ));
    });
}

fn top_region(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, v: &TopView) {
    header_block(p, fonts, pr, &v.header);
    let strip = |fill| {
        panel(
            Node {
                padding: UiRect::axes(px(20), px(8)),
                column_gap: px(12),
                align_items: AlignItems::Center,
                ..default()
            },
            fill,
        )
    };
    if let Some(message) = &v.read_only {
        p.spawn(strip(Swatch::Chrome)).with_children(|s| {
            s.spawn(text(fonts, message.clone(), Type::MUTED));
            if v.retry {
                s.spawn((
                    button(fonts, "Try again", Variant::Secondary),
                    TryAgain(pr.clone()),
                    observe(on_try_again),
                ));
            }
        });
    }
    if let Some(message) = &v.closed {
        p.spawn(strip(Swatch::OrangeSoft)).with_children(|s| {
            s.spawn(text(fonts, message.clone(), Type::BODY.ink(Swatch::Orange)));
        });
    }
    section_tabs(p, fonts, pr, &v.sections, true);
}

/// The section tab row. `live: false` draws it inert (under the loading card).
pub fn section_tabs(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    sections: &[(ReviewSection, String, bool)],
    live: bool,
) {
    p.spawn(Node {
        padding: UiRect::horizontal(px(20)),
        column_gap: px(4),
        ..default()
    })
    .with_children(|row| {
        for (section, label, on) in sections {
            let base = section.label();
            let count = label.strip_prefix(base).unwrap_or("").trim().to_string();
            let mut tab = row.spawn((
                Node {
                    height: px(34),
                    padding: UiRect::horizontal(px(12)),
                    align_items: AlignItems::Center,
                    column_gap: px(6),
                    border: UiRect::bottom(px(2)),
                    ..default()
                },
                BorderColor::default(),
                Stroke(if *on { Swatch::Green } else { Swatch::Clear }),
            ));
            if live {
                tab.insert((
                    WidgetButton,
                    Clickable,
                    Hovered::default(),
                    TabIndex(0),
                    BackgroundColor::default(),
                    Fill(Swatch::Clear),
                    HoverFill(Swatch::Hover),
                    SectionTab {
                        pr: pr.clone(),
                        section: *section,
                    },
                    observe(on_section),
                ));
            }
            tab.with_children(|t| {
                t.spawn(text(
                    fonts,
                    base,
                    if *on { Type::STRONG } else { Type::MUTED },
                ));
                if !count.is_empty() {
                    t.spawn(text(fonts, count, Type::META));
                }
            });
        }
    });
}

/// Fills `SectionBody` for the four sections that arrive later, and empties it when the
/// section moves to one of them or away from them.
fn fill_placeholders(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    fonts: Res<UiFonts>,
    bodies: Query<(Entity, &SectionBody, Option<&Children>)>,
    placeholders: Query<&Placeholder>,
) {
    for (entity, body, children) in &bodies {
        let Some(tab) = tabs.0.get(&body.pr) else {
            continue;
        };
        let section = tab.ui.section;
        let shown = children
            .into_iter()
            .flatten()
            .find_map(|c| placeholders.get(*c).ok().copied());
        if !is_placeholder(section) {
            if shown.is_some() {
                commands.entity(entity).despawn_related::<Children>();
            }
            continue;
        }
        if shown == Some(Placeholder(section)) {
            continue;
        }
        let (heading, line) = placeholder_text(section);
        refill(&mut commands, entity, |p| {
            p.spawn((
                Node {
                    flex_grow: 1.0,
                    flex_direction: FlexDirection::Column,
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    row_gap: px(8),
                    padding: px(40).all(),
                    ..default()
                },
                Placeholder(section),
            ))
            .with_children(|c| {
                c.spawn(text(&fonts, heading, Type::HEADING));
                c.spawn(text(&fonts, line, Type::MUTED));
            });
        });
    }
}

fn rebuild_right(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    clock: Res<Clock>,
    fonts: Res<UiFonts>,
    mut panels: Query<(Entity, &mut RightPanel)>,
) {
    for (entity, mut right) in &mut panels {
        let Some((v, _)) = current_view(&tabs, &model, &right.pr, clock.now()) else {
            continue;
        };
        let tab = &tabs.0[&right.pr];
        let general = tab
            .ui
            .editor
            .as_ref()
            .filter(|e| e.target == EditTarget::General);
        let want = RightView {
            cards: v.draft,
            comments: tab.ui.section == ReviewSection::Comments,
            read_only: v.read_only.is_some(),
            general: general.map(|e| (e.error.clone(), e.ticket.is_some())),
        };
        if right.built.as_ref() == Some(&want) {
            continue;
        }
        let pr = right.pr.clone();
        let editor = general.cloned();
        refill(&mut commands, entity, |p| {
            right_region(p, &fonts, &pr, &want, editor.as_ref())
        });
        right.built = Some(want);
    }
}

fn right_region(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    v: &RightView,
    editor: Option<&Editor>,
) {
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(10),
        padding: px(16).all(),
        border: UiRect::bottom(px(1)),
        ..default()
    })
    .insert((BorderColor::default(), Stroke(Swatch::Line)))
    .with_children(|agent| {
        agent
            .spawn(Node {
                column_gap: px(8),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|h| {
                h.spawn(text(fonts, "Agent", Type::STRONG));
                h.spawn(badge(fonts, "not set up", Tone::Neutral));
            });
        agent.spawn(text(
            fonts,
            "Connect Claude Code, Codex or your own command to talk through this pull request. Your draft works without it.",
            Type::MUTED,
        ));
        agent.spawn((
            button(fonts, "Set up a harness", Variant::Secondary),
            HarnessButton,
            observe(on_harness),
        ));
    });
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(8),
        padding: px(16).all(),
        ..default()
    })
    .with_children(|draft| {
        draft
            .spawn(Node {
                column_gap: px(8),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|h| {
                h.spawn(text(fonts, "Draft", Type::STRONG));
                h.spawn(text(fonts, v.cards.len().to_string(), Type::META));
                h.spawn(Node {
                    flex_grow: 1.0,
                    ..default()
                });
                if !v.read_only {
                    h.spawn((
                        button(fonts, "+ General note", Variant::Ghost),
                        GeneralNoteButton(pr.clone()),
                        observe(on_general_note),
                    ));
                }
            });
        if v.comments {
            draft.spawn(text(
                fonts,
                "Replies and new threads you write here join the draft. Nothing is posted until you publish.",
                Type::META,
            ));
        }
        if let Some(editor) = editor {
            editor_box(draft, fonts, editor, pr);
        }
        if v.cards.is_empty() && editor.is_none() {
            draft.spawn(text(
                fonts,
                "Click a line in the diff to comment on it.",
                Type::META,
            ));
        }
        for card in &v.cards {
            draft_card_node(draft, fonts, pr, card, v.read_only);
        }
    });
}

fn draft_card_node(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    card: &DraftCard,
    read_only: bool,
) {
    p.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(4),
            padding: UiRect::axes(px(12), px(10)),
            border: px(1).all(),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        WidgetButton,
        Clickable,
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(Swatch::Surface),
        HoverFill(Swatch::Hover),
        BorderColor::default(),
        Stroke(if card.obsolete {
            Swatch::Orange
        } else {
            Swatch::Line
        }),
        DraftCardButton {
            pr: pr.clone(),
            target: card.target.clone(),
        },
        observe(on_card),
    ))
    .with_children(|c| {
        c.spawn(Node {
            column_gap: px(8),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|h| {
            h.spawn(text(fonts, card.title.clone(), Type::MONO));
            if let Some((label, tone)) = &card.badge {
                h.spawn(badge(fonts, label, *tone));
            }
            if !read_only {
                h.spawn(Node {
                    flex_grow: 1.0,
                    ..default()
                });
                // A button inside the card's button: its click stops here.
                h.spawn((
                    button(fonts, "Remove", Variant::Ghost),
                    DraftCardRemove {
                        pr: pr.clone(),
                        id: card.id.clone(),
                    },
                    observe(on_card_remove),
                ));
            }
        });
        if !card.body.is_empty() {
            c.spawn(text(fonts, card.body.clone(), Type::BODY));
        }
    });
}

fn rebuild_status(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    clock: Res<Clock>,
    fonts: Res<UiFonts>,
    mut bars: Query<(Entity, &mut StatusBar)>,
) {
    for (entity, mut bar) in &mut bars {
        let Some((v, _)) = current_view(&tabs, &model, &bar.pr, clock.now()) else {
            continue;
        };
        if bar.built.as_ref() == Some(&v.status) {
            continue;
        }
        let pr = bar.pr.clone();
        let status = v.status.clone();
        refill(&mut commands, entity, |p| {
            p.spawn(Node {
                column_gap: px(8),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|a| {
                a.spawn(panel(
                    Node {
                        width: px(7),
                        height: px(7),
                        border_radius: BorderRadius::MAX,
                        ..default()
                    },
                    Swatch::Faint,
                ));
                a.spawn(text(&fonts, status.agent.clone(), Type::MUTED));
            });
            p.spawn(text(&fonts, status.draft.clone(), Type::META));
            p.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            if status.finalize {
                p.spawn((
                    button(&fonts, "Finalize review", Variant::Primary),
                    FinalizeButton(pr.clone()),
                    observe(on_finalize),
                ));
            } else {
                p.spawn(disabled_button(&fonts, "Finalize review"));
            }
        });
        bar.built = Some(v.status);
    }
}

/// Despawns root modals whose tab no longer wants them or is not on screen.
fn drop_stale_modals(
    mut commands: Commands,
    nav: Res<Nav>,
    tabs: Res<ReviewTabs>,
    modals: Query<(Entity, &ModalFor)>,
) {
    for (entity, m) in &modals {
        let showing = nav.screen == Screen::Review(m.pr.clone());
        let wanted = tabs.0.get(&m.pr).and_then(|t| t.ui.modal) == Some(m.modal);
        if !(showing && wanted) {
            commands.entity(entity).despawn();
        }
    }
}

fn on_section(activate: On<Activate>, tabs_q: Query<&SectionTab>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(t) = tabs_q.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(&t.pr)
        && tab.ui.section != t.section
    {
        tab.ui.section = t.section;
    }
}

fn on_open_in_editor(
    activate: On<Activate>,
    buttons: Query<&OpenInEditorButton>,
    tabs: Res<ReviewTabs>,
    mut asks: ResMut<Asks>,
) {
    let Ok(OpenInEditorButton(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(Phase::Ready(ready)) = tabs.0.get(pr).map(|t| &t.phase)
        && let Some(path) = &ready.view.worktree
    {
        asks.send(Ask::OpenInEditor {
            path: path.clone(),
            line: None,
        });
    }
}

fn on_view_on_github(
    activate: On<Activate>,
    buttons: Query<&ViewOnGitHubButton>,
    tabs: Res<ReviewTabs>,
    urls: Option<ResMut<OpenUrls>>,
) {
    let Ok(ViewOnGitHubButton(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(Phase::Ready(ready)) = tabs.0.get(pr).map(|t| &t.phase) {
        visit(urls, &header_view(&ready.view).url);
    }
}

pub fn on_try_again(
    activate: On<Activate>,
    buttons: Query<&TryAgain>,
    mut tabs: ResMut<ReviewTabs>,
    mut asks: ResMut<Asks>,
) {
    if let Ok(TryAgain(pr)) = buttons.get(activate.entity) {
        tabs.retry(pr, &mut asks);
    }
}

fn on_harness(_activate: On<Activate>, mut nav: ResMut<Nav>) {
    nav.open_section(Section::Harness);
}

fn on_general_note(
    activate: On<Activate>,
    buttons: Query<&GeneralNoteButton>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let Ok(GeneralNoteButton(pr)) = buttons.get(activate.entity) else {
        return;
    };
    // An editor with unsent text stays: the user finishes or cancels it first.
    let busy = |e: &Editor| {
        e.target == EditTarget::General || (e.ticket.is_none() && !e.text.trim().is_empty())
    };
    if let Some(tab) = tabs.0.get_mut(pr)
        && !tab.ui.editor.as_ref().is_some_and(busy)
    {
        tab.ui.editor = Some(Editor {
            target: EditTarget::General,
            text: String::new(),
            error: None,
            ticket: None,
        });
    }
}

fn on_card(activate: On<Activate>, cards: Query<&DraftCardButton>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(card) = cards.get(activate.entity) else {
        return;
    };
    let Some(tab) = tabs.0.get_mut(&card.pr) else {
        return;
    };
    match &card.target {
        CardTarget::Diff(path) => {
            tab.ui.section = ReviewSection::Diff;
            tab.ui.file = Some(path.clone());
        }
        CardTarget::Comments => tab.ui.section = ReviewSection::Comments,
        CardTarget::Stay => {}
    }
}

fn on_card_remove(
    activate: On<Activate>,
    buttons: Query<&DraftCardRemove>,
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
    asks.send(Ask::RemoveItem {
        pr: b.pr.clone(),
        id: b.id.clone(),
    });
}

fn on_finalize(
    activate: On<Activate>,
    buttons: Query<&FinalizeButton>,
    mut tabs: ResMut<ReviewTabs>,
) {
    if let Ok(FinalizeButton(pr)) = buttons.get(activate.entity)
        && let Some(tab) = tabs.0.get_mut(pr)
    {
        tab.ui.modal = Some(Modal::Finalize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::nav::Screen;
    use crate::testing::{self, NOW};
    use clusia_core::{Anchor, Origin, Side, ThreadRef};

    fn ready_tab() -> Tab {
        let (view, news) = fixture::demo_review(NOW);
        Tab {
            phase: Phase::Ready(Box::new(Ready {
                view,
                news,
                cached_at: None,
            })),
            ui: Default::default(),
        }
    }

    fn view_of(tab: &Tab, connection: &Connection) -> ShellView {
        shell_view(tab, &fixture::demo(NOW), connection, NOW).expect("ready")
    }

    fn item(kind: DraftKind, anchor: Option<Anchor>, thread: Option<ThreadRef>) -> DraftItem {
        DraftItem {
            id: "i9".into(),
            kind,
            origin: Origin::Human,
            anchor,
            body: "  Why one minute?\n\nThe CLI uses 30 seconds.  ".into(),
            status: ItemStatus::Ok,
            accepted: true,
            created_at: NOW,
            thread,
        }
    }

    #[test]
    fn demo_shell_matches_the_mockup() {
        let v = view_of(&ready_tab(), &Connection::Live);
        let h = &v.header;
        assert_eq!(
            (h.repo.as_str(), h.number.as_str(), h.title.as_str()),
            ("rzorzal/clusia", "#123", "feat: auth refresh")
        );
        assert_eq!(h.author, "@octo wants to merge");
        assert_eq!(h.branches, "octo:auth-refresh → main");
        assert_eq!(
            (h.additions.as_str(), h.deletions.as_str(), h.files.as_str()),
            ("+120", "−34", "· 7 files")
        );
        assert_eq!(h.checks, Some(("✓ Checks passing".into(), Tone::Green)));
        assert_eq!(h.url, "https://github.com/rzorzal/clusia/pull/123");
        let labels: Vec<(&str, bool)> = v.sections.iter().map(|s| (s.1.as_str(), s.2)).collect();
        assert_eq!(
            labels,
            [
                ("Diagrams", false),
                ("Security", false),
                ("Diff 7", true),
                ("Audits", false),
                ("Comments 5", false),
                ("Tests", false),
            ]
        );
        let titles: Vec<&str> = v.draft.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["refresh.rs:44", "store.rs:88", "http.rs:20"]);
        assert_eq!(v.draft[0].badge, None);
        assert!(
            v.draft[0]
                .body
                .starts_with("Holding the lock across the network call")
        );
        assert!(v.draft[0].body.ends_with('…'));
        assert_eq!(
            v.draft[1].badge,
            Some(("moved from 81".into(), Tone::Green))
        );
        assert_eq!(v.draft[2].badge, Some(("obsolete".into(), Tone::Orange)));
        assert!(v.draft[2].obsolete && !v.draft[1].obsolete);
        assert_eq!(
            v.draft[0].target,
            CardTarget::Diff("src/auth/refresh.rs".into())
        );
        assert_eq!(
            v.status,
            StatusView {
                agent: "Agent not connected".into(),
                draft: "Draft saved · 3 items".into(),
                finalize: true,
            }
        );
        assert_eq!((v.read_only, v.closed), (None, None));
    }

    #[test]
    fn checks_and_pr_state_in_the_header() {
        let mut tab = ready_tab();
        let Phase::Ready(ready) = &mut tab.phase else {
            unreachable!()
        };
        let checks = |total, failed, pending| clusia_core::ChecksSummary {
            total,
            passed: total - failed - pending,
            failed,
            pending,
        };
        ready.view.checks = Some(checks(3, 1, 0));
        assert_eq!(
            header_view(&ready.view).checks,
            Some(("× Checks failing".into(), Tone::Orange))
        );
        ready.view.checks = Some(checks(3, 0, 2));
        assert_eq!(
            header_view(&ready.view).checks,
            Some(("• Checks running".into(), Tone::Neutral))
        );
        ready.view.checks = Some(checks(0, 0, 0));
        assert_eq!(header_view(&ready.view).checks, None);
        ready.view.checks = None;
        assert_eq!(header_view(&ready.view).checks, None);
        ready.view.pr.merged = true;
        ready.view.pr.closed = true;
        assert_eq!(header_view(&ready.view).author, "Merged · @octo");
        let v = view_of(&tab, &Connection::Live);
        assert_eq!(
            v.closed.as_deref(),
            Some(
                "This pull request is merged — publishing is off; you can still discard the review."
            )
        );
        assert!(v.status.finalize, "discarding still goes through Finalize");
        let Phase::Ready(ready) = &mut tab.phase else {
            unreachable!()
        };
        ready.view.pr.merged = false;
        assert!(
            view_of(&tab, &Connection::Live)
                .closed
                .unwrap()
                .contains("is closed")
        );
    }

    #[test]
    fn read_only_when_lost_or_cached() {
        let mut tab = ready_tab();
        let lost = view_of(
            &tab,
            &Connection::Lost("clusiad closed the connection".into()),
        );
        assert!(
            lost.read_only
                .unwrap()
                .starts_with("Not connected to clusiad")
        );
        assert!(!lost.status.finalize);
        let Phase::Ready(ready) = &mut tab.phase else {
            unreachable!()
        };
        ready.cached_at = Some(NOW - 3 * 3600);
        let cached = view_of(&tab, &Connection::Live);
        assert_eq!(
            cached.read_only.as_deref(),
            Some("Showing the cached copy from 3 hours ago.")
        );
        assert!(!cached.status.finalize);
        tab.phase = Phase::Loading {
            steps: vec![],
            cached: None,
        };
        assert!(shell_view(&tab, &Snapshot::default(), &Connection::Live, NOW).is_none());
    }

    #[test]
    fn cards_for_every_kind_of_item() {
        let thread = ThreadRef {
            id: "PRRT_1".into(),
            author: "mona".into(),
            path: Some("src/auth/refresh.rs".into()),
            line: Some(41),
        };
        let range = Anchor {
            path: "src/auth/store.rs".into(),
            line: 44,
            start_line: Some(40),
            side: Side::Right,
            commit: "h".into(),
        };
        let line = draft_card(&item(DraftKind::LineComment, Some(range.clone()), None));
        assert_eq!(line.title, "store.rs:40–44");
        assert_eq!(line.body, "Why one minute? The CLI uses 30 seconds.");
        let general = draft_card(&item(DraftKind::General, None, None));
        assert_eq!(
            (general.title.as_str(), general.target),
            ("General note", CardTarget::Stay)
        );
        let reply = draft_card(&item(DraftKind::Reply, None, Some(thread.clone())));
        assert_eq!(reply.title, "Reply to @mona · refresh.rs:41");
        assert_eq!(reply.target, CardTarget::Comments);
        let mut resolve = item(DraftKind::Resolve, None, Some(thread));
        resolve.body.clear();
        let resolve = draft_card(&resolve);
        assert_eq!(resolve.title, "Resolve thread by @mona");
        assert_eq!(resolve.body, "");
        let mut moved = item(DraftKind::LineComment, Some(range), None);
        moved.status = ItemStatus::Moved {
            from_path: "src/auth/old.rs".into(),
            from_line: 9,
        };
        assert_eq!(
            draft_card(&moved).badge,
            Some(("moved from old.rs:9".into(), Tone::Green))
        );
        assert_eq!(preview("a b", 80), "a b");
    }

    #[test]
    fn shell_fills_a_ready_tab() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        assert_eq!(testing::count::<SectionBody>(&mut app), 1);
        assert_eq!(testing::count::<OpeningRegion>(&mut app), 0);
        assert_eq!(testing::count::<SectionTab>(&mut app), 6);
        assert_eq!(testing::count::<DraftCardButton>(&mut app), 3);
        assert_eq!(testing::count::<FinalizeButton>(&mut app), 1);
        assert_eq!(
            testing::count::<Placeholder>(&mut app),
            0,
            "Diff is not a placeholder"
        );
        let security =
            testing::find::<SectionTab>(&mut app, |t| t.section == ReviewSection::Security);
        testing::activate(&mut app, security);
        testing::settle(&mut app);
        assert_eq!(
            app.world().resource::<ReviewTabs>().0[&pr].ui.section,
            ReviewSection::Security
        );
        let shown = testing::find::<Placeholder>(&mut app, |_| true);
        assert_eq!(
            app.world().get::<Placeholder>(shown),
            Some(&Placeholder(ReviewSection::Security))
        );
        let has = |app: &mut App, needle: &str| {
            let mut q = app.world_mut().query::<&Text>();
            q.iter(app.world()).any(|t| t.0.contains(needle))
        };
        assert!(has(&mut app, "Arrives with the harness (SP2, #7)"));
        let diff = testing::find::<SectionTab>(&mut app, |t| t.section == ReviewSection::Diff);
        testing::activate(&mut app, diff);
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<Placeholder>(&mut app),
            0,
            "Diff's filler takes over"
        );
        assert_eq!(
            testing::count::<SectionBody>(&mut app),
            1,
            "the body itself stays"
        );
    }

    #[test]
    fn loading_tabs_mount_the_opening_region_then_the_shell() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr: PrRef = "rzorzal/clusia#123".parse().unwrap();
        app.world_mut().write_message(crate::bridge::ShowRequested(
            clusia_protocol::WindowTarget::Review { pr: pr.clone() },
        ));
        testing::settle(&mut app);
        assert_eq!(testing::recorded(&mut app), [Ask::OpenReview(pr.clone())]);
        assert_eq!(testing::count::<OpeningRegion>(&mut app), 1);
        assert_eq!(testing::count::<SectionBody>(&mut app), 0);
        let (view, _) = fixture::demo_review(NOW);
        testing::tell(
            &mut app,
            crate::bridge::Tell::Opened {
                pr,
                view: Box::new(view),
                news: vec![],
            },
        );
        testing::settle(&mut app);
        assert_eq!(testing::count::<OpeningRegion>(&mut app), 0);
        assert_eq!(testing::count::<SectionBody>(&mut app), 1);
    }

    #[test]
    fn header_buttons_and_harness() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        let open = testing::find::<OpenInEditorButton>(&mut app, |_| true);
        testing::activate(&mut app, open);
        let worktree = fixture::demo_review(NOW).0.worktree.expect("a worktree");
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::OpenInEditor {
                path: worktree,
                line: None
            }]
        );
        let github = testing::find::<ViewOnGitHubButton>(&mut app, |_| true);
        testing::activate(&mut app, github);
        assert_eq!(
            app.world().resource::<OpenUrls>().0,
            ["https://github.com/rzorzal/clusia/pull/123"]
        );
        let harness = testing::find::<HarnessButton>(&mut app, |_| true);
        testing::activate(&mut app, harness);
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Config(Section::Harness)
        );
    }

    #[test]
    fn cards_jump_and_buttons_open_the_finalize_modal_and_the_note_editor() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        let ui = |app: &App| app.world().resource::<ReviewTabs>().0[&pr].ui.clone();
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .section = ReviewSection::Tests;
        testing::settle(&mut app);
        let store = testing::find::<DraftCardButton>(&mut app, |c| {
            c.target == CardTarget::Diff("src/auth/store.rs".into())
        });
        testing::activate(&mut app, store);
        assert_eq!(ui(&app).section, ReviewSection::Diff);
        assert_eq!(ui(&app).file.as_deref(), Some("src/auth/store.rs"));
        let finalize = testing::find::<FinalizeButton>(&mut app, |_| true);
        testing::activate(&mut app, finalize);
        assert_eq!(ui(&app).modal, Some(Modal::Finalize));
        let note = testing::find::<GeneralNoteButton>(&mut app, |_| true);
        testing::activate(&mut app, note);
        testing::settle(&mut app);
        assert_eq!(ui(&app).editor.map(|e| e.target), Some(EditTarget::General));
        assert_eq!(
            testing::count::<crate::screens::review::editor::EditorArea>(&mut app),
            1,
            "the general note editor sits in the right panel"
        );
    }

    #[test]
    fn a_general_note_waits_for_an_unsent_editor() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        let line = EditTarget::Line {
            path: "src/auth/refresh.rs".into(),
            side: Side::Right,
            start: None,
            line: 44,
        };
        let set = |app: &mut App, text: &str| {
            // Closed first: the diff draws the line editor, and its text area is what counts.
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0.get_mut(&pr).unwrap().ui.editor = None;
            testing::settle(app);
            app.world_mut()
                .resource_mut::<ReviewTabs>()
                .0
                .get_mut(&pr)
                .unwrap()
                .ui
                .editor = Some(Editor {
                target: line.clone(),
                text: text.into(),
                error: None,
                ticket: None,
            });
            testing::settle(app);
        };
        let editor = |app: &App| {
            app.world().resource::<ReviewTabs>().0[&pr]
                .ui
                .editor
                .clone()
        };
        set(&mut app, "Half a thought");
        let note = testing::find::<GeneralNoteButton>(&mut app, |_| true);
        testing::activate(&mut app, note);
        let kept = editor(&app).unwrap();
        assert_eq!(
            (kept.target, kept.text.as_str()),
            (line.clone(), "Half a thought"),
            "unsent text is never dropped"
        );
        set(&mut app, "  ");
        let note = testing::find::<GeneralNoteButton>(&mut app, |_| true);
        testing::activate(&mut app, note);
        assert_eq!(editor(&app).map(|e| e.target), Some(EditTarget::General));
    }

    #[test]
    fn comments_section_explains_the_draft() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        let copy = "Replies and new threads you write here join the draft. Nothing is posted until you publish.";
        let has = |app: &mut App| {
            let mut q = app.world_mut().query::<&Text>();
            q.iter(app.world()).any(|t| t.0 == copy)
        };
        assert!(!has(&mut app));
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .section = ReviewSection::Comments;
        testing::settle(&mut app);
        assert!(has(&mut app));
    }

    #[test]
    fn cached_copy_is_read_only_with_try_again() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        if let Phase::Ready(ready) = &mut app
            .world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .phase
        {
            ready.cached_at = Some(NOW - 3600);
        }
        testing::settle(&mut app);
        assert_eq!(testing::count::<FinalizeButton>(&mut app), 0, "disabled");
        assert_eq!(testing::count::<GeneralNoteButton>(&mut app), 0);
        let again = testing::find::<TryAgain>(&mut app, |_| true);
        testing::activate(&mut app, again);
        assert_eq!(testing::recorded(&mut app), [Ask::OpenReview(pr.clone())]);
        testing::settle(&mut app);
        let tabs = app.world().resource::<ReviewTabs>();
        assert!(
            matches!(
                &tabs.0[&pr].phase,
                Phase::Loading {
                    cached: Some(_),
                    ..
                }
            ),
            "reloads, keeping the cached copy to show meanwhile"
        );
    }

    #[test]
    fn stale_modals_go_away() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        let modal = app
            .world_mut()
            .spawn((
                crate::ui::modal::modal_root(),
                ModalFor {
                    pr: pr.clone(),
                    modal: Modal::Finalize,
                },
            ))
            .id();
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .modal = Some(Modal::Finalize);
        app.update();
        assert!(
            app.world().get_entity(modal).is_ok(),
            "wanted and on screen"
        );
        app.world_mut().resource_mut::<Nav>().screen = Screen::Home;
        app.update();
        assert!(
            app.world().get_entity(modal).is_err(),
            "the tab is not on screen"
        );
    }

    #[test]
    fn draft_cards_remove_their_item_unless_read_only() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        let obsolete = testing::ready(&app, &pr)
            .view
            .review
            .draft
            .items
            .iter()
            .find(|i| matches!(i.status, ItemStatus::Obsolete { .. }))
            .expect("the demo's obsolete item")
            .id
            .clone();
        assert_eq!(testing::count::<DraftCardRemove>(&mut app), 3, "every card");
        let remove = testing::find::<DraftCardRemove>(&mut app, |r| r.id == obsolete);
        testing::activate(&mut app, remove);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RemoveItem {
                pr: pr.clone(),
                id: obsolete
            }]
        );
        assert_eq!(
            app.world().resource::<ReviewTabs>().0[&pr].ui.file,
            None,
            "the card itself was not clicked"
        );
        app.world_mut().resource_mut::<Model>().connection = Connection::Lost("gone".into());
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<DraftCardRemove>(&mut app),
            0,
            "not connected"
        );
        app.world_mut().resource_mut::<Model>().connection = Connection::Live;
        if let Phase::Ready(ready) = &mut app
            .world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .phase
        {
            ready.cached_at = Some(NOW - 3600);
        }
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<DraftCardRemove>(&mut app),
            0,
            "cached copy"
        );
    }
}
