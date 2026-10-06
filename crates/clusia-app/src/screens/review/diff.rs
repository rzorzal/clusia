//! Diff (spec §7.2, mockups `Review.png` and `DiffSplit.png`):
//! - the file list, then one file's diff in Unified or Split with syntax colors;
//! - open review threads and draft line comments under their lines;
//! - a click on a commentable line opens the inline editor (Shift-click extends the range).
//!
//! Each region rebuilds only when its pure view changes. The editor's typed text is not part
//! of any view, so typing never rebuilds the diff.

use std::collections::HashMap;
use std::ops::Range;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::commentable_lines;
use clusia_core::draft::{DraftKind, ItemStatus, Side, ThreadRef};
use clusia_highlight::{Span, highlight};
use clusia_view::diff::{
    Row, RowKind, anchor_of, intraline, line_on, parse_patch, row_of, side_text, split_rows,
};

use crate::bridge::{Ask, Asks, Model, set_config};
use crate::fonts::UiFonts;
use crate::review_state::{
    DiffMode, EditTarget, Editor, Phase, Ready, ReviewSection, ReviewTabs, SHOW_STEP, TabUi,
};
use crate::screens::review::ReviewSystems;
use crate::screens::review::editor::editor_box;
use crate::screens::review::shell::SectionBody;
use crate::theme::{Swatch, Theme};
use crate::ui::code::{CodeSpan, code_line, spans_for};
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Stroke, Tone, Type, Variant, avatar, badge, button, card, panel,
    segment, segments, text,
};

const NO_DIFF: &str = "No diff to show for this file";

#[derive(Debug, Clone, PartialEq)]
pub struct FileEntry {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnifiedLine {
    /// Index into the file's rows (`parse_patch`).
    pub row: usize,
    pub kind: RowKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub spans: Vec<CodeSpan>,
    /// The row's anchor (`anchor_of`): what a click comments on.
    pub anchor: Option<(Side, u32)>,
    pub commentable: bool,
    /// Inside the range the line editor targets.
    pub selected: bool,
}

/// One side of a Split line.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub row: usize,
    pub number: Option<u32>,
    pub kind: RowKind,
    pub spans: Vec<CodeSpan>,
    /// The row's anchor: a context cell anchors on its new line on both sides.
    pub anchor: Option<(Side, u32)>,
    pub commentable: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SplitLine {
    /// The `@@ … @@` header: shown across both sides; `left`/`right` are `None`.
    pub hunk: Option<String>,
    pub left: Option<Cell>,
    pub right: Option<Cell>,
    pub below_left: Vec<Below>,
    pub below_right: Vec<Below>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadCard {
    pub thread: ThreadRef,
    pub author: String,
    /// First line of the first comment.
    pub first: String,
    pub replies: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DraftCard {
    pub id: String,
    /// "You · line 44" / "You · lines 40–44".
    pub label: String,
    pub body: String,
    /// "moved from 81" (green) or "obsolete" (orange).
    pub status: Option<(String, Tone)>,
}

/// What sits under a line.
#[derive(Debug, Clone, PartialEq)]
pub enum Below {
    Thread(ThreadCard),
    Draft(DraftCard),
    /// Task 9's `editor_box` for the current `ui.editor`.
    Editor,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiffBody {
    Empty(&'static str),
    Unified(Vec<(UnifiedLine, Vec<Below>)>),
    Split {
        /// "Before · main @ 9b1c0e2".
        before: String,
        /// "After · auth-refresh @ 3f2a1c4".
        after: String,
        lines: Vec<SplitLine>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffView {
    pub files: Vec<FileEntry>,
    pub path: Option<String>,
    pub additions: u64,
    pub deletions: u64,
    pub mode: DiffMode,
    pub code_size: f32,
    pub body: DiffBody,
    /// Lines (Unified rows or Split lines) not shown yet.
    pub more: usize,
    /// The editor's refusal, re-rendered under the editor.
    pub editor_error: Option<String>,
}

/// One file's rows highlighted per side, indexed by row.
#[derive(Debug, Clone, Default)]
struct Highlighted {
    left: Vec<Option<Vec<Span>>>,
    right: Vec<Option<Vec<Span>>>,
}

/// Highlighted files by `(head_sha, path)`: a new head re-highlights, a file switch does not.
#[derive(Resource, Default)]
pub struct HighlightCache(HashMap<(String, String), Highlighted>);

impl HighlightCache {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn get(&mut self, head: &str, path: &str, rows: &[Row]) -> &Highlighted {
        self.0
            .entry((head.to_string(), path.to_string()))
            .or_insert_with(|| {
                let mut lit = Highlighted {
                    left: vec![None; rows.len()],
                    right: vec![None; rows.len()],
                };
                for side in [Side::Left, Side::Right] {
                    // Each side is highlighted as one text, so multi-line constructs color right.
                    let text = side_text(rows, side);
                    let lines = highlight(path, &text.text);
                    let out = match side {
                        Side::Left => &mut lit.left,
                        Side::Right => &mut lit.right,
                    };
                    for (i, &row) in text.rows.iter().enumerate() {
                        out[row] = lines.get(i).cloned();
                    }
                }
                lit
            })
    }
}

fn patch_of<'a>(ready: &'a Ready, path: &str) -> Option<&'a str> {
    ready
        .view
        .diff
        .iter()
        .find(|f| f.path == path)
        .and_then(|f| f.patch.as_deref())
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn first_line(body: &str) -> String {
    let line = body
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= 120 {
        return line.to_string();
    }
    let kept: String = line.chars().take(119).collect();
    format!("{}…", kept.trim_end())
}

/// Rows under the line editor's range: side and first/last line.
fn selection(ui: &TabUi, path: &str) -> Option<(Side, u32, u32)> {
    match ui.editor.as_ref().map(|e| &e.target) {
        Some(EditTarget::Line {
            path: p,
            side,
            start,
            line,
        }) if p == path => Some((*side, start.unwrap_or(*line), *line)),
        _ => None,
    }
}

fn is_selected(sel: Option<(Side, u32, u32)>, anchor: Option<(Side, u32)>) -> bool {
    match (sel, anchor) {
        (Some((s, from, to)), Some((side, n))) => s == side && (from..=to).contains(&n),
        _ => false,
    }
}

#[derive(Default)]
struct Placements {
    left: HashMap<usize, Vec<Below>>,
    right: HashMap<usize, Vec<Below>>,
}

impl Placements {
    fn put(&mut self, rows: &[Row], side: Side, line: u32, below: Below) {
        let Some(row) = row_of(rows, side, line) else {
            return; // not in this diff (outdated thread, obsolete item): the right panel has it
        };
        let map = match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
        };
        map.entry(row).or_default().push(below);
    }

    fn take(&mut self, side: Side, row: usize) -> Vec<Below> {
        match side {
            Side::Left => self.left.remove(&row),
            Side::Right => self.right.remove(&row),
        }
        .unwrap_or_default()
    }
}

/// Threads, draft line comments and the editor, placed under their rows.
fn placements(ready: &Ready, ui: &TabUi, path: &str, rows: &[Row]) -> Placements {
    let mut out = Placements::default();
    let target = ui.editor.as_ref().map(|e| &e.target);
    if let Some(conversation) = &ready.view.conversation {
        for t in conversation
            .review_threads
            .iter()
            .filter(|t| t.path == path && !t.is_resolved)
        {
            let (Some(line), Some(first)) = (t.line, t.comments.first()) else {
                continue;
            };
            let thread = ThreadRef {
                id: t.id.clone(),
                author: first.author.clone(),
                path: Some(t.path.clone()),
                line: Some(line),
            };
            out.put(
                rows,
                t.side,
                line,
                Below::Thread(ThreadCard {
                    thread,
                    author: first.author.clone(),
                    first: first_line(&first.body),
                    replies: t.comments.len() - 1,
                }),
            );
            if matches!(target, Some(EditTarget::Reply(r)) if r.id == t.id) {
                out.put(rows, t.side, line, Below::Editor);
            }
        }
    }
    for item in &ready.view.review.draft.items {
        let Some(anchor) = &item.anchor else { continue };
        if item.kind != DraftKind::LineComment || !item.accepted || anchor.path != path {
            continue;
        }
        let below = if matches!(target, Some(EditTarget::Item(id)) if *id == item.id) {
            Below::Editor
        } else {
            let label = match anchor.start_line {
                Some(start) if start < anchor.line => {
                    format!("You · lines {start}–{}", anchor.line)
                }
                _ => format!("You · line {}", anchor.line),
            };
            let status = match &item.status {
                ItemStatus::Ok => None,
                ItemStatus::Moved { from_line, .. } => {
                    Some((format!("moved from {from_line}"), Tone::Green))
                }
                ItemStatus::Obsolete { .. } => Some(("obsolete".to_string(), Tone::Orange)),
            };
            Below::Draft(DraftCard {
                id: item.id.clone(),
                label,
                body: item.body.clone(),
                status,
            })
        };
        out.put(rows, anchor.side, anchor.line, below);
    }
    if let Some(EditTarget::Line {
        path: p,
        side,
        line,
        ..
    }) = target
        && p == path
    {
        out.put(rows, *side, *line, Below::Editor);
    }
    out
}

/// A row's colored spans from one side's highlighting.
fn row_spans(
    row: &Row,
    index: usize,
    side: Side,
    lit: &Highlighted,
    emphasis: Option<(Range<usize>, Swatch)>,
) -> Vec<CodeSpan> {
    if row.kind == RowKind::Hunk {
        return vec![CodeSpan {
            text: row.text.clone(),
            ink: Swatch::Muted,
            mark: None,
        }];
    }
    let spans = match side {
        Side::Left => &lit.left[index],
        Side::Right => &lit.right[index],
    };
    spans_for(&row.text, spans.as_deref().unwrap_or(&[]), emphasis)
}

pub fn diff_view(
    ready: &Ready,
    ui: &TabUi,
    code_size: f32,
    cache: &mut HighlightCache,
) -> DiffView {
    let view = &ready.view;
    // A2: `view.diff` is exactly what the daemon fetched (patches included); `view.files` is
    // only the fallback for a view that carries no patches.
    let listed: Vec<(&str, u64, u64)> = if view.diff.is_empty() {
        view.files
            .iter()
            .map(|f| (f.path.as_str(), f.additions, f.deletions))
            .collect()
    } else {
        view.diff
            .iter()
            .map(|f| (f.path.as_str(), f.additions, f.deletions))
            .collect()
    };
    let selected = ui
        .file
        .clone()
        .filter(|p| listed.iter().any(|f| f.0 == p))
        .or_else(|| listed.first().map(|f| f.0.to_string()));
    let files = listed
        .iter()
        .map(|&(path, additions, deletions)| FileEntry {
            path: path.to_string(),
            additions,
            deletions,
            on: selected.as_deref() == Some(path),
        })
        .collect();
    let (additions, deletions) = selected
        .as_deref()
        .and_then(|p| listed.iter().find(|f| f.0 == p))
        .map_or((0, 0), |f| (f.1, f.2));
    let mut out = DiffView {
        files,
        path: selected.clone(),
        additions,
        deletions,
        mode: ui.mode,
        code_size,
        body: DiffBody::Empty(NO_DIFF),
        more: 0,
        editor_error: ui.editor.as_ref().and_then(|e| e.error.clone()),
    };
    let Some(path) = selected else {
        out.body = DiffBody::Empty("This pull request changes no files");
        return out;
    };
    let Some(patch) = patch_of(ready, &path) else {
        return out;
    };
    let rows = parse_patch(patch);
    if rows.is_empty() {
        return out;
    }
    let (left_ok, right_ok) = commentable_lines(patch);
    let can = |anchor: Option<(Side, u32)>| match anchor {
        Some((Side::Left, n)) => left_ok.contains(&n),
        Some((Side::Right, n)) => right_ok.contains(&n),
        None => false,
    };
    let sel = selection(ui, &path);
    let mut below = placements(ready, ui, &path, &rows);
    let lit = cache.get(&view.pr.head_sha, &path, &rows);
    let limit = ui.shown.max(SHOW_STEP);
    match ui.mode {
        DiffMode::Unified => {
            out.more = rows.len().saturating_sub(limit);
            let lines = rows
                .iter()
                .enumerate()
                .take(limit)
                .map(|(i, row)| {
                    let side = if row.kind == RowKind::Removed {
                        Side::Left
                    } else {
                        Side::Right
                    };
                    let mut under = below.take(Side::Left, i);
                    under.extend(below.take(Side::Right, i));
                    let line = UnifiedLine {
                        row: i,
                        kind: row.kind,
                        old: row.old,
                        new: row.new,
                        spans: row_spans(row, i, side, lit, None),
                        anchor: anchor_of(row),
                        commentable: can(anchor_of(row)),
                        selected: is_selected(sel, anchor_of(row)),
                    };
                    (line, under)
                })
                .collect();
            out.body = DiffBody::Unified(lines);
        }
        DiffMode::Split => {
            let pairs = split_rows(&rows);
            out.more = pairs.len().saturating_sub(limit);
            let cell = |i: usize, side: Side, emphasis: Option<(Range<usize>, Swatch)>| {
                let row = &rows[i];
                // Split: each column comments on its own side (a context line has both).
                let anchor = line_on(row, side).map(|n| (side, n));
                Cell {
                    row: i,
                    number: match side {
                        Side::Left => row.old,
                        Side::Right => row.new,
                    },
                    kind: row.kind,
                    spans: row_spans(row, i, side, lit, emphasis),
                    anchor,
                    commentable: can(anchor),
                    selected: is_selected(sel, anchor),
                }
            };
            let mut lines = Vec::new();
            for pair in pairs.iter().take(limit) {
                if let Some(h) = pair.hunk {
                    lines.push(SplitLine {
                        hunk: Some(rows[h].text.clone()),
                        left: None,
                        right: None,
                        below_left: Vec::new(),
                        below_right: Vec::new(),
                    });
                    continue;
                }
                let marks = match (pair.left, pair.right) {
                    (Some(l), Some(r))
                        if rows[l].kind == RowKind::Removed && rows[r].kind == RowKind::Added =>
                    {
                        intraline(&rows[l].text, &rows[r].text)
                    }
                    _ => None,
                };
                let (left_mark, right_mark) = match marks {
                    Some((a, b)) => (
                        Some((a, Swatch::RemovedStrong)),
                        Some((b, Swatch::AddedStrong)),
                    ),
                    None => (None, None),
                };
                lines.push(SplitLine {
                    hunk: None,
                    left: pair.left.map(|l| cell(l, Side::Left, left_mark)),
                    right: pair.right.map(|r| cell(r, Side::Right, right_mark)),
                    below_left: pair
                        .left
                        .map(|l| below.take(Side::Left, l))
                        .unwrap_or_default(),
                    below_right: pair
                        .right
                        .map(|r| below.take(Side::Right, r))
                        .unwrap_or_default(),
                });
            }
            let pr = &view.pr;
            out.body = DiffBody::Split {
                before: format!("Before · {} @ {}", pr.base_ref, short(&pr.base_sha)),
                after: format!("After · {} @ {}", pr.head_ref, short(&pr.head_sha)),
                lines,
            };
        }
    }
    out
}

/// The Diff section's root inside `SectionBody` (A1: its absence means "fill me").
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DiffRegion(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FileButton {
    pub pr: PrRef,
    pub path: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ModeButton {
    pub pr: PrRef,
    pub mode: DiffMode,
}

/// A commentable line; `side`/`line` are the row's anchor (`anchor_of`).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LineButton {
    pub pr: PrRef,
    pub path: String,
    pub row: usize,
    pub side: Side,
    pub line: u32,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ThreadReply {
    pub pr: PrRef,
    pub thread: ThreadRef,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DraftEdit {
    pub pr: PrRef,
    pub id: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DraftRemove {
    pub pr: PrRef,
    pub id: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ShowMore(pub PrRef);

/// Story and Calls: shown, not usable yet.
#[derive(Component, Debug)]
struct LaterSegment;

#[derive(Component, Debug)]
struct LaterHint;

#[derive(Component)]
struct FilesPart {
    pr: PrRef,
    built: Option<Vec<FileEntry>>,
}

#[derive(Component)]
struct HeaderPart {
    pr: PrRef,
    built: Option<(Option<String>, u64, u64, DiffMode)>,
}

#[derive(Debug, Clone, PartialEq)]
struct BodyKey {
    path: Option<String>,
    body: DiffBody,
    more: usize,
    code_size: f32,
    error: Option<String>,
}

#[derive(Component)]
struct BodyPart {
    pr: PrRef,
    built: Option<BodyKey>,
}

pub struct DiffPlugin;

impl Plugin for DiffPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HighlightCache>().add_systems(
            Update,
            (fill_region, rebuild_diff, later_hints)
                .chain()
                .after(ReviewSystems),
        );
    }
}

/// A1: when the Diff section shows and `SectionBody` has no `DiffRegion`, take it over.
fn fill_region(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    bodies: Query<(Entity, &SectionBody)>,
    regions: Query<&ChildOf, With<DiffRegion>>,
) {
    for (entity, body) in &bodies {
        let Some(tab) = tabs.0.get(&body.pr) else {
            continue;
        };
        if tab.ui.section != ReviewSection::Diff || !matches!(tab.phase, Phase::Ready(_)) {
            continue;
        }
        if regions.iter().any(|child_of| child_of.parent() == entity) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        let pr = body.pr.clone();
        commands.entity(entity).with_children(|p| frame(p, &pr));
    }
}

fn frame(p: &mut ChildSpawnerCommands, pr: &PrRef) {
    p.spawn((
        Node {
            flex_grow: 1.0,
            width: percent(100),
            min_height: px(0),
            ..default()
        },
        DiffRegion(pr.clone()),
    ))
    .with_children(|r| {
        r.spawn((
            Node {
                width: px(258),
                flex_shrink: 0.0,
                flex_direction: FlexDirection::Column,
                row_gap: px(2),
                padding: UiRect::axes(px(10), px(14)),
                border: UiRect::right(px(1)),
                overflow: Overflow::scroll_y(),
                ..default()
            },
            ScrollArea,
            BorderColor::default(),
            Stroke(Swatch::Line),
            FilesPart {
                pr: pr.clone(),
                built: None,
            },
        ));
        r.spawn(Node {
            flex_grow: 1.0,
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            ..default()
        })
        .with_children(|c| {
            c.spawn((
                panel(
                    Node {
                        height: px(46),
                        flex_shrink: 0.0,
                        padding: UiRect::horizontal(px(16)),
                        column_gap: px(8),
                        align_items: AlignItems::Center,
                        border: UiRect::bottom(px(1)),
                        ..default()
                    },
                    Swatch::Surface,
                ),
                BorderColor::default(),
                Stroke(Swatch::Line),
                HeaderPart {
                    pr: pr.clone(),
                    built: None,
                },
            ));
            c.spawn((
                panel(
                    Node {
                        flex_grow: 1.0,
                        min_height: px(0),
                        flex_direction: FlexDirection::Column,
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
        });
    });
}

fn refill(commands: &mut Commands, entity: Entity, build: impl FnOnce(&mut ChildSpawnerCommands)) {
    commands.entity(entity).despawn_related::<Children>();
    commands.entity(entity).with_children(build);
}

fn rebuild_diff(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    theme: Res<Theme>,
    fonts: Res<UiFonts>,
    mut cache: ResMut<HighlightCache>,
    mut files: Query<(Entity, &mut FilesPart)>,
    mut headers: Query<(Entity, &mut HeaderPart)>,
    mut bodies: Query<(Entity, &mut BodyPart, &mut ScrollPosition)>,
) {
    let fresh = files.iter().any(|(_, p)| p.built.is_none())
        || headers.iter().any(|(_, p)| p.built.is_none())
        || bodies.iter().any(|(_, p, _)| p.built.is_none());
    if !(fresh || tabs.is_changed() || theme.is_changed()) {
        return;
    }
    let mut views: HashMap<PrRef, (DiffView, Option<Editor>)> = HashMap::new();
    for pr in files.iter().map(|(_, p)| p.pr.clone()) {
        if let Some(tab) = tabs.0.get(&pr)
            && let Phase::Ready(ready) = &tab.phase
        {
            let v = diff_view(ready, &tab.ui, theme.code_size, &mut cache);
            views.insert(pr, (v, tab.ui.editor.clone()));
        }
    }
    let fonts = &*fonts;
    for (entity, mut part) in &mut files {
        let Some((v, _)) = views.get(&part.pr) else {
            continue;
        };
        if part.built.as_ref() == Some(&v.files) {
            continue;
        }
        let pr = part.pr.clone();
        refill(&mut commands, entity, |p| {
            file_list(p, fonts, &pr, &v.files)
        });
        part.built = Some(v.files.clone());
    }
    for (entity, mut part) in &mut headers {
        let Some((v, _)) = views.get(&part.pr) else {
            continue;
        };
        let key = (v.path.clone(), v.additions, v.deletions, v.mode);
        if part.built.as_ref() == Some(&key) {
            continue;
        }
        let pr = part.pr.clone();
        refill(&mut commands, entity, |p| header(p, fonts, &pr, v));
        part.built = Some(key);
    }
    for (entity, mut part, mut scroll) in &mut bodies {
        let Some((v, editor)) = views.get(&part.pr) else {
            continue;
        };
        let key = BodyKey {
            path: v.path.clone(),
            body: v.body.clone(),
            more: v.more,
            code_size: v.code_size,
            error: v.editor_error.clone(),
        };
        if part.built.as_ref() == Some(&key) {
            continue;
        }
        if part.built.as_ref().map(|b| &b.path) != Some(&key.path) {
            scroll.0 = Vec2::ZERO; // another file starts at its top
        }
        let pr = part.pr.clone();
        let path = v.path.clone().unwrap_or_default();
        refill(&mut commands, entity, |p| {
            body(p, fonts, &pr, &path, v, editor.as_ref());
        });
        part.built = Some(key);
    }
}

fn file_list(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, files: &[FileEntry]) {
    p.spawn(Node {
        padding: UiRect::new(px(8), px(8), px(0), px(8)),
        ..default()
    })
    .with_children(|h| {
        h.spawn(text(fonts, format!("FILES · {}", files.len()), Type::META));
    });
    for f in files {
        p.spawn((
            Node {
                padding: UiRect::axes(px(8), px(6)),
                column_gap: px(8),
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            WidgetButton,
            Hovered::default(),
            TabIndex(0),
            Clickable,
            BackgroundColor::default(),
            Fill(if f.on {
                Swatch::Selected
            } else {
                Swatch::Clear
            }),
            HoverFill(if f.on {
                Swatch::Selected
            } else {
                Swatch::Hover
            }),
            FileButton {
                pr: pr.clone(),
                path: f.path.clone(),
            },
            observe(on_file),
        ))
        .with_children(|r| {
            r.spawn((
                Node {
                    flex_grow: 1.0,
                    min_width: px(0),
                    overflow: Overflow::clip_x(),
                    ..default()
                },
                children![text(fonts, f.path.clone(), Type::MONO.ink(Swatch::Fg))],
            ));
            r.spawn(text(
                fonts,
                format!("+{}", f.additions),
                Type::MONO.ink(Swatch::Green),
            ));
            r.spawn(text(
                fonts,
                format!("−{}", f.deletions),
                Type::MONO.ink(Swatch::Orange),
            ));
        });
    }
}

fn header(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, v: &DiffView) {
    if let Some(path) = &v.path {
        p.spawn(text(
            fonts,
            path.clone(),
            Type::MONO.ink(Swatch::Fg).size(13.0),
        ));
        p.spawn(text(
            fonts,
            format!("+{}", v.additions),
            Type::MONO.ink(Swatch::Green),
        ));
        p.spawn(text(
            fonts,
            format!("−{}", v.deletions),
            Type::MONO.ink(Swatch::Orange),
        ));
    }
    p.spawn(Node {
        flex_grow: 1.0,
        ..default()
    });
    p.spawn((
        text(fonts, "Arrives with SP3 (#71)", Type::META),
        Node {
            display: Display::None,
            ..default()
        },
        LaterHint,
    ));
    p.spawn(segments()).with_children(|s| {
        for (label, mode) in [("Unified", DiffMode::Unified), ("Split", DiffMode::Split)] {
            s.spawn((
                segment(fonts, label, v.mode == mode),
                ModeButton {
                    pr: pr.clone(),
                    mode,
                },
                observe(on_mode),
            ));
        }
        for label in ["Story", "Calls"] {
            s.spawn((
                Node {
                    height: px(26),
                    padding: UiRect::horizontal(px(12)),
                    align_items: AlignItems::Center,
                    border_radius: BorderRadius::all(px(6)),
                    ..default()
                },
                Hovered::default(),
                LaterSegment,
                children![text(fonts, label, Type::MUTED.ink(Swatch::Faint))],
            ));
        }
    });
}

/// Shows "Arrives with SP3 (#71)" while Story or Calls is hovered.
fn later_hints(
    segments: Query<&Hovered, With<LaterSegment>>,
    mut hints: Query<&mut Node, With<LaterHint>>,
) {
    let want = if segments.iter().any(|h| h.get()) {
        Display::Flex
    } else {
        Display::None
    };
    for mut node in &mut hints {
        if node.display != want {
            node.display = want;
        }
    }
}

const NUMBER_WIDTH: f32 = 44.0;

fn fill_of(kind: RowKind, selected: bool) -> Swatch {
    match (selected, kind) {
        (true, _) => Swatch::Selected,
        (_, RowKind::Added) => Swatch::AddedBg,
        (_, RowKind::Removed) => Swatch::RemovedBg,
        (_, RowKind::Hunk) => Swatch::Chrome,
        (_, RowKind::Context) => Swatch::Clear,
    }
}

fn marker(kind: RowKind) -> (&'static str, Swatch) {
    match kind {
        RowKind::Added => ("+", Swatch::Green),
        RowKind::Removed => ("-", Swatch::Orange),
        RowKind::Context | RowKind::Hunk => (" ", Swatch::Faint),
    }
}

fn number(p: &mut ChildSpawnerCommands, fonts: &UiFonts, n: Option<u32>, size: f32) {
    p.spawn(Node {
        width: px(NUMBER_WIDTH),
        flex_shrink: 0.0,
        justify_content: JustifyContent::FlexEnd,
        padding: UiRect::right(px(10)),
        ..default()
    })
    .with_children(|c| {
        c.spawn(text(
            fonts,
            n.map(|n| n.to_string()).unwrap_or_default(),
            Type::MONO.size(size).ink(Swatch::Faint),
        ));
    });
}

fn mark(p: &mut ChildSpawnerCommands, fonts: &UiFonts, kind: RowKind, size: f32) {
    let (sign, ink) = marker(kind);
    p.spawn(Node {
        width: px(18),
        flex_shrink: 0.0,
        ..default()
    })
    .with_children(|c| {
        c.spawn(text(fonts, sign, Type::MONO.size(size).ink(ink)));
    });
}

/// A line's container; commentable lines are buttons that open the editor.
fn line_node(
    p: &mut ChildSpawnerCommands,
    width: Val,
    kind: RowKind,
    selected: bool,
    button: Option<LineButton>,
    build: impl FnOnce(&mut ChildSpawnerCommands),
) {
    let fill = fill_of(kind, selected);
    let mut e = p.spawn((
        Node {
            width,
            min_height: px(22),
            flex_shrink: 0.0,
            align_items: AlignItems::Center,
            overflow: Overflow::clip_x(),
            ..default()
        },
        BackgroundColor::default(),
        Fill(fill),
    ));
    if let Some(b) = button {
        e.insert((
            WidgetButton,
            Hovered::default(),
            Clickable,
            HoverFill(Swatch::Selected),
            b,
            observe(on_line),
        ));
    }
    e.with_children(build);
}

fn body(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    path: &str,
    v: &DiffView,
    editor: Option<&Editor>,
) {
    let size = v.code_size;
    let line_button = |row: usize, anchor: Option<(Side, u32)>, commentable: bool| {
        let (side, line) = anchor.filter(|_| commentable)?;
        Some(LineButton {
            pr: pr.clone(),
            path: path.to_string(),
            row,
            side,
            line,
        })
    };
    match &v.body {
        DiffBody::Empty(message) => {
            p.spawn(Node {
                padding: px(24).all(),
                ..default()
            })
            .with_children(|e| {
                e.spawn(text(fonts, *message, Type::MUTED));
            });
        }
        DiffBody::Unified(lines) => {
            for (line, under) in lines {
                line_node(
                    p,
                    percent(100),
                    line.kind,
                    line.selected,
                    line_button(line.row, line.anchor, line.commentable),
                    |r| {
                        number(r, fonts, line.old, size);
                        number(r, fonts, line.new, size);
                        mark(r, fonts, line.kind, size);
                        r.spawn(code_line(fonts, size, line.spans.clone()));
                    },
                );
                if !under.is_empty() {
                    p.spawn(Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: px(8),
                        padding: UiRect::new(px(2.0 * NUMBER_WIDTH + 18.0), px(16), px(8), px(8)),
                        ..default()
                    })
                    .with_children(|b| {
                        for item in under {
                            below(b, fonts, pr, item, editor);
                        }
                    });
                }
            }
        }
        DiffBody::Split {
            before,
            after,
            lines,
        } => {
            p.spawn((
                Node {
                    flex_shrink: 0.0,
                    border: UiRect::bottom(px(1)),
                    ..default()
                },
                BorderColor::default(),
                Stroke(Swatch::Line),
            ))
            .with_children(|h| {
                for label in [before, after] {
                    h.spawn(Node {
                        width: percent(50),
                        padding: UiRect::axes(px(12), px(8)),
                        ..default()
                    })
                    .with_children(|c| {
                        c.spawn(text(fonts, label.clone(), Type::META));
                    });
                }
            });
            for line in lines {
                p.spawn(Node {
                    width: percent(100),
                    flex_shrink: 0.0,
                    ..default()
                })
                .with_children(|r| {
                    if let Some(hunk) = &line.hunk {
                        for _ in 0..2 {
                            line_node(r, percent(50), RowKind::Hunk, false, None, |c| {
                                c.spawn(Node {
                                    width: px(NUMBER_WIDTH + 18.0),
                                    flex_shrink: 0.0,
                                    ..default()
                                });
                                c.spawn(code_line(
                                    fonts,
                                    size,
                                    vec![CodeSpan {
                                        text: hunk.clone(),
                                        ink: Swatch::Muted,
                                        mark: None,
                                    }],
                                ));
                            });
                        }
                        return;
                    }
                    for cell in [&line.left, &line.right] {
                        match cell {
                            Some(c) => {
                                let b = line_button(c.row, c.anchor, c.commentable);
                                line_node(r, percent(50), c.kind, c.selected, b, |x| {
                                    number(x, fonts, c.number, size);
                                    mark(x, fonts, c.kind, size);
                                    x.spawn(code_line(fonts, size, c.spans.clone()));
                                });
                            }
                            None => {
                                r.spawn(panel(
                                    Node {
                                        width: percent(50),
                                        min_height: px(22),
                                        ..default()
                                    },
                                    Swatch::Bg,
                                ));
                            }
                        }
                    }
                });
                if !(line.below_left.is_empty() && line.below_right.is_empty()) {
                    p.spawn(Node {
                        width: percent(100),
                        flex_shrink: 0.0,
                        ..default()
                    })
                    .with_children(|r| {
                        for items in [&line.below_left, &line.below_right] {
                            r.spawn(Node {
                                width: percent(50),
                                flex_direction: FlexDirection::Column,
                                row_gap: px(8),
                                padding: UiRect::new(px(NUMBER_WIDTH + 10.0), px(16), px(8), px(8)),
                                ..default()
                            })
                            .with_children(|b| {
                                for item in items {
                                    below(b, fonts, pr, item, editor);
                                }
                            });
                        }
                    });
                }
            }
        }
    }
    if v.more > 0 {
        p.spawn(Node {
            padding: UiRect::axes(px(16), px(12)),
            column_gap: px(10),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|m| {
            m.spawn((
                button(fonts, "Show more", Variant::Secondary),
                ShowMore(pr.clone()),
                observe(on_more),
            ));
            m.spawn(text(fonts, format!("{} more lines", v.more), Type::META));
        });
    }
}

fn below(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    item: &Below,
    editor: Option<&Editor>,
) {
    match item {
        Below::Editor => {
            if let Some(editor) = editor {
                editor_box(p, fonts, editor, pr);
            }
        }
        Below::Thread(t) => {
            p.spawn(card(Node {
                padding: UiRect::axes(px(14), px(10)),
                column_gap: px(10),
                align_items: AlignItems::Center,
                ..default()
            }))
            .with_children(|c| {
                c.spawn(avatar(fonts, &t.author));
                c.spawn(text(fonts, format!("@{}", t.author), Type::STRONG));
                c.spawn((
                    Node {
                        flex_grow: 1.0,
                        min_width: px(0),
                        overflow: Overflow::clip_x(),
                        ..default()
                    },
                    children![(
                        text(fonts, t.first.clone(), Type::BODY),
                        TextLayout::no_wrap()
                    )],
                ));
                let replies = match t.replies {
                    0 => String::new(),
                    1 => "1 reply".to_string(),
                    n => format!("{n} replies"),
                };
                if !replies.is_empty() {
                    c.spawn(text(fonts, replies, Type::META));
                }
                c.spawn((
                    button(fonts, "Reply", Variant::Ghost),
                    ThreadReply {
                        pr: pr.clone(),
                        thread: t.thread.clone(),
                    },
                    observe(on_reply),
                ));
            });
        }
        Below::Draft(d) => {
            let obsolete = matches!(&d.status, Some((_, Tone::Orange)));
            p.spawn((
                panel(
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: px(8),
                        padding: UiRect::axes(px(14), px(10)),
                        border: px(1).all(),
                        border_radius: BorderRadius::all(px(8)),
                        ..default()
                    },
                    Swatch::Chrome,
                ),
                BorderColor::default(),
                Stroke(if obsolete {
                    Swatch::Orange
                } else {
                    Swatch::Line
                }),
            ))
            .with_children(|c| {
                c.spawn(Node {
                    column_gap: px(8),
                    align_items: AlignItems::Center,
                    ..default()
                })
                .with_children(|h| {
                    h.spawn(badge(fonts, "Draft", Tone::Green));
                    h.spawn(text(fonts, d.label.clone(), Type::MUTED));
                    if let Some((label, tone)) = &d.status {
                        h.spawn(badge(fonts, label, *tone));
                    }
                    h.spawn(Node {
                        flex_grow: 1.0,
                        ..default()
                    });
                    h.spawn((
                        button(fonts, "Edit", Variant::Ghost),
                        DraftEdit {
                            pr: pr.clone(),
                            id: d.id.clone(),
                        },
                        observe(on_edit),
                    ));
                    h.spawn((
                        button(fonts, "Remove", Variant::Ghost),
                        DraftRemove {
                            pr: pr.clone(),
                            id: d.id.clone(),
                        },
                        observe(on_remove),
                    ));
                });
                c.spawn(text(fonts, d.body.clone(), Type::BODY));
            });
        }
    }
}

fn ui_of<'a>(tabs: &'a mut ReviewTabs, pr: &PrRef) -> Option<&'a mut TabUi> {
    tabs.0.get_mut(pr).map(|t| &mut t.ui)
}

/// An editor with unsent text stays (as for **+ General note**): the user finishes or cancels
/// it before another one opens.
fn unsent(editor: Option<&Editor>) -> bool {
    editor.is_some_and(|e| e.ticket.is_none() && !e.text.trim().is_empty())
}

fn on_file(activate: On<Activate>, buttons: Query<&FileButton>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(ui) = ui_of(&mut tabs, &b.pr) {
        ui.file = Some(b.path.clone());
        ui.shown = SHOW_STEP;
    }
}

fn on_mode(
    activate: On<Activate>,
    buttons: Query<&ModeButton>,
    mut tabs: ResMut<ReviewTabs>,
    mut asks: ResMut<Asks>,
    mut model: ResMut<Model>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    let Some(ui) = ui_of(&mut tabs, &b.pr) else {
        return;
    };
    if ui.mode == b.mode {
        return;
    }
    ui.mode = b.mode;
    ui.shown = SHOW_STEP;
    let value = match b.mode {
        DiffMode::Unified => "unified",
        DiffMode::Split => "split",
    };
    set_config(&mut asks, &mut model, "appearance.diff_view", value);
}

fn on_line(
    activate: On<Activate>,
    buttons: Query<&LineButton>,
    keys: Res<ButtonInput<KeyCode>>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    let Some(ui) = ui_of(&mut tabs, &b.pr) else {
        return;
    };
    let shift = keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]);
    if shift
        && let Some(editor) = &mut ui.editor
        && let EditTarget::Line {
            path,
            side,
            start,
            line,
        } = &mut editor.target
        && *path == b.path
        && *side == b.side
    {
        let from = start.unwrap_or(*line).min(b.line);
        let to = (*line).max(b.line);
        *start = (from < to).then_some(from);
        *line = to;
        editor.error = None;
        return;
    }
    if unsent(ui.editor.as_ref()) {
        return;
    }
    ui.editor = Some(Editor {
        target: EditTarget::Line {
            path: b.path.clone(),
            side: b.side,
            start: None,
            line: b.line,
        },
        text: String::new(),
        error: None,
        ticket: None,
    });
}

fn on_reply(activate: On<Activate>, buttons: Query<&ThreadReply>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(ui) = ui_of(&mut tabs, &b.pr)
        && !unsent(ui.editor.as_ref())
    {
        ui.editor = Some(Editor {
            target: EditTarget::Reply(b.thread.clone()),
            text: String::new(),
            error: None,
            ticket: None,
        });
    }
}

fn on_edit(activate: On<Activate>, buttons: Query<&DraftEdit>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    let Some(tab) = tabs.0.get_mut(&b.pr) else {
        return;
    };
    let Phase::Ready(ready) = &tab.phase else {
        return;
    };
    let Some(item) = ready.view.review.draft.get(&b.id) else {
        return;
    };
    if unsent(tab.ui.editor.as_ref()) {
        return;
    }
    let text = item.body.clone();
    tab.ui.editor = Some(Editor {
        target: EditTarget::Item(b.id.clone()),
        text,
        error: None,
        ticket: None,
    });
}

fn on_remove(activate: On<Activate>, buttons: Query<&DraftRemove>, mut asks: ResMut<Asks>) {
    if let Ok(b) = buttons.get(activate.entity) {
        asks.send(Ask::RemoveItem {
            pr: b.pr.clone(),
            id: b.id.clone(),
        });
    }
}

fn on_more(activate: On<Activate>, buttons: Query<&ShowMore>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(ShowMore(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(ui) = ui_of(&mut tabs, pr) {
        ui.shown = ui.shown.max(SHOW_STEP) + SHOW_STEP;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Tell;
    use crate::fixture;
    use crate::screens::review::editor::{EditorArea, EditorSubmit};
    use crate::testing::{self, NOW};
    use bevy::text::EditableText;
    use clusia_core::FileDiff;
    use clusia_protocol::AnchorInput;
    use clusia_view::diff::{commentable, line_on};

    const REFRESH: &str = "src/auth/refresh.rs";

    fn ready() -> Ready {
        let (view, news) = fixture::demo_review(NOW);
        Ready {
            view,
            news,
            cached_at: None,
        }
    }

    fn view_of(ready: &Ready, ui: &TabUi) -> DiffView {
        diff_view(ready, ui, 13.0, &mut HighlightCache::default())
    }

    fn patch(ready: &Ready, path: &str) -> String {
        ready
            .view
            .diff
            .iter()
            .find(|f| f.path == path)
            .and_then(|f| f.patch.clone())
            .expect("demo patch")
    }

    fn unified(v: &DiffView) -> &[(UnifiedLine, Vec<Below>)] {
        match &v.body {
            DiffBody::Unified(lines) => lines,
            other => panic!("expected Unified, got {other:?}"),
        }
    }

    fn text_of(spans: &[CodeSpan]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// The demo review open and ready, without What's new (`open_ready(.., false)`).
    fn review_app() -> App {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        app
    }

    fn ui_mut<'a>(app: &'a mut App, pr: &PrRef) -> Mut<'a, ReviewTabs> {
        let tabs = app.world_mut().resource_mut::<ReviewTabs>();
        assert!(tabs.0.contains_key(pr));
        tabs
    }

    fn has_text(app: &mut App, needle: &str) -> bool {
        let mut q = app.world_mut().query::<&Text>();
        q.iter(app.world()).any(|t| t.0.contains(needle))
    }

    #[test]
    fn demo_diff_view_unified() {
        let r = ready();
        let v = view_of(&r, &TabUi::default());
        assert_eq!(v.files.len(), 7);
        assert!(v.files[0].on && v.files.iter().skip(1).all(|f| !f.on));
        assert_eq!(
            (
                v.files[0].path.as_str(),
                v.files[0].additions,
                v.files[0].deletions
            ),
            (REFRESH, 9, 2)
        );
        assert_eq!(v.path.as_deref(), Some(REFRESH));
        assert_eq!(
            (v.additions, v.deletions, v.mode),
            (9, 2, DiffMode::Unified)
        );
        assert_eq!(v.more, 0);
        let lines = unified(&v);
        let (hunk, _) = &lines[0];
        assert_eq!(hunk.kind, RowKind::Hunk);
        assert!(text_of(&hunk.spans).starts_with("@@ -38,9 +38,11 @@ impl TokenStore {"));
        assert!(!hunk.commentable);
        let removed = lines
            .iter()
            .find(|(l, _)| l.kind == RowKind::Removed)
            .expect("a removed line");
        assert_eq!((removed.0.old, removed.0.new), (Some(39), None));
        assert!(removed.0.commentable);
        // Syntax colors: `let` is a keyword somewhere on the removed line.
        assert!(
            removed.0.spans.iter().any(|s| s.text.trim() == "let"
                && s.ink == Swatch::Code(clusia_highlight::Class::Keyword))
        );
        // mona's thread sits under new line 41, the draft comment under new line 44.
        let (at41, below41) = lines
            .iter()
            .find(|(l, _)| l.new == Some(41) && l.kind == RowKind::Added)
            .expect("new line 41");
        assert!(at41.commentable);
        let Below::Thread(thread) = &below41[0] else {
            panic!("a thread under 41: {below41:?}")
        };
        assert_eq!(thread.author, "mona");
        assert_eq!(thread.first, "Why one minute? The CLI uses 30 seconds.");
        assert_eq!(thread.replies, 2, "octo and hubot answered");
        assert_eq!(thread.thread.path.as_deref(), Some(REFRESH));
        assert_eq!(thread.thread.line, Some(41));
        let (_, below44) = lines
            .iter()
            .find(|(l, _)| l.new == Some(44))
            .expect("new line 44");
        let Below::Draft(draft) = &below44[0] else {
            panic!("a draft under 44: {below44:?}")
        };
        assert_eq!(draft.label, "You · line 44");
        assert!(
            draft
                .body
                .starts_with("Holding the lock across the network call")
        );
        assert_eq!(draft.status, None);
    }

    #[test]
    fn split_pairs_lines_and_marks_what_changed() {
        let r = ready();
        let ui = TabUi {
            mode: DiffMode::Split,
            ..TabUi::default()
        };
        let v = view_of(&r, &ui);
        let DiffBody::Split {
            before,
            after,
            lines,
        } = &v.body
        else {
            panic!("expected Split")
        };
        assert!(before.starts_with("Before · main @ "), "{before}");
        assert!(after.starts_with("After · "), "{after}");
        assert!(lines[0].hunk.is_some());
        let pair = lines
            .iter()
            .find(|l| {
                l.left
                    .as_ref()
                    .is_some_and(|c| c.number == Some(39) && c.kind == RowKind::Removed)
            })
            .expect("old 39 paired");
        let right = pair.right.as_ref().expect("new 39 beside it");
        assert_eq!((right.number, right.kind), (Some(39), RowKind::Added));
        // `self.load()?;` → `self.load().await?;`: a pure insertion, nothing to mark on the left.
        let left = pair.left.as_ref().unwrap();
        assert!(left.spans.iter().all(|s| s.mark.is_none()));
        assert!(
            right
                .spans
                .iter()
                .any(|s| s.mark == Some(Swatch::AddedStrong))
        );
        // The second pair (`if token…` → `// Refresh…`) changes on both sides.
        assert!(lines.iter().any(|l| {
            l.left.as_ref().is_some_and(|c| {
                c.spans
                    .iter()
                    .any(|s| s.mark == Some(Swatch::RemovedStrong))
            })
        }));
        let marked: String = right
            .spans
            .iter()
            .filter(|s| s.mark.is_some())
            .map(|s| s.text.as_str())
            .collect();
        assert!(marked.contains(".await"), "{marked}");
        // Context rows show on both sides with their own numbers.
        let context = lines
            .iter()
            .find(|l| l.left.as_ref().is_some_and(|c| c.kind == RowKind::Context))
            .unwrap();
        assert_eq!(context.left.as_ref().unwrap().number, Some(38));
        assert_eq!(context.right.as_ref().unwrap().number, Some(38));
        // Each column anchors on its own side.
        assert_eq!(
            context.left.as_ref().unwrap().anchor,
            Some((Side::Left, 38))
        );
        assert_eq!(
            context.right.as_ref().unwrap().anchor,
            Some((Side::Right, 38))
        );
        // The thread on new 41 goes to the right column.
        assert!(
            lines
                .iter()
                .any(|l| matches!(l.below_right.first(), Some(Below::Thread(_))))
        );
        assert!(lines.iter().all(|l| l.below_left.is_empty()));
    }

    #[test]
    fn long_files_show_500_lines_at_a_time() {
        let mut r = ready();
        let body: String = (1..=1200).map(|n| format!("+line {n}\n")).collect();
        r.view.diff.push(FileDiff {
            path: "big.txt".into(),
            previous_path: None,
            status: "added".into(),
            additions: 1200,
            deletions: 0,
            patch: Some(format!("@@ -0,0 +1,1200 @@\n{body}")),
        });
        let mut ui = TabUi {
            file: Some("big.txt".into()),
            ..TabUi::default()
        };
        let v = view_of(&r, &ui);
        assert_eq!(unified(&v).len(), SHOW_STEP);
        assert_eq!(v.more, 1201 - SHOW_STEP, "the hunk row counts as a line");
        ui.shown = 2 * SHOW_STEP;
        assert_eq!(view_of(&r, &ui).more, 1201 - 2 * SHOW_STEP);
        ui.mode = DiffMode::Split;
        let v = view_of(&r, &ui);
        assert_eq!(v.more, 1201 - 2 * SHOW_STEP);
    }

    #[test]
    fn files_without_a_patch_say_so() {
        let mut r = ready();
        r.view.diff.retain(|f| f.path != "CHANGELOG.md");
        r.view.diff.push(FileDiff {
            path: "CHANGELOG.md".into(),
            previous_path: None,
            status: "modified".into(),
            additions: 3,
            deletions: 10,
            patch: None,
        });
        let ui = TabUi {
            file: Some("CHANGELOG.md".into()),
            ..TabUi::default()
        };
        let v = view_of(&r, &ui);
        assert_eq!(v.body, DiffBody::Empty("No diff to show for this file"));
        assert_eq!((v.additions, v.deletions), (3, 10));
    }

    #[test]
    fn the_line_editor_selects_its_range() {
        let r = ready();
        let ui = TabUi {
            editor: Some(Editor {
                target: EditTarget::Line {
                    path: REFRESH.into(),
                    side: Side::Right,
                    start: Some(39),
                    line: 41,
                },
                text: String::new(),
                error: None,
                ticket: None,
            }),
            ..TabUi::default()
        };
        let v = view_of(&r, &ui);
        let selected: Vec<u32> = unified(&v)
            .iter()
            .filter(|(l, _)| l.selected)
            .filter_map(|(l, _)| l.new)
            .collect();
        assert_eq!(selected, [39, 40, 41]);
        let (_, below) = unified(&v)
            .iter()
            .find(|(l, _)| l.new == Some(41) && l.kind == RowKind::Added)
            .unwrap();
        assert!(
            below.contains(&Below::Editor),
            "the editor sits under the range"
        );
    }

    #[test]
    fn highlight_cache_reuses_per_head_and_path() {
        let r = ready();
        let mut cache = HighlightCache::default();
        assert!(cache.is_empty());
        diff_view(&r, &TabUi::default(), 13.0, &mut cache);
        diff_view(&r, &TabUi::default(), 14.0, &mut cache);
        assert_eq!(cache.len(), 1);
        let other = TabUi {
            file: Some("src/auth/store.rs".into()),
            ..TabUi::default()
        };
        diff_view(&r, &other, 13.0, &mut cache);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn only_commentable_lines_open_the_editor() {
        for mode in [DiffMode::Unified, DiffMode::Split] {
            let mut app = review_app();
            let pr = fixture::demo_pr();
            ui_mut(&mut app, &pr).0.get_mut(&pr).unwrap().ui.mode = mode;
            testing::settle(&mut app);
            let r = ready();
            let patch = patch(&r, REFRESH);
            let rows = parse_patch(&patch);
            let mut want: Vec<(bool, u32)> = match mode {
                DiffMode::Unified => rows
                    .iter()
                    .filter(|row| commentable(&patch, row))
                    .filter_map(anchor_of)
                    .map(|(side, n)| (side == Side::Right, n))
                    .collect(),
                DiffMode::Split => rows
                    .iter()
                    .flat_map(|row| {
                        [Side::Left, Side::Right]
                            .into_iter()
                            .filter_map(move |side| line_on(row, side).map(|n| (side, n)))
                    })
                    .filter(|&(side, n)| clusia_core::can_comment(&patch, side, n))
                    .map(|(side, n)| (side == Side::Right, n))
                    .collect(),
            };
            want.sort();
            want.dedup();
            let mut q = app.world_mut().query::<&LineButton>();
            let mut got: Vec<(bool, u32)> = q
                .iter(app.world())
                .map(|b| (b.side == Side::Right, b.line))
                .collect();
            got.sort();
            got.dedup();
            assert_eq!(got, want, "{mode:?}");
            for b in q.iter(app.world()) {
                assert!(
                    clusia_core::can_comment(&patch, b.side, b.line),
                    "{mode:?} {}",
                    b.line
                );
            }
            let line =
                testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == 44);
            testing::activate(&mut app, line);
            assert_eq!(
                testing::tab(&app, &pr).ui.editor.map(|e| e.target),
                Some(EditTarget::Line {
                    path: REFRESH.into(),
                    side: Side::Right,
                    start: None,
                    line: 44
                })
            );
            testing::settle(&mut app);
            assert_eq!(testing::count::<EditorArea>(&mut app), 1, "{mode:?}");
        }
    }

    #[test]
    fn shift_click_extends_the_range() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        let first =
            testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == 39);
        testing::activate(&mut app, first);
        testing::settle(&mut app);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::ShiftLeft);
        let last = testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == 41);
        testing::activate(&mut app, last);
        assert_eq!(
            testing::tab(&app, &pr).ui.editor.map(|e| e.target),
            Some(EditTarget::Line {
                path: REFRESH.into(),
                side: Side::Right,
                start: Some(39),
                line: 41
            })
        );
        // Shift-click upwards keeps the range ordered.
        let up = testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == 38);
        testing::activate(&mut app, up);
        assert_eq!(
            testing::tab(&app, &pr).ui.editor.map(|e| e.target),
            Some(EditTarget::Line {
                path: REFRESH.into(),
                side: Side::Right,
                start: Some(38),
                line: 41
            })
        );
        // The other side starts over.
        let left = testing::find::<LineButton>(&mut app, |b| b.side == Side::Left && b.line == 39);
        testing::activate(&mut app, left);
        assert_eq!(
            testing::tab(&app, &pr).ui.editor.map(|e| e.target),
            Some(EditTarget::Line {
                path: REFRESH.into(),
                side: Side::Left,
                start: None,
                line: 39
            })
        );
    }

    fn comment_on_44(app: &mut App, body: &str) -> u64 {
        let line = testing::find::<LineButton>(app, |b| b.side == Side::Right && b.line == 44);
        testing::activate(app, line);
        testing::settle(app);
        let area = testing::find::<EditorArea>(app, |_| true);
        testing::type_into(app, area, body);
        let submit = testing::find::<EditorSubmit>(app, |_| true);
        testing::activate(app, submit);
        let asks = testing::recorded(app);
        let Some(Ask::AddItem {
            kind,
            anchor,
            thread,
            body: sent,
            ticket,
            ..
        }) = asks.into_iter().find(|a| matches!(a, Ask::AddItem { .. }))
        else {
            panic!("an AddItem ask")
        };
        assert_eq!(kind, DraftKind::LineComment);
        assert_eq!(
            anchor,
            Some(AnchorInput {
                path: REFRESH.into(),
                line: 44,
                start_line: None,
                side: Side::Right
            })
        );
        assert_eq!((thread, sent.as_str()), (None, body));
        ticket
    }

    #[test]
    fn refused_comment_keeps_the_text() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        let ticket = comment_on_44(&mut app, "Drop the guard before the exchange.");
        let message = "line 44 of src/auth/refresh.rs is not part of this pull request's diff";
        testing::tell(
            &mut app,
            Tell::Refused {
                pr: pr.clone(),
                ticket,
                message: message.into(),
            },
        );
        testing::settle(&mut app);
        let editor = testing::tab(&app, &pr)
            .ui
            .editor
            .expect("the editor stays open");
        assert_eq!(editor.error.as_deref(), Some(message));
        assert_eq!(testing::count::<EditorArea>(&mut app), 1);
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        assert_eq!(
            app.world()
                .get::<EditableText>(area)
                .unwrap()
                .value()
                .to_string(),
            "Drop the guard before the exchange."
        );
        assert!(has_text(&mut app, message));
    }

    #[test]
    fn saved_comment_closes_the_editor() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        let ticket = comment_on_44(&mut app, "Drop the guard before the exchange.");
        testing::tell(
            &mut app,
            Tell::Saved {
                pr: pr.clone(),
                ticket,
            },
        );
        testing::settle(&mut app);
        assert_eq!(testing::tab(&app, &pr).ui.editor, None);
        assert_eq!(testing::count::<EditorArea>(&mut app), 0);
    }

    #[test]
    fn files_modes_threads_and_drafts() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        assert_eq!(testing::count::<FileButton>(&mut app), 7);
        assert_eq!(testing::count::<DiffRegion>(&mut app), 1);

        let split = testing::find::<ModeButton>(&mut app, |m| m.mode == DiffMode::Split);
        testing::activate(&mut app, split);
        assert_eq!(testing::tab(&app, &pr).ui.mode, DiffMode::Split);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "appearance.diff_view".into(),
                value: "split".into()
            }]
        );

        let reply = testing::find::<ThreadReply>(&mut app, |_| true);
        testing::activate(&mut app, reply);
        let target = testing::tab(&app, &pr).ui.editor.map(|e| e.target);
        let Some(EditTarget::Reply(thread)) = target else {
            panic!("a reply editor, got {target:?}")
        };
        assert_eq!((thread.author.as_str(), thread.line), ("mona", Some(41)));
        testing::settle(&mut app);
        assert_eq!(testing::count::<EditorArea>(&mut app), 1);

        let remove = testing::find::<DraftRemove>(&mut app, |_| true);
        let id = app.world().get::<DraftRemove>(remove).unwrap().id.clone();
        testing::activate(&mut app, remove);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RemoveItem {
                pr: pr.clone(),
                id: id.clone()
            }]
        );
        let edit = testing::find::<DraftEdit>(&mut app, |_| true);
        testing::activate(&mut app, edit);
        let editor = testing::tab(&app, &pr).ui.editor.unwrap();
        assert_eq!(editor.target, EditTarget::Item(id));
        assert!(editor.text.starts_with("Holding the lock"));

        let store = testing::find::<FileButton>(&mut app, |f| f.path == "src/auth/store.rs");
        testing::activate(&mut app, store);
        let ui = testing::tab(&app, &pr).ui;
        assert_eq!(
            (ui.file.as_deref(), ui.shown),
            (Some("src/auth/store.rs"), SHOW_STEP)
        );
        testing::settle(&mut app);
        assert!(has_text(&mut app, "src/auth/store.rs"));
    }

    #[test]
    fn another_click_never_drops_unsent_text() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        let line = testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == 44);
        testing::activate(&mut app, line);
        testing::settle(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        testing::type_into(&mut app, area, "Half a thought");
        testing::settle(&mut app);
        let other =
            testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == 39);
        testing::activate(&mut app, other);
        let edit = testing::find::<DraftEdit>(&mut app, |_| true);
        testing::activate(&mut app, edit);
        testing::settle(&mut app);
        let editor = testing::tab(&app, &pr).ui.editor.expect("still open");
        assert_eq!(
            (editor.target, editor.text.as_str()),
            (
                EditTarget::Line {
                    path: REFRESH.into(),
                    side: Side::Right,
                    start: None,
                    line: 44
                },
                "Half a thought"
            )
        );
    }

    #[test]
    fn leaving_the_section_hands_the_body_back() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        assert_eq!(testing::count::<DiffRegion>(&mut app), 1);
        ui_mut(&mut app, &pr).0.get_mut(&pr).unwrap().ui.section = ReviewSection::Security;
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<DiffRegion>(&mut app),
            0,
            "Task 9's placeholder took over"
        );
        ui_mut(&mut app, &pr).0.get_mut(&pr).unwrap().ui.section = ReviewSection::Diff;
        testing::settle(&mut app);
        assert_eq!(testing::count::<DiffRegion>(&mut app), 1);
        assert_eq!(testing::count::<FileButton>(&mut app), 7);
    }
}
