//! Diff (spec §7.2, mockups `Review.png` and `DiffSplit.png`):
//! - the file list, then one file's diff in Unified or Split with syntax colors;
//! - the switch to Story and Calls, which have their own frames (`story`, `calls`);
//! - open review threads and draft line comments under their lines;
//! - a click on a commentable line opens the inline editor (Shift-click extends the range).
//!
//! Each region rebuilds only when its pure view changes. The editor's typed text is not part
//! of any view, so typing never rebuilds the diff. The body keeps its rows: a selection or a
//! focus is set on the rows that are there, an editor or card is added or removed under its
//! row alone, a line whose content changed is respawned in its place, and more lines append.

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
use clusia_protocol::AnchorInput;
use clusia_view::diff::{
    Row, RowKind, anchor_of, intraline, line_on, parse_patch, row_of, split_rows,
};

use crate::bridge::{Ask, Asks, Model, Toasts, set_config};
use crate::fonts::UiFonts;
use crate::review_state::{
    DiffMode, EditTarget, Editor, Phase, Ready, ReviewSection, ReviewTabs, SHOW_STEP, Tab, TabUi,
};
use crate::screens::review::ReviewSystems;
use crate::screens::review::editor::{editor_box, not_sent, read_only_reason};
use crate::screens::review::highlight::{HighlightCache, Highlighted, HighlightsLanded};
use crate::screens::review::revalidation::{Revalidations, reanchor_click};
use crate::screens::review::shell::SectionBody;
use crate::theme::{Swatch, Theme};
use crate::ui::code::{CodeSpan, plain_text, respan, spans_for, wrapped_code_line};
use crate::ui::code_select::{ClickKind, CodeSelection, LineKey, selectable};
use crate::ui::composer::ComposerMode;
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Preview, Reveal, SideTrack, Stroke, Tone, Type, Variant, badge,
    button, card, panel, segment, segments, text, wrapped_text, wrapping_button,
};
use crate::ui::markdown::parse::parse_in;
use crate::ui::markdown::{RenderOpts, copy_button, markdown, markdown_line};
use crate::ui::people::{person_avatar, person_name};

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
    /// Every comment of the thread, oldest first.
    pub posts: Vec<CardPost>,
    pub replies: usize,
    /// Drawn in full instead of as the first line of its first comment.
    pub expanded: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CardPost {
    pub author: String,
    /// Markdown source.
    pub body: String,
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
    /// The `editor_box` for the current `ui.editor`.
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
    /// The index (among the listed lines) of the line `TabUi::focus_line` asks for.
    pub focus_row: Option<usize>,
    /// `TabUi::focus_seq`: a new request scrolls even to the row that was shown.
    pub focus_seq: u32,
}

fn patch_of<'a>(ready: &'a Ready, path: &str) -> Option<&'a str> {
    ready
        .view
        .diff
        .iter()
        .find(|f| f.path == path)
        .and_then(|f| f.patch.as_deref())
}

/// Rows a step shows at most; the rest is counted, not drawn.
pub(crate) const STEP_ROWS: usize = 60;

/// The rows of a file's patch that a step's head-side `ranges` select: the added and context
/// lines whose head line is in a range, and each run of removed lines right before a selected
/// row (or right after one, at the end of a hunk). Indices ascend.
pub(crate) fn rows_in_ranges(rows: &[Row], ranges: &[(u32, u32)]) -> Vec<usize> {
    let hit = |row: &Row| {
        row.kind != RowKind::Hunk
            && row.kind != RowKind::Removed
            && row
                .new
                .is_some_and(|n| ranges.iter().any(|&(a, b)| a <= n && n <= b.max(a)))
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        if rows[i].kind != RowKind::Removed {
            if hit(&rows[i]) {
                out.push(i);
            }
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && rows[i].kind == RowKind::Removed {
            i += 1;
        }
        let after = rows.get(i).filter(|r| r.kind != RowKind::Hunk);
        let take = match after {
            Some(next) => hit(next),
            None => start > 0 && hit(&rows[start - 1]),
        };
        if take {
            out.extend(start..i);
        }
    }
    out
}

/// A step's lines of one file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Excerpt {
    pub path: String,
    pub lines: Vec<(UnifiedLine, Vec<Below>)>,
    /// Rows past `STEP_ROWS`.
    pub hidden: usize,
}

/// The rows of `path` that `ranges` select, built as the Unified view builds them. Threads and
/// the open editor sit under their rows; draft comments do not (a step lists them apart).
/// `None` when the file is not in the diff or no row is in a range.
pub(crate) fn excerpt(
    ready: &Ready,
    ui: &TabUi,
    cache: &mut HighlightCache,
    path: &str,
    ranges: &[(u32, u32)],
) -> Option<Excerpt> {
    let patch = patch_of(ready, path)?;
    let rows = parse_patch(patch);
    let picked = rows_in_ranges(&rows, ranges);
    if picked.is_empty() {
        return None;
    }
    let (left_ok, right_ok) = commentable_lines(patch);
    let can = |anchor: Option<(Side, u32)>| match anchor {
        Some((Side::Left, n)) => left_ok.contains(&n),
        Some((Side::Right, n)) => right_ok.contains(&n),
        None => false,
    };
    let sel = selection(ui, path);
    let mut below = placements(ready, ui, path, &rows);
    let lit = &cache.get(&ready.view.pr.head_sha, path, &rows);
    let lines = picked
        .iter()
        .take(STEP_ROWS)
        .map(|&i| {
            let mut under = below.take(Side::Left, i);
            under.extend(below.take(Side::Right, i));
            under.retain(|b| !matches!(b, Below::Draft(_)));
            (unified_line(&rows, i, lit, &can, sel), under)
        })
        .collect();
    Some(Excerpt {
        path: path.to_string(),
        lines,
        hidden: picked.len().saturating_sub(STEP_ROWS),
    })
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// Rows under the line editor's range: side and first/last line.
fn selection(ui: &TabUi, path: &str) -> Option<(Side, u32, u32)> {
    if let Some((p, side, start, line)) = ui.editor.as_ref().and_then(|e| e.target.line_place()) {
        return (p == path).then(|| (side, start.unwrap_or(line), line));
    }
    let (side, line) = ui.focus_line?;
    Some((side, line, line))
}

/// The index of the line `side`/`line` among the lines the view lists (`rows` in Unified, the
/// paired lines in Split).
fn display_index(rows: &[Row], mode: DiffMode, side: Side, line: u32) -> Option<usize> {
    let row = row_of(rows, side, line)?;
    match mode {
        DiffMode::Unified => Some(row),
        DiffMode::Split => split_rows(rows)
            .iter()
            .position(|p| p.left == Some(row) || p.right == Some(row)),
        DiffMode::Story | DiffMode::Calls => None,
    }
}

/// Whether `path` is a file of the diff with a patch to show.
pub fn in_diff(ready: &Ready, path: &str) -> bool {
    patch_of(ready, path).is_some_and(|p| !parse_patch(p).is_empty())
}

/// Shows `path` in Diff. With `lines` (first and last line of a range on the head side) the
/// first of them that is in the patch is lit and scrolled to, and enough lines are shown to
/// reach it. A file or range that is not in the diff leaves the file shown without a line.
/// Returns `false`, changing nothing, when `path` is not in the diff.
pub fn show_line(tab: &mut Tab, path: &str, lines: Option<(u32, u32)>) -> bool {
    let Some(ready) = tab.ready() else {
        return false;
    };
    let Some(patch) = patch_of(ready, path) else {
        return false;
    };
    let rows = parse_patch(patch);
    if rows.is_empty() {
        return false;
    }
    let mode = tab.ui.mode;
    // The range comes from an agent and may span billions of numbers: look at the patch's rows.
    let found = lines.and_then(|(first, last)| {
        let n = rows
            .iter()
            .filter_map(|r| line_on(r, Side::Right))
            .filter(|n| (first..=last.max(first)).contains(n))
            .min()?;
        display_index(&rows, mode, Side::Right, n).map(|index| (n, index))
    });
    super::agent::suggestion::show_file(&mut tab.ui, path);
    tab.ui.focus_line = found.map(|(n, _)| (Side::Right, n));
    if found.is_some() {
        tab.ui.focus_seq = tab.ui.focus_seq.wrapping_add(1);
    }
    if let Some((_, index)) = found {
        tab.ui.shown = tab.ui.shown.max((index / SHOW_STEP + 1) * SHOW_STEP);
    }
    true
}

/// What a diff row costs in height at least; a wrapped line makes its row taller.
const ROW_HEIGHT: f32 = 22.0;
/// Where the text of a row starts, from its top. The number, the sign and the code share it, so a
/// wrapped line keeps them on its first visual line while a single line still sits mid-row.
const TEXT_TOP: f32 = 3.0;

/// Lines of context kept above a line that is scrolled to.
const FOCUS_CONTEXT: usize = 3;

/// The scroll offset that brings the line at `index` into view with a few lines above it. Wrapped
/// lines and comments above it make rows taller, so this is a first guess that `reach_focus`
/// corrects once the rows are laid out.
fn focus_scroll(index: usize) -> f32 {
    index.saturating_sub(FOCUS_CONTEXT) as f32 * ROW_HEIGHT
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
                    posts: t
                        .comments
                        .iter()
                        .map(|c| CardPost {
                            author: c.author.clone(),
                            body: c.body.clone(),
                        })
                        .collect(),
                    replies: t.comments.len() - 1,
                    expanded: ui.expanded.contains(&t.id),
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
    if let Some((p, side, _, line)) = target.and_then(|t| t.line_place())
        && p == path
    {
        out.put(rows, side, line, Below::Editor);
    }
    out
}

/// A row's colored spans from one side's highlighting.
/// One Unified row as the view lists it: its numbers, colored spans, anchor and whether a click
/// can comment on it.
pub(crate) fn unified_line(
    rows: &[Row],
    index: usize,
    lit: &Highlighted,
    can: &dyn Fn(Option<(Side, u32)>) -> bool,
    sel: Option<(Side, u32, u32)>,
) -> UnifiedLine {
    let row = &rows[index];
    let side = if row.kind == RowKind::Removed {
        Side::Left
    } else {
        Side::Right
    };
    UnifiedLine {
        row: index,
        kind: row.kind,
        old: row.old,
        new: row.new,
        spans: row_spans(row, index, side, lit, None),
        anchor: anchor_of(row),
        commentable: can(anchor_of(row)),
        selected: is_selected(sel, anchor_of(row)),
    }
}

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
    // `view.diff` is exactly what the daemon fetched (patches included); `view.files` is
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
        focus_row: None,
        focus_seq: ui.focus_seq,
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
    out.focus_row = ui
        .focus_line
        .and_then(|(side, line)| display_index(&rows, ui.mode, side, line));
    let mut below = placements(ready, ui, &path, &rows);
    let lit = &cache.get(&view.pr.head_sha, &path, &rows);
    let limit = ui.shown.max(SHOW_STEP);
    match ui.mode {
        DiffMode::Unified => {
            out.more = rows.len().saturating_sub(limit);
            let lines = rows
                .iter()
                .enumerate()
                .take(limit)
                .map(|(i, _)| {
                    let mut under = below.take(Side::Left, i);
                    under.extend(below.take(Side::Right, i));
                    (unified_line(&rows, i, lit, &can, sel), under)
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
        // Story and Calls draw in frames of their own; this body stays empty.
        DiffMode::Story | DiffMode::Calls => {}
    }
    out
}

/// What a Diff region is made of: the file list and one file's diff (Unified and Split), the
/// Story, or the Calls graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Files,
    Story,
    Calls,
}

impl From<DiffMode> for FrameKind {
    fn from(mode: DiffMode) -> Self {
        match mode {
            DiffMode::Unified | DiffMode::Split => FrameKind::Files,
            DiffMode::Story => FrameKind::Story,
            DiffMode::Calls => FrameKind::Calls,
        }
    }
}

/// The Diff section's root inside `SectionBody`; its absence, or another kind than the mode
/// wants, means "fill me".
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DiffRegion {
    pub pr: PrRef,
    pub kind: FrameKind,
}

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
pub struct ThreadToggle {
    pub pr: PrRef,
    pub id: String,
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

#[derive(Component)]
struct FilesPart {
    pr: PrRef,
    built: Option<Vec<FileEntry>>,
}

#[derive(Component)]
struct HeaderPart {
    pr: PrRef,
    built: Option<(Option<String>, u64, u64)>,
}

/// What a body is laid out as. Another layout, file, text size or render options draws the body
/// anew; anything else updates the rows that are there.
#[derive(Debug, Clone, PartialEq)]
enum Layout {
    Empty(&'static str),
    Unified,
    Split,
}

#[derive(Debug, Clone, PartialEq)]
struct BodyShape {
    path: Option<String>,
    layout: Layout,
    code_size: f32,
    opts: RenderOpts,
}

/// A line as its row draws it: with no selection (a selected fill is set on the row in place)
/// and nothing under it.
#[derive(Debug, Clone, PartialEq)]
enum RowKey {
    Unified(UnifiedLine),
    Split(SplitLine),
}

impl RowKey {
    /// The kind of the line on side `at` (0 left, 1 right; a Unified row is side 0).
    fn kind(&self, at: usize) -> Option<RowKind> {
        match self {
            RowKey::Unified(l) => (at == 0).then_some(l.kind),
            RowKey::Split(l) => [&l.left, &l.right][at].as_ref().map(|c| c.kind),
        }
    }

    /// The code runs of side `at`, as `kind` counts sides.
    fn spans(&self, at: usize) -> Option<&[CodeSpan]> {
        match self {
            RowKey::Unified(l) => (at == 0).then_some(&l.spans[..]),
            RowKey::Split(l) => [&l.left, &l.right][at].as_ref().map(|c| &c.spans[..]),
        }
    }

    /// The line with its runs cut down to its text: two lines that differ only in color (a
    /// highlight landing, or a new head shown plain) are equal so.
    fn uncolored(&self) -> RowKey {
        let plain = |spans: &[CodeSpan]| {
            vec![CodeSpan {
                text: spans.iter().map(|s| s.text.as_str()).collect(),
                ink: Swatch::Muted,
                mark: None,
            }]
        };
        match self {
            RowKey::Unified(l) => RowKey::Unified(UnifiedLine {
                spans: plain(&l.spans),
                ..l.clone()
            }),
            RowKey::Split(l) => {
                let cell = |c: &Option<Cell>| {
                    c.as_ref().map(|c| Cell {
                        spans: plain(&c.spans),
                        ..c.clone()
                    })
                };
                RowKey::Split(SplitLine {
                    left: cell(&l.left),
                    right: cell(&l.right),
                    ..l.clone()
                })
            }
        }
    }
}

/// What sits under a row: one list in Unified, one per side in Split.
#[derive(Debug, Clone, PartialEq, Default)]
struct UnderKey {
    items: [Vec<Below>; 2],
    /// The editor's target, refusal and **Saving…**, when one of the items is its box: the box
    /// draws them. Its typed text is not here, so typing never respawns it.
    editor: Option<(EditTarget, Option<String>, bool)>,
}

impl UnderKey {
    fn is_empty(&self) -> bool {
        self.items.iter().all(Vec::is_empty)
    }
}

/// A body line as the view wants it.
struct Want {
    row: RowKey,
    selected: [bool; 2],
    under: UnderKey,
}

/// A body line's entities and what they were built from.
struct Slot {
    row: RowKey,
    entity: Entity,
    /// The nodes whose fill shows the selection: the row in Unified, each side's cell in Split.
    cells: [Option<Entity>; 2],
    /// Each side's code text, recolored in place when only its colors change.
    texts: [Option<Entity>; 2],
    selected: [bool; 2],
    /// It carries `FocusRow`.
    focused: bool,
    under: UnderKey,
    /// What sits under the row, placed right after it.
    below: Option<Under>,
}

/// The node under a row, its columns (one in Unified, the node itself) and each item's node.
struct Under {
    node: Entity,
    columns: [Option<Entity>; 2],
    items: [Vec<Entity>; 2],
}

/// What a body's children are. They are, in order: the Split labels (in Split), each slot's row
/// and its below node, and the **Show more** footer.
struct Built {
    shape: BodyShape,
    slots: Vec<Slot>,
    more: usize,
    footer: Option<Entity>,
    /// The Split labels and their node, the first child.
    labels: Option<((String, String), Entity)>,
    /// The line to scroll to.
    focus: Option<(usize, u32)>,
}

#[derive(Component)]
struct BodyPart {
    pr: PrRef,
    built: Option<Built>,
}

/// The row that `Open in diff` lit, in the body of the `PrRef` it holds.
#[derive(Component)]
struct FocusRow(PrRef);

/// Frames a body waits for its focused row to be laid out before it stops trying.
const FOCUS_FRAMES: u8 = 4;

/// On a body whose scroll is an estimate until its rows are laid out: wrapped rows are taller
/// than the estimate counts, so the first laid-out frame moves the scroll to the real row. It
/// is one-shot: it goes when the row is reached, when the scroll is no longer where the estimate
/// put it (the user moved it), or after `FOCUS_FRAMES` frames without a laid-out row.
#[derive(Component)]
struct FocusPending {
    /// The scroll offset the estimate set.
    set: f32,
    left: u8,
}

impl FocusPending {
    fn at(set: f32) -> Self {
        Self {
            set,
            left: FOCUS_FRAMES,
        }
    }
}

pub struct DiffPlugin;

impl Plugin for DiffPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HighlightCache>().add_systems(
            Update,
            (fill_region, rebuild_diff, reach_focus)
                .chain()
                .after(ReviewSystems),
        );
    }
}

/// Fills the Diff section's body with the frame the mode wants, and again when the mode asks for
/// another one. Unified and Split share the Files frame.
pub(crate) fn fill_region(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    bodies: Query<(Entity, &SectionBody)>,
    regions: Query<(&ChildOf, &DiffRegion)>,
) {
    for (entity, body) in &bodies {
        let Some(tab) = tabs.0.get(&body.pr) else {
            continue;
        };
        if tab.ui.section != ReviewSection::Diff || !matches!(tab.phase, Phase::Ready(_)) {
            continue;
        }
        let kind = FrameKind::from(tab.ui.mode);
        if regions
            .iter()
            .any(|(child_of, region)| child_of.parent() == entity && region.kind == kind)
        {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        let pr = body.pr.clone();
        commands.entity(entity).with_children(|p| match kind {
            FrameKind::Files => frame(p, &pr),
            other => {
                p.spawn((
                    Node {
                        flex_grow: 1.0,
                        width: percent(100),
                        min_height: px(0),
                        ..default()
                    },
                    DiffRegion {
                        pr: pr.clone(),
                        kind: other,
                    },
                ))
                .with_children(|r| match other {
                    FrameKind::Story => super::story::frame(r, &pr),
                    _ => super::calls::frame(r, &pr),
                });
            }
        });
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
        DiffRegion {
            pr: pr.clone(),
            kind: FrameKind::Files,
        },
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
                        min_height: px(46),
                        flex_shrink: 0.0,
                        padding: UiRect::axes(px(16), px(6)),
                        column_gap: px(8),
                        row_gap: px(4),
                        flex_wrap: FlexWrap::Wrap,
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
    model: Res<Model>,
    fonts: Res<UiFonts>,
    mut cache: ResMut<HighlightCache>,
    landed: Res<HighlightsLanded>,
    mut last_opts: Local<Option<RenderOpts>>,
    mut files: Query<(Entity, &mut FilesPart)>,
    mut headers: Query<(Entity, &mut HeaderPart)>,
    mut bodies: Query<(Entity, &mut BodyPart, &mut ScrollPosition)>,
) {
    let opts = RenderOpts::from_config(theme.code_size, &model.snapshot.config);
    let fresh = files.iter().any(|(_, p)| p.built.is_none())
        || headers.iter().any(|(_, p)| p.built.is_none())
        || bodies.iter().any(|(_, p, _)| p.built.is_none());
    let opts_changed = last_opts.as_ref() != Some(&opts);
    if !(fresh || tabs.is_changed() || theme.is_changed() || landed.is_changed() || opts_changed) {
        return;
    }
    *last_opts = Some(opts.clone());
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
        let key = (v.path.clone(), v.additions, v.deletions);
        if part.built.as_ref() == Some(&key) {
            continue;
        }
        refill(&mut commands, entity, |p| header(p, fonts, v));
        part.built = Some(key);
    }
    for (entity, mut part, mut scroll) in &mut bodies {
        let Some((v, editor)) = views.get(&part.pr) else {
            continue;
        };
        let shape = BodyShape {
            path: v.path.clone(),
            layout: match &v.body {
                DiffBody::Empty(message) => Layout::Empty(message),
                DiffBody::Unified(_) => Layout::Unified,
                DiffBody::Split { .. } => Layout::Split,
            },
            code_size: v.code_size,
            opts: opts.clone(),
        };
        let focus = v.focus_row.map(|row| (row, v.focus_seq));
        if part.built.as_ref().map(|b| &b.shape.path) != Some(&shape.path) {
            scroll.0 = Vec2::ZERO; // another file starts at its top
        }
        if let Some((index, _)) = focus
            && part.built.as_ref().map(|b| b.focus) != Some(focus)
        {
            scroll.0.y = focus_scroll(index);
            commands.entity(entity).insert(FocusPending::at(scroll.0.y));
        }
        let pr = part.pr.clone();
        let path = v.path.clone().unwrap_or_default();
        let draw = Draw {
            fonts,
            pr: &pr,
            path: &path,
            group: entity,
            size: v.code_size,
            editor: editor.as_ref(),
            opts: &shape.opts,
            gutter: Gutter::Both,
        };
        let mut built = match part.built.take() {
            Some(built) if built.shape == shape => built,
            _ => {
                refill(&mut commands, entity, |p| {
                    if let Layout::Empty(message) = &shape.layout {
                        empty_body(p, fonts, message);
                    }
                });
                Built {
                    shape: shape.clone(),
                    slots: Vec::new(),
                    more: 0,
                    footer: None,
                    labels: None,
                    focus: None,
                }
            }
        };
        if let DiffBody::Split { before, after, .. } = &v.body {
            let labels = (before.clone(), after.clone());
            if built.labels.as_ref().map(|(l, _)| l) != Some(&labels) {
                if let Some((_, node)) = built.labels.take() {
                    commands.entity(node).despawn();
                }
                let mut node = Entity::PLACEHOLDER;
                commands.entity(entity).with_children(|p| {
                    node = split_labels(p, fonts, before, after);
                });
                commands.entity(entity).insert_child(0, node);
                built.labels = Some((labels, node));
            }
        }
        let wants = wants_of(&v.body, editor.as_ref());
        reconcile(
            &mut commands,
            entity,
            &mut built,
            &draw,
            wants,
            v.more,
            v.focus_row,
        );
        built.focus = focus;
        part.built = Some(built);
    }
}

/// The lines of `body` as rows want them, with what sits under each.
fn wants_of(body: &DiffBody, editor: Option<&Editor>) -> Vec<Want> {
    let under = |items: [Vec<Below>; 2]| {
        let editor = editor
            .filter(|_| items.iter().flatten().any(|b| *b == Below::Editor))
            .map(|e| (e.target.clone(), e.error.clone(), e.ticket.is_some()));
        UnderKey { items, editor }
    };
    match body {
        DiffBody::Empty(_) => Vec::new(),
        DiffBody::Unified(lines) => lines
            .iter()
            .map(|(line, items)| Want {
                row: RowKey::Unified(UnifiedLine {
                    selected: false,
                    ..line.clone()
                }),
                selected: [line.selected, false],
                under: under([items.clone(), Vec::new()]),
            })
            .collect(),
        DiffBody::Split { lines, .. } => lines
            .iter()
            .map(|line| {
                let plain = |cell: &Option<Cell>| {
                    cell.clone().map(|c| Cell {
                        selected: false,
                        ..c
                    })
                };
                let lit = |cell: &Option<Cell>| cell.as_ref().is_some_and(|c| c.selected);
                Want {
                    row: RowKey::Split(SplitLine {
                        hunk: line.hunk.clone(),
                        left: plain(&line.left),
                        right: plain(&line.right),
                        below_left: Vec::new(),
                        below_right: Vec::new(),
                    }),
                    selected: [lit(&line.left), lit(&line.right)],
                    under: under([line.below_left.clone(), line.below_right.clone()]),
                }
            })
            .collect(),
    }
}

/// Brings `built`'s children to `wants`: a line whose row changed is respawned in its place, a
/// new selection or focus is set on the row that is there, what sits under a row is respawned
/// alone, lines past the end go and new lines are appended. Lines are matched by position, so a
/// line added or removed above respawns every row below it.
fn reconcile(
    commands: &mut Commands,
    body: Entity,
    built: &mut Built,
    draw: &Draw,
    wants: Vec<Want>,
    more: usize,
    focus_row: Option<usize>,
) {
    // The footer is the last child: it goes before lines are appended and comes back after.
    if (built.more != more || built.slots.len() != wants.len())
        && let Some(footer) = built.footer.take()
    {
        commands.entity(footer).despawn();
    }
    let keep = built.slots.len().min(wants.len());
    for slot in built.slots.drain(keep..) {
        despawn_slot(commands, &slot);
    }
    // The index of the current slot's row among the body's children.
    let mut at = usize::from(built.labels.is_some());
    for (order, want) in wants.into_iter().enumerate() {
        let focused = focus_row == Some(order);
        let Some(slot) = built.slots.get_mut(order) else {
            built
                .slots
                .push(spawn_slot(commands, body, None, draw, order, want, focused));
            at += 1 + usize::from(built.slots[order].below.is_some());
            continue;
        };
        if slot.row != want.row && slot.row.uncolored() != want.row.uncolored() {
            despawn_slot(commands, slot);
            *slot = spawn_slot(commands, body, Some(at), draw, order, want, focused);
        } else {
            if slot.row != want.row {
                for side in 0..2 {
                    if let (Some(text), Some(spans)) = (slot.texts[side], want.row.spans(side))
                        && slot.row.spans(side) != Some(spans)
                    {
                        respan(commands, text, draw.fonts, draw.size, spans.to_vec());
                    }
                }
                slot.row = want.row;
            }
            for side in 0..2 {
                if slot.selected[side] != want.selected[side]
                    && let (Some(cell), Some(kind)) = (slot.cells[side], slot.row.kind(side))
                {
                    commands
                        .entity(cell)
                        .insert(Fill(fill_of(kind, want.selected[side])));
                }
            }
            slot.selected = want.selected;
            if slot.focused != focused {
                if focused {
                    commands
                        .entity(slot.entity)
                        .insert(FocusRow(draw.pr.clone()));
                } else {
                    commands.entity(slot.entity).remove::<FocusRow>();
                }
                slot.focused = focused;
            }
            if slot.under != want.under {
                match &mut slot.below {
                    Some(under) if want.under.is_empty() => {
                        commands.entity(under.node).despawn();
                        slot.below = None;
                    }
                    Some(under) => {
                        let editor = slot.under.editor != want.under.editor;
                        for side in 0..2 {
                            if let Some(column) = under.columns[side] {
                                update_items(
                                    commands,
                                    draw,
                                    column,
                                    &mut under.items[side],
                                    (&slot.under.items[side], &want.under.items[side]),
                                    editor,
                                );
                            }
                        }
                    }
                    None => {
                        let mut spawned = None;
                        commands.entity(body).with_children(|p| {
                            spawned = under_node(p, draw, &slot.row, &want.under);
                        });
                        if let Some(under) = &spawned {
                            commands.entity(body).insert_child(at + 1, under.node);
                        }
                        slot.below = spawned;
                    }
                }
                slot.under = want.under;
            }
        }
        at += 1 + usize::from(built.slots[order].below.is_some());
    }
    if built.footer.is_none() && more > 0 {
        commands.entity(body).with_children(|p| {
            built.footer = Some(show_more(p, draw.fonts, draw.pr, more));
        });
    }
    built.more = more;
}

fn despawn_slot(commands: &mut Commands, slot: &Slot) {
    commands.entity(slot.entity).despawn();
    if let Some(under) = &slot.below {
        commands.entity(under.node).despawn();
    }
}

/// Brings the items of one column from `was` to `want`: an item that changed is respawned in its
/// place, items past the end go and new ones are appended. With `editor`, the editor's box
/// counts as changed.
fn update_items(
    commands: &mut Commands,
    draw: &Draw,
    column: Entity,
    nodes: &mut Vec<Entity>,
    (was, want): (&[Below], &[Below]),
    editor: bool,
) {
    let keep = was.len().min(want.len());
    for node in nodes.drain(keep..) {
        commands.entity(node).despawn();
    }
    for (i, item) in want.iter().enumerate() {
        if i < keep {
            if was[i] == *item && !(editor && *item == Below::Editor) {
                continue;
            }
            commands.entity(nodes[i]).despawn();
        }
        let mut spawned = Entity::PLACEHOLDER;
        commands.entity(column).with_children(|p| {
            spawned = below(p, draw.fonts, draw.pr, item, draw.editor, draw.opts);
        });
        if i < keep {
            commands.entity(column).insert_child(i, spawned);
            nodes[i] = spawned;
        } else {
            nodes.push(spawned);
        }
    }
}

/// Spawns a line's row and what sits under it in `body`, at child index `place` (at the end
/// without one).
fn spawn_slot(
    commands: &mut Commands,
    body: Entity,
    place: Option<usize>,
    draw: &Draw,
    order: usize,
    want: Want,
    focused: bool,
) -> Slot {
    let mut spawned = (Entity::PLACEHOLDER, [None; 2], [None; 2], None);
    commands.entity(body).with_children(|p| {
        let (entity, cells, texts) = match &want.row {
            RowKey::Unified(line) => {
                let (row, text) = unified_row(p, draw, order, line, want.selected[0], focused);
                (row, [Some(row), None], [text, None])
            }
            RowKey::Split(line) => split_row(p, draw, order, line, want.selected, focused),
        };
        spawned = (
            entity,
            cells,
            texts,
            under_node(p, draw, &want.row, &want.under),
        );
    });
    let (entity, cells, texts, below) = spawned;
    if let Some(at) = place {
        let mut placed = vec![entity];
        placed.extend(below.as_ref().map(|u| u.node));
        commands.entity(body).insert_children(at, &placed);
    }
    Slot {
        row: want.row,
        entity,
        cells,
        texts,
        selected: want.selected,
        focused,
        under: want.under,
        below,
    }
}

/// Scrolls a body to its focused row, once the row has a laid-out box, with a few rows of
/// context above it.
fn reach_focus(
    mut commands: Commands,
    mut bodies: Query<(
        Entity,
        &BodyPart,
        &ComputedNode,
        &UiGlobalTransform,
        &mut ScrollPosition,
        &mut FocusPending,
    )>,
    rows: Query<(&FocusRow, &ComputedNode, &UiGlobalTransform)>,
) {
    for (entity, part, body, body_at, mut scroll, mut pending) in &mut bodies {
        if (scroll.0.y - pending.set).abs() > 0.5 {
            commands.entity(entity).remove::<FocusPending>();
            continue;
        }
        let Some((_, row, row_at)) = rows
            .iter()
            .find(|(f, row, _)| f.0 == part.pr && row.size.y > 0.0)
        else {
            pending.left -= 1;
            if pending.left == 0 {
                commands.entity(entity).remove::<FocusPending>();
            }
            continue;
        };
        let scale = body.inverse_scale_factor;
        let top =
            |n: &ComputedNode, at: &UiGlobalTransform| (at.translation.y - n.size.y / 2.0) * scale;
        let into = top(row, row_at) - top(body, body_at) + scroll.0.y;
        scroll.0.y = (into - FOCUS_CONTEXT as f32 * ROW_HEIGHT).max(0.0);
        commands.entity(entity).remove::<FocusPending>();
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
                    flex_basis: px(0),
                    min_width: px(0),
                    ..default()
                },
                children![wrapped_text(
                    fonts,
                    f.path.clone(),
                    Type::MONO.ink(Swatch::Fg)
                )],
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

fn header(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &DiffView) {
    if let Some(path) = &v.path {
        p.spawn((
            wrapped_text(fonts, path.clone(), Type::MONO.ink(Swatch::Fg).size(13.0)),
            Node {
                max_width: percent(100),
                ..default()
            },
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
}

/// The *Unified | Split | Story | Calls* switch, with `current` on. It sits at the right of the
/// section row while the Diff section shows.
pub(crate) fn mode_switch(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    current: DiffMode,
) {
    let mut track = p.spawn(segments());
    track.entry::<Node>().and_modify(|mut n| {
        n.align_self = AlignSelf::Center;
        // Narrower than its four segments, the track scrolls sideways.
        n.max_width = percent(100);
        n.overflow = Overflow::scroll_x();
    });
    track.insert(SideTrack);
    track.with_children(|s| {
        for (label, mode) in [
            ("Unified", DiffMode::Unified),
            ("Split", DiffMode::Split),
            ("Story", DiffMode::Story),
            ("Calls", DiffMode::Calls),
        ] {
            let mut e = s.spawn((
                segment(fonts, label, current == mode),
                ModeButton {
                    pr: pr.clone(),
                    mode,
                },
                observe(on_mode),
            ));
            if current == mode {
                e.insert(Reveal);
            }
        }
    });
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
    number_text(p, fonts, n.map(|n| n.to_string()).unwrap_or_default(), size);
}

fn number_text(p: &mut ChildSpawnerCommands, fonts: &UiFonts, text_: String, size: f32) {
    p.spawn(Node {
        width: px(NUMBER_WIDTH),
        min_height: px(ROW_HEIGHT),
        flex_shrink: 0.0,
        align_self: AlignSelf::Start,
        justify_content: JustifyContent::FlexEnd,
        padding: UiRect {
            right: px(10),
            top: px(TEXT_TOP),
            ..default()
        },
        ..default()
    })
    .with_children(|c| {
        c.spawn(text(fonts, text_, Type::MONO.size(size).ink(Swatch::Faint)));
    });
}

fn mark(p: &mut ChildSpawnerCommands, fonts: &UiFonts, kind: RowKind, size: f32) {
    let (sign, ink) = marker(kind);
    p.spawn(Node {
        width: px(18),
        min_height: px(ROW_HEIGHT),
        flex_shrink: 0.0,
        align_self: AlignSelf::Start,
        padding: UiRect::top(px(TEXT_TOP)),
        ..default()
    })
    .with_children(|c| {
        c.spawn(text(fonts, sign, Type::MONO.size(size).ink(ink)));
    });
}

/// The code of a row. It takes the room the number and the sign leave and breaks at its edge, so
/// a wrapped line continues under its own start, with no number of its own.
fn code(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    size: f32,
    spans: Vec<CodeSpan>,
    select: Option<LineKey>,
) -> Option<Entity> {
    let mut text = None;
    p.spawn(Node {
        flex_grow: 1.0,
        flex_basis: px(0),
        min_width: px(0),
        min_height: px(ROW_HEIGHT),
        padding: UiRect {
            right: px(8),
            top: px(TEXT_TOP),
            ..default()
        },
        ..default()
    })
    .with_children(|c| match select {
        Some(key) => {
            text = Some(selectable(
                c,
                key,
                plain_text(&spans),
                wrapped_code_line(fonts, size, spans),
            ));
        }
        None => {
            c.spawn(wrapped_code_line(fonts, size, spans));
        }
    });
    text
}

/// A line's container; commentable lines are buttons that open the editor.
fn line_node(
    p: &mut ChildSpawnerCommands,
    width: Val,
    kind: RowKind,
    selected: bool,
    button: Option<LineButton>,
    focus: Option<FocusRow>,
    build: impl FnOnce(&mut ChildSpawnerCommands),
) -> Entity {
    let fill = fill_of(kind, selected);
    let mut e = p.spawn((
        Node {
            width,
            min_height: px(ROW_HEIGHT),
            flex_shrink: 0.0,
            overflow: Overflow::clip_x(),
            ..default()
        },
        BackgroundColor::default(),
        Fill(fill),
    ));
    if let Some(focus) = focus {
        e.insert(focus);
    }
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
    e.id()
}

/// Which number columns a Unified row draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gutter {
    /// The old and the new number and a sign: the Diff.
    Both,
    /// One column with the head line (a `−` on a removed line) and no sign: a Story step.
    Head,
}

/// What every row of one body is drawn with.
struct Draw<'a> {
    fonts: &'a UiFonts,
    pr: &'a PrRef,
    path: &'a str,
    /// The selection group of the body's code lines.
    group: Entity,
    size: f32,
    editor: Option<&'a Editor>,
    opts: &'a RenderOpts,
    gutter: Gutter,
}

impl Draw<'_> {
    fn line_button(
        &self,
        row: usize,
        anchor: Option<(Side, u32)>,
        commentable: bool,
    ) -> Option<LineButton> {
        let (side, line) = anchor.filter(|_| commentable)?;
        Some(LineButton {
            pr: self.pr.clone(),
            path: self.path.to_string(),
            row,
            side,
            line,
        })
    }
}

/// The rows of a Unified body, each with what sits under it. `focus_row` is the index among
/// `lines` that `Open in diff` lit.
#[allow(clippy::too_many_arguments)]
pub(crate) fn unified_rows(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    path: &str,
    lines: &[(UnifiedLine, Vec<Below>)],
    size: f32,
    editor: Option<&Editor>,
    opts: &RenderOpts,
    gutter: Gutter,
    focus_row: Option<usize>,
) {
    let draw = Draw {
        fonts,
        pr,
        path,
        group: p.target_entity(),
        size,
        editor,
        opts,
        gutter,
    };
    for (at, (line, under)) in lines.iter().enumerate() {
        unified_row(p, &draw, at, line, line.selected, focus_row == Some(at));
        if !under.is_empty() {
            unified_below(p, &draw, under);
        }
    }
}

/// One Unified row, `order`-th of its body, and its code text.
fn unified_row(
    p: &mut ChildSpawnerCommands,
    draw: &Draw,
    order: usize,
    line: &UnifiedLine,
    selected: bool,
    focused: bool,
) -> (Entity, Option<Entity>) {
    let (fonts, size) = (draw.fonts, draw.size);
    let mut text = None;
    let row = line_node(
        p,
        percent(100),
        line.kind,
        selected,
        draw.line_button(line.row, line.anchor, line.commentable),
        focused.then(|| FocusRow(draw.pr.clone())),
        |r| {
            match draw.gutter {
                Gutter::Both => {
                    number(r, fonts, line.old, size);
                    number(r, fonts, line.new, size);
                    mark(r, fonts, line.kind, size);
                }
                Gutter::Head => {
                    let head = match (line.new, line.kind) {
                        (Some(n), _) => n.to_string(),
                        (None, RowKind::Removed) => "−".to_string(),
                        (None, _) => String::new(),
                    };
                    number_text(r, fonts, head, size);
                }
            }
            let key = LineKey {
                group: draw.group,
                order,
                column: 0,
            };
            text = code(r, fonts, size, line.spans.clone(), Some(key));
        },
    );
    (row, text)
}

/// What sits under a Unified row, indented to its code.
fn unified_below(p: &mut ChildSpawnerCommands, draw: &Draw, under: &[Below]) -> Under {
    let indent = match draw.gutter {
        Gutter::Both => 2.0 * NUMBER_WIDTH + 18.0,
        Gutter::Head => NUMBER_WIDTH,
    };
    let mut column = p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(8),
        padding: UiRect::new(px(indent), px(8), px(8), px(8)),
        ..default()
    });
    let node = column.id();
    let mut items = Vec::new();
    column.with_children(|b| {
        for item in under {
            items.push(below(b, draw.fonts, draw.pr, item, draw.editor, draw.opts));
        }
    });
    Under {
        node,
        columns: [Some(node), None],
        items: [items, Vec::new()],
    }
}

/// The node under a row that holds `under`; none when nothing sits there.
fn under_node(
    p: &mut ChildSpawnerCommands,
    draw: &Draw,
    row: &RowKey,
    under: &UnderKey,
) -> Option<Under> {
    if under.is_empty() {
        return None;
    }
    Some(match row {
        RowKey::Unified(_) => unified_below(p, draw, &under.items[0]),
        RowKey::Split(_) => split_below(p, draw, &under.items),
    })
}

/// One Split line, `order`-th of its body: the row, each side's cell and each side's code text.
fn split_row(
    p: &mut ChildSpawnerCommands,
    draw: &Draw,
    order: usize,
    line: &SplitLine,
    selected: [bool; 2],
    focused: bool,
) -> (Entity, [Option<Entity>; 2], [Option<Entity>; 2]) {
    let (fonts, size) = (draw.fonts, draw.size);
    let mut cells = [None; 2];
    let mut texts = [None; 2];
    let mut pair = p.spawn(Node {
        width: percent(100),
        flex_shrink: 0.0,
        ..default()
    });
    if focused {
        pair.insert(FocusRow(draw.pr.clone()));
    }
    pair.with_children(|r| {
        if let Some(hunk) = &line.hunk {
            for _ in 0..2 {
                line_node(r, percent(50), RowKind::Hunk, false, None, None, |c| {
                    c.spawn(Node {
                        width: px(NUMBER_WIDTH + 18.0),
                        flex_shrink: 0.0,
                        ..default()
                    });
                    code(
                        c,
                        fonts,
                        size,
                        vec![CodeSpan {
                            text: hunk.clone(),
                            ink: Swatch::Muted,
                            mark: None,
                        }],
                        None,
                    );
                });
            }
            return;
        }
        for (column, cell) in [&line.left, &line.right].into_iter().enumerate() {
            match cell {
                Some(c) => {
                    let b = draw.line_button(c.row, c.anchor, c.commentable);
                    let node = line_node(r, percent(50), c.kind, selected[column], b, None, |x| {
                        number(x, fonts, c.number, size);
                        mark(x, fonts, c.kind, size);
                        let key = LineKey {
                            group: draw.group,
                            order,
                            column: column as u8,
                        };
                        texts[column] = code(x, fonts, size, c.spans.clone(), Some(key));
                    });
                    cells[column] = Some(node);
                }
                None => {
                    r.spawn(panel(
                        Node {
                            width: percent(50),
                            min_height: px(ROW_HEIGHT),
                            ..default()
                        },
                        Swatch::Bg,
                    ));
                }
            }
        }
    });
    (pair.id(), cells, texts)
}

/// What sits under a Split line, each side under its own column.
fn split_below(p: &mut ChildSpawnerCommands, draw: &Draw, under: &[Vec<Below>; 2]) -> Under {
    let mut columns = [None; 2];
    let mut items = [Vec::new(), Vec::new()];
    let mut line = p.spawn(Node {
        width: percent(100),
        flex_shrink: 0.0,
        ..default()
    });
    let node = line.id();
    line.with_children(|r| {
        for (side, list) in under.iter().enumerate() {
            let mut column = r.spawn(Node {
                width: percent(50),
                flex_direction: FlexDirection::Column,
                row_gap: px(8),
                padding: UiRect::new(px(NUMBER_WIDTH + 10.0), px(8), px(8), px(8)),
                ..default()
            });
            columns[side] = Some(column.id());
            column.with_children(|b| {
                for item in list {
                    items[side].push(below(b, draw.fonts, draw.pr, item, draw.editor, draw.opts));
                }
            });
        }
    });
    Under {
        node,
        columns,
        items,
    }
}

fn empty_body(p: &mut ChildSpawnerCommands, fonts: &UiFonts, message: &'static str) {
    p.spawn(Node {
        padding: px(24).all(),
        ..default()
    })
    .with_children(|e| {
        e.spawn(text(fonts, message, Type::MUTED));
    });
}

/// The *Before* and *After* labels over the Split columns.
fn split_labels(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    before: &str,
    after: &str,
) -> Entity {
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
                c.spawn(wrapped_text(fonts, label.to_string(), Type::META));
            });
        }
    })
    .id()
}

/// **Show more** and how many lines it would add.
fn show_more(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, more: usize) -> Entity {
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
        m.spawn(text(fonts, format!("{more} more lines"), Type::META));
    })
    .id()
}

fn below(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    item: &Below,
    editor: Option<&Editor>,
    opts: &RenderOpts,
) -> Entity {
    match item {
        Below::Editor => match editor {
            Some(editor) => editor_box(p, fonts, editor, pr),
            // The view places the editor only while there is one; the empty node keeps one node
            // per item, which `update_items` indexes by.
            None => p.spawn(Node::default()).id(),
        },
        Below::Thread(t) => p
            .spawn(card(Node {
                flex_direction: FlexDirection::Column,
                row_gap: px(8),
                padding: UiRect::axes(px(10), px(10)),
                ..default()
            }))
            .with_children(|c| {
                // The buttons drop to a second row when the card is narrow (a Split column).
                c.spawn(Node {
                    column_gap: px(10),
                    row_gap: px(4),
                    flex_wrap: FlexWrap::Wrap,
                    align_items: AlignItems::Center,
                    ..default()
                })
                .with_children(|h| {
                    h.spawn(person_avatar(fonts, &t.author, 22.0));
                    h.spawn(person_name(fonts, &t.author));
                    // Collapsed, the first comment shows as one line: the box is one text
                    // line high and clips what wraps below it.
                    let mut line = h.spawn((
                        Node {
                            flex_grow: 1.0,
                            flex_basis: px(0),
                            min_width: px(60),
                            max_height: if t.expanded { Val::Auto } else { px(18) },
                            overflow: Overflow::clip(),
                            ..default()
                        },
                        Preview,
                    ));
                    if !t.expanded
                        && let Some(first) = t.posts.first()
                    {
                        // The words lay out on one line far wider than the card, so the clip
                        // cuts the preview where the card ends instead of wrapping its tail
                        // out of sight.
                        line.with_children(|l| {
                            l.spawn(Node {
                                width: px(1200),
                                flex_shrink: 0.0,
                                ..default()
                            })
                            .with_children(|w| {
                                markdown_line(
                                    w,
                                    fonts,
                                    &parse_in(&first.body, &opts.github_host, pr),
                                    Type::BODY,
                                    opts,
                                    120,
                                );
                            });
                        });
                    }
                    // The count rides on the toggle's label, which never shrinks: only the
                    // preview gives way when the card is narrow.
                    let toggle = if t.expanded { "Collapse" } else { "Expand" };
                    let toggle = match t.replies {
                        0 => toggle.to_string(),
                        1 => format!("{toggle} · 1 reply"),
                        n => format!("{toggle} · {n} replies"),
                    };
                    h.spawn((
                        wrapping_button(fonts, &toggle, Variant::Ghost),
                        ThreadToggle {
                            pr: pr.clone(),
                            id: t.thread.id.clone(),
                        },
                        observe(on_toggle),
                    ));
                    h.spawn((
                        wrapping_button(fonts, "Reply", Variant::Ghost),
                        ThreadReply {
                            pr: pr.clone(),
                            thread: t.thread.clone(),
                        },
                        observe(on_reply),
                    ));
                });
                if t.expanded {
                    for (i, post) in t.posts.iter().enumerate() {
                        c.spawn(Node {
                            flex_direction: FlexDirection::Column,
                            row_gap: px(4),
                            padding: UiRect::left(px(32)),
                            ..default()
                        })
                        .with_children(|b| {
                            b.spawn(Node {
                                column_gap: px(8),
                                align_items: AlignItems::Center,
                                ..default()
                            })
                            .with_children(|r| {
                                if i > 0 {
                                    r.spawn(person_name(fonts, &post.author));
                                }
                                r.spawn(Node {
                                    flex_grow: 1.0,
                                    ..default()
                                });
                                r.spawn(copy_button(fonts, &post.body));
                            });
                            markdown(b, fonts, &parse_in(&post.body, &opts.github_host, pr), opts);
                        });
                    }
                }
            })
            .id(),
        Below::Draft(d) => {
            let obsolete = matches!(&d.status, Some((_, Tone::Orange)));
            p.spawn((
                panel(
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: px(8),
                        padding: UiRect::axes(px(10), px(10)),
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
                    row_gap: px(4),
                    flex_wrap: FlexWrap::Wrap,
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
                    h.spawn(copy_button(fonts, &d.body));
                    h.spawn((
                        wrapping_button(fonts, "Edit", Variant::Ghost),
                        DraftEdit {
                            pr: pr.clone(),
                            id: d.id.clone(),
                        },
                        observe(on_edit),
                    ));
                    h.spawn((
                        wrapping_button(fonts, "Remove", Variant::Ghost),
                        DraftRemove {
                            pr: pr.clone(),
                            id: d.id.clone(),
                        },
                        observe(on_remove),
                    ));
                });
                markdown(c, fonts, &parse_in(&d.body, &opts.github_host, pr), opts);
            })
            .id()
        }
    }
}

fn ui_of<'a>(tabs: &'a mut ReviewTabs, pr: &PrRef) -> Option<&'a mut TabUi> {
    tabs.0.get_mut(pr).map(|t| &mut t.ui)
}

/// An editor with unsent text stays (as for **+ General note**): the user finishes or cancels
/// it before another one opens.
pub(crate) fn unsent(editor: Option<&Editor>) -> bool {
    editor.is_some_and(|e| e.ticket.is_none() && !e.text.trim().is_empty())
}

fn on_file(activate: On<Activate>, buttons: Query<&FileButton>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(ui) = ui_of(&mut tabs, &b.pr) {
        ui.file = Some(b.path.clone());
        ui.shown = SHOW_STEP;
        ui.focus_line = None;
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
    set_config(
        &mut asks,
        &mut model,
        "appearance.diff_view",
        diff_view_key(b.mode),
    );
}

/// The value `appearance.diff_view` takes for a mode.
pub(crate) fn diff_view_key(mode: DiffMode) -> &'static str {
    match mode {
        DiffMode::Unified => "unified",
        DiffMode::Split => "split",
        DiffMode::Story => "story",
        DiffMode::Calls => "calls",
    }
}

fn on_line(
    activate: On<Activate>,
    buttons: Query<&LineButton>,
    keys: Res<ButtonInput<KeyCode>>,
    mut tabs: ResMut<ReviewTabs>,
    model: Res<Model>,
    mut rounds: ResMut<Revalidations>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
    mut selection: ResMut<CodeSelection>,
) {
    let kind = selection.take_click();
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    match kind {
        // A drag across the line selected its code; it is not a click on it.
        ClickKind::Selected { multi: false } => {}
        // A second press on the line, soon after the click that opened an editor on it: the
        // click was the start of a double click, so the editor it opened goes.
        ClickKind::Selected { multi: true } => {
            if let Some(ui) = ui_of(&mut tabs, &b.pr) {
                close_untouched(ui, b);
            }
        }
        ClickKind::Plain => open_line(
            b,
            keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]),
            &mut tabs,
            &model,
            &mut rounds,
            &mut toasts,
            &time,
            &mut asks,
        ),
    }
}

/// Closes the editor on `b`'s line when nothing was typed in it and nothing came of it.
fn close_untouched(ui: &mut TabUi, b: &LineButton) {
    let untouched = ui.editor.as_ref().is_some_and(|e| {
        e.text.is_empty()
            && e.error.is_none()
            && e.ticket.is_none()
            && matches!(
                &e.target,
                EditTarget::Line { path, side, start: None, line }
                    if *path == b.path && *side == b.side && *line == b.line
            )
    });
    if untouched {
        ui.editor = None;
    }
}

fn open_line(
    b: &LineButton,
    shift: bool,
    tabs: &mut ReviewTabs,
    model: &Model,
    rounds: &mut Revalidations,
    toasts: &mut Toasts,
    time: &Time,
    asks: &mut Asks,
) {
    // While a comment waits for its new place, a click on a line is that place.
    let place = AnchorInput {
        path: b.path.clone(),
        line: b.line,
        start_line: None,
        side: b.side,
    };
    if reanchor_click(tabs, model, rounds, toasts, time, asks, &b.pr, place) {
        return;
    }
    let Some(ui) = ui_of(tabs, &b.pr) else {
        return;
    };
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
        mode: ComposerMode::Write,
    });
}

fn on_toggle(activate: On<Activate>, buttons: Query<&ThreadToggle>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(ui) = ui_of(&mut tabs, &b.pr)
        && !ui.expanded.remove(&b.id)
    {
        ui.expanded.insert(b.id.clone());
    }
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
            mode: ComposerMode::Write,
        });
    }
}

pub(crate) fn on_edit(
    activate: On<Activate>,
    buttons: Query<&DraftEdit>,
    mut tabs: ResMut<ReviewTabs>,
) {
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
        mode: ComposerMode::Write,
    });
}

pub(crate) fn on_remove(
    activate: On<Activate>,
    buttons: Query<&DraftRemove>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    if let Ok(b) = buttons.get(activate.entity) {
        if let Some(reason) = read_only_reason(&tabs, &model, &b.pr) {
            return not_sent(&mut toasts, &time, reason);
        }
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
    use crate::screens::review::calls::CallsRoot;
    use crate::screens::review::editor::{EditorArea, EditorCancel, EditorSubmit};
    use crate::screens::review::highlight::{HIGHLIGHT_CACHE_BYTES, finish_highlights};
    use crate::screens::review::shell::SectionTab;
    use crate::screens::review::story::StoryRoot;
    use crate::testing::{self, NOW};
    use crate::ui::markdown::{CopiedText, CopyMarkdown};
    use bevy::text::EditableText;
    use clusia_core::FileDiff;
    use clusia_protocol::AnchorInput;
    use clusia_view::diff::{commentable, line_on};
    use std::collections::HashSet;

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
        assert_eq!(
            thread.posts[0].body,
            "Why one minute? The CLI uses 30 seconds."
        );
        assert_eq!(thread.posts.len(), 3);
        assert!(!thread.expanded);
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
                mode: ComposerMode::Write,
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
                DiffMode::Story | DiffMode::Calls => unreachable!("only the file views list lines"),
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

    #[test]
    fn read_only_reviews_remove_nothing() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        let set_cached = |app: &mut App, at: Option<i64>| {
            if let Phase::Ready(r) = &mut ui_mut(app, &pr).0.get_mut(&pr).unwrap().phase {
                r.cached_at = at;
            }
            testing::settle(app);
        };
        let last_toast = |app: &App| {
            app.world()
                .resource::<crate::bridge::Toasts>()
                .0
                .last()
                .map(|t| (t.text.clone(), t.warning))
        };
        set_cached(&mut app, Some(NOW - 3600));
        let remove = testing::find::<DraftRemove>(&mut app, |_| true);
        testing::activate(&mut app, remove);
        assert!(testing::recorded(&mut app).is_empty(), "the cached copy");
        assert_eq!(
            last_toast(&app),
            Some((
                crate::screens::review::editor::CACHED_COPY.to_string(),
                true
            ))
        );
        set_cached(&mut app, None);
        app.world_mut()
            .resource_mut::<crate::bridge::Model>()
            .connection = crate::bridge::Connection::Lost("gone".into());
        testing::settle(&mut app);
        testing::recorded(&mut app); // MarkSeen for the fresh copy
        let remove = testing::find::<DraftRemove>(&mut app, |_| true);
        testing::activate(&mut app, remove);
        let sent = testing::recorded(&mut app);
        assert!(sent.is_empty(), "not connected: {sent:?}");
        assert_eq!(
            last_toast(&app),
            Some(("Not connected to clusiad — not sent".to_string(), true))
        );
    }

    #[test]
    fn a_waiting_comment_shows_saving() {
        let mut app = review_app();
        comment_on_44(&mut app, "Drop the guard before the exchange.");
        testing::settle(&mut app);
        assert!(has_text(&mut app, "Saving…"));
        assert_eq!(testing::count::<EditorSubmit>(&mut app), 0);
    }

    #[test]
    fn a_lost_daemon_frees_the_waiting_comment() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        let body = "Drop the guard before the exchange.";
        comment_on_44(&mut app, body);
        testing::tell(&mut app, Tell::Lost("clusiad closed the connection".into()));
        testing::settle(&mut app);
        let editor = testing::tab(&app, &pr).ui.editor.expect("the editor stays");
        assert_eq!(
            (editor.text.as_str(), editor.ticket, editor.error.as_deref()),
            (body, None, Some(crate::review_state::LOST_WHILE_SAVING))
        );
        assert!(
            unsent(Some(&editor)),
            "closing the tab or the window protects it again"
        );
        assert!(has_text(&mut app, crate::review_state::LOST_WHILE_SAVING));
        let snapshot = app
            .world()
            .resource::<crate::bridge::Model>()
            .snapshot
            .clone();
        testing::tell(&mut app, Tell::Snapshot(Box::new(snapshot)));
        testing::settle(&mut app);
        testing::recorded(&mut app);
        let submit = testing::find::<EditorSubmit>(&mut app, |_| true);
        testing::activate(&mut app, submit);
        let asks = testing::recorded(&mut app);
        assert!(
            matches!(
                asks.as_slice(),
                [Ask::AddItem { body: sent, ticket, .. }] if sent == body && *ticket > 0
            ),
            "sent again after reconnecting: {asks:?}"
        );
    }

    #[test]
    fn thread_authors_in_the_diff_open_their_profiles() {
        use crate::platform_open::OpenUrls;
        use crate::ui::people::{PersonAvatar, PersonTarget};
        let mut app = review_app();
        testing::settle(&mut app);
        let name = testing::entities::<PersonTarget>(&mut app)
            .into_iter()
            .find(|e| {
                app.world()
                    .get::<PersonTarget>(*e)
                    .is_some_and(|t| t.0 == "mona")
                    && app.world().get::<PersonAvatar>(*e).is_none()
            })
            .expect("mona's name on her thread");
        assert!(
            testing::entities::<PersonAvatar>(&mut app)
                .iter()
                .any(|e| app
                    .world()
                    .get::<PersonAvatar>(*e)
                    .is_some_and(|a| a.0 == "mona")),
            "and her avatar"
        );
        testing::click_button(&mut app, name);
        assert_eq!(
            app.world().resource::<OpenUrls>().0,
            ["https://github.com/mona"]
        );
        assert!(
            testing::tab(&app, &fixture::demo_pr())
                .ui
                .expanded
                .is_empty(),
            "the click did not also toggle the thread"
        );
    }

    #[test]
    fn thread_cards_collapse_to_a_line_and_expand_to_the_full_thread() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        let first = "Why one minute? The CLI uses 30 seconds.";
        let last = "A minute is safer while the runners drift.";
        assert!(testing::shows(&mut app, first));
        assert!(
            !testing::shows(&mut app, last),
            "collapsed: the first line only"
        );
        let toggle = testing::find::<ThreadToggle>(&mut app, |_| true);
        testing::activate(&mut app, toggle);
        testing::settle(&mut app);
        assert_eq!(testing::tab(&app, &pr).ui.expanded.len(), 1);
        assert!(testing::shows(&mut app, last), "expanded: every comment");
        // Each comment of the thread can be copied as written.
        let copy = testing::find::<CopyMarkdown>(&mut app, |c| c.0 == last);
        testing::activate(&mut app, copy);
        assert_eq!(app.world().resource::<CopiedText>().0, [last]);
        let toggle = testing::find::<ThreadToggle>(&mut app, |_| true);
        testing::activate(&mut app, toggle);
        testing::settle(&mut app);
        assert!(testing::tab(&app, &pr).ui.expanded.is_empty());
        assert!(!testing::shows(&mut app, last), "collapsed again");
    }

    #[test]
    fn the_reply_count_of_a_collapsed_thread_only_wraps_when_the_card_is_narrower_than_it() {
        let mut app = review_app();
        let mut q = app.world_mut().query::<(&Text, &TextLayout, &ChildOf)>();
        let (_, layout, parent) = q
            .iter(app.world())
            .find(|(t, _, _)| t.0 == "Expand · 2 replies")
            .expect("the count is part of the toggle's label");
        assert_eq!(layout.linebreak, LineBreak::WordOrCharacter);
        let button = app.world().get::<Node>(parent.parent()).unwrap();
        assert_eq!(button.flex_shrink, 0.0, "only the preview gives way");
        assert_eq!(
            button.max_width,
            percent(100),
            "wider than the card, it wraps"
        );
    }

    #[test]
    fn draft_comments_render_markdown_with_a_copy_button() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        let body = "Use a **guard** around `exchange`";
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let Phase::Ready(ready) = &mut tabs.0.get_mut(&pr).unwrap().phase else {
                panic!("ready")
            };
            let id = ready
                .view
                .review
                .draft
                .items
                .iter()
                .find(|i| {
                    i.kind == DraftKind::LineComment
                        && i.anchor.as_ref().is_some_and(|a| a.line == 44)
                })
                .map(|i| i.id.clone())
                .expect("the draft on line 44");
            ready.view.review.draft.update_body(&id, body).unwrap();
        }
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Use a guard around exchange"));
        assert!(!testing::shows(&mut app, "**"));
        let copy = testing::find::<CopyMarkdown>(&mut app, |c| c.0 == body);
        testing::activate(&mut app, copy);
        assert_eq!(app.world().resource::<CopiedText>().0, [body]);
    }

    fn tab_of(ready: Ready) -> Tab {
        Tab {
            phase: Phase::Ready(Box::new(ready)),
            ui: TabUi::default(),
        }
    }

    fn big_file(ready: &mut Ready) {
        let body: String = (1..=1200).map(|n| format!("+line {n}\n")).collect();
        ready.view.diff.push(FileDiff {
            path: "big.txt".into(),
            previous_path: None,
            status: "added".into(),
            additions: 1200,
            deletions: 0,
            patch: Some(format!("@@ -0,0 +1,1200 @@\n{body}")),
        });
    }

    #[test]
    fn the_focus_line_is_lit_and_its_row_is_named() {
        let r = ready();
        let ui = TabUi {
            focus_line: Some((Side::Right, 44)),
            ..TabUi::default()
        };
        let v = view_of(&r, &ui);
        let lit: Vec<u32> = unified(&v)
            .iter()
            .filter(|(l, _)| l.selected)
            .filter_map(|(l, _)| l.new)
            .collect();
        assert_eq!(lit, [44]);
        let at = unified(&v).iter().position(|(l, _)| l.new == Some(44));
        assert!(at.is_some());
        assert_eq!(v.focus_row, at);
        let none = view_of(&r, &TabUi::default());
        assert_eq!(none.focus_row, None);
        assert!(unified(&none).iter().all(|(l, _)| !l.selected));
    }

    #[test]
    fn the_focus_row_counts_split_lines_in_split() {
        let r = ready();
        let ui = TabUi {
            mode: DiffMode::Split,
            focus_line: Some((Side::Right, 44)),
            ..TabUi::default()
        };
        let v = view_of(&r, &ui);
        let DiffBody::Split { lines, .. } = &v.body else {
            panic!("expected Split")
        };
        let at = lines
            .iter()
            .position(|l| l.right.as_ref().is_some_and(|c| c.number == Some(44)));
        assert!(at.is_some());
        assert_eq!(v.focus_row, at);
    }

    #[test]
    fn an_open_editor_wins_over_the_focus_line() {
        let r = ready();
        let ui = TabUi {
            focus_line: Some((Side::Right, 44)),
            editor: Some(Editor {
                target: EditTarget::Line {
                    path: REFRESH.into(),
                    side: Side::Right,
                    start: None,
                    line: 39,
                },
                text: String::new(),
                error: None,
                ticket: None,
                mode: ComposerMode::Write,
            }),
            ..TabUi::default()
        };
        let lit: Vec<u32> = unified(&view_of(&r, &ui))
            .iter()
            .filter(|(l, _)| l.selected)
            .filter_map(|(l, _)| l.new)
            .collect();
        assert_eq!(lit, [39]);
    }

    #[test]
    fn show_line_lights_the_first_line_of_the_range_that_is_in_the_diff() {
        let mut tab = tab_of(ready());
        assert!(show_line(&mut tab, REFRESH, Some((30, 44))));
        assert_eq!(tab.ui.section, ReviewSection::Diff);
        assert_eq!(tab.ui.file.as_deref(), Some(REFRESH));
        assert_eq!(tab.ui.focus_line, Some((Side::Right, 38)));
    }

    #[test]
    fn show_line_on_a_huge_range_looks_at_the_rows_not_at_every_number() {
        let mut tab = tab_of(ready());
        tab.ui.mode = DiffMode::Split;
        let started = std::time::Instant::now();
        assert!(show_line(&mut tab, REFRESH, Some((1, 4_000_000_000))));
        assert_eq!(tab.ui.focus_line, Some((Side::Right, 38)));
        assert!(show_line(&mut tab, REFRESH, Some((5_000, u32::MAX))));
        assert_eq!(tab.ui.focus_line, None);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn show_line_without_lines_or_with_lines_outside_the_diff_shows_the_file_only() {
        let mut tab = tab_of(ready());
        tab.ui.focus_line = Some((Side::Right, 44));
        assert!(show_line(&mut tab, REFRESH, None));
        assert_eq!(tab.ui.file.as_deref(), Some(REFRESH));
        assert_eq!(tab.ui.focus_line, None);
        assert!(show_line(&mut tab, REFRESH, Some((1, 5))));
        assert_eq!(tab.ui.focus_line, None);
    }

    #[test]
    fn show_line_leaves_the_tab_alone_when_the_file_is_not_in_the_diff() {
        let mut tab = tab_of(ready());
        let before = tab.ui.clone();
        assert!(!show_line(&mut tab, "src/missing.rs", Some((1, 2))));
        assert_eq!(tab.ui, before);
        assert!(in_diff(tab.ready().unwrap(), REFRESH));
        assert!(!in_diff(tab.ready().unwrap(), "src/missing.rs"));
    }

    #[test]
    fn a_far_line_widens_the_window_of_shown_lines() {
        let mut r = ready();
        big_file(&mut r);
        let mut tab = tab_of(r);
        assert!(show_line(&mut tab, "big.txt", Some((1100, 1100))));
        assert_eq!(tab.ui.focus_line, Some((Side::Right, 1100)));
        assert_eq!(tab.ui.shown, 3 * SHOW_STEP);
        let v = view_of(tab.ready().unwrap(), &tab.ui);
        assert_eq!(v.focus_row, Some(1100));
        assert!(v.more < 1201);
    }

    #[test]
    fn the_scroll_keeps_a_few_lines_above_the_line() {
        assert_eq!(focus_scroll(0), 0.0);
        assert_eq!(focus_scroll(3), 0.0);
        assert_eq!(focus_scroll(10), 7.0 * ROW_HEIGHT);
    }

    #[test]
    fn choosing_another_file_clears_the_focus_and_the_same_file_keeps_it() {
        let mut ui = TabUi {
            file: Some(REFRESH.into()),
            focus_line: Some((Side::Right, 44)),
            ..TabUi::default()
        };
        crate::screens::review::agent::suggestion::show_file(&mut ui, REFRESH);
        assert_eq!(ui.focus_line, Some((Side::Right, 44)));
        crate::screens::review::agent::suggestion::show_file(&mut ui, "CHANGELOG.md");
        assert_eq!(ui.focus_line, None);
    }

    fn body_scroll(app: &mut App) -> f32 {
        let mut q = app
            .world_mut()
            .query_filtered::<&ScrollPosition, With<BodyPart>>();
        q.iter(app.world()).map(|p| p.y).fold(0.0, f32::max)
    }

    fn reset_body_scroll(app: &mut App) {
        let mut q = app
            .world_mut()
            .query_filtered::<&mut ScrollPosition, With<BodyPart>>();
        for mut p in q.iter_mut(app.world_mut()) {
            p.y = 0.0;
        }
    }

    fn show_in(app: &mut App, pr: &PrRef, path: &str, lines: Option<(u32, u32)>) {
        let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
        let tab = tabs.0.get_mut(pr).expect("a tab");
        assert!(show_line(tab, path, lines));
        testing::settle(app);
    }

    #[test]
    fn asking_for_the_same_line_again_scrolls_to_it_again() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        show_in(&mut app, &pr, REFRESH, Some((44, 44)));
        assert!(body_scroll(&mut app) > 0.0, "the first request scrolls");
        reset_body_scroll(&mut app);
        show_in(&mut app, &pr, REFRESH, Some((44, 44)));
        assert!(body_scroll(&mut app) > 0.0, "a repeat scrolls again");
    }

    #[test]
    fn the_same_row_of_another_file_scrolls_too() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let Phase::Ready(ready) = &mut tabs.0.get_mut(&pr).expect("a tab").phase else {
                panic!("ready")
            };
            let mut twin = ready
                .view
                .diff
                .iter()
                .find(|f| f.path == REFRESH)
                .expect("the demo file")
                .clone();
            twin.path = "src/auth/refresh_copy.rs".into();
            ready.view.diff.push(twin);
        }
        show_in(&mut app, &pr, REFRESH, Some((44, 44)));
        assert!(body_scroll(&mut app) > 0.0);
        show_in(&mut app, &pr, "src/auth/refresh_copy.rs", Some((44, 44)));
        assert!(
            body_scroll(&mut app) > 0.0,
            "another file starts at its top, then scrolls to the line"
        );
    }

    const LONG: &str = "Refresh one minute early";

    fn wrap_app() -> App {
        let mut app = testing::layout_app(fixture::demo(NOW), UVec2::new(1280, 800));
        testing::open_ready(&mut app, false);
        testing::settle(&mut app);
        app
    }

    /// The `Text` of the code line that holds `needle`, and the line's row.
    fn code_of(app: &mut App, needle: &str) -> (Entity, Entity) {
        let mut q = app.world_mut().query::<(&TextSpan, &ChildOf)>();
        let text = q
            .iter(app.world())
            .find(|(s, _)| s.0.contains(needle))
            .map(|(_, p)| p.parent())
            .expect("the code line");
        let cell = app.world().get::<ChildOf>(text).unwrap().parent();
        let row = app.world().get::<ChildOf>(cell).unwrap().parent();
        (text, row)
    }

    fn text_of_line(app: &App, text: Entity) -> String {
        app.world()
            .get::<Children>(text)
            .unwrap()
            .iter()
            .filter_map(|c| app.world().get::<TextSpan>(c))
            .map(|s| s.0.as_str())
            .collect()
    }

    fn set_mode(app: &mut App, mode: DiffMode) {
        let pr = fixture::demo_pr();
        ui_mut(app, &pr).0.get_mut(&pr).unwrap().ui.mode = mode;
        testing::settle(app);
    }

    fn parent_of(app: &App, entity: Entity) -> Entity {
        app.world().get::<ChildOf>(entity).unwrap().parent()
    }

    #[test]
    fn comment_headers_wrap_their_buttons_in_the_narrow_columns_of_split() {
        for large in [false, true] {
            let mut app = wrap_app();
            if large {
                testing::set_config_locally(&mut app, "appearance.text_size", "large");
            }
            set_mode(&mut app, DiffMode::Split);
            testing::settle(&mut app);
            let flagged: Vec<String> = testing::overflowing_text(&mut app)
                .into_iter()
                .filter(|t| {
                    ["Expand", "Reply", "Copy markdown", "Edit", "Remove"]
                        .iter()
                        .any(|b| t.starts_with(&format!("\"{b}")))
                })
                .collect();
            assert_eq!(flagged, Vec::<String>::new(), "large={large}");
        }
    }

    #[test]
    fn long_paths_wrap_in_the_header_the_file_list_and_the_split_labels() {
        let token = "refresh_with_retry_and_a_very_long_identifier_that_has_no_space_in_it";
        for (mode, large) in [
            (DiffMode::Unified, false),
            (DiffMode::Unified, true),
            (DiffMode::Split, false),
            (DiffMode::Split, true),
        ] {
            let mut app = wrap_app();
            let pr = fixture::demo_pr();
            {
                let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
                let tab = tabs.0.get_mut(&pr).unwrap();
                let Phase::Ready(ready) = &mut tab.phase else {
                    panic!("ready")
                };
                for (i, f) in ready.view.files.iter_mut().enumerate() {
                    f.path = format!("src/{token}/{i}_{token}.rs");
                }
                for (i, f) in ready.view.diff.iter_mut().enumerate() {
                    f.path = format!("src/{token}/{i}_{token}.rs");
                }
                ready.view.pr.head_ref = format!("{token}-{token}");
                tab.ui.mode = mode;
            }
            if large {
                testing::set_config_locally(&mut app, "appearance.text_size", "large");
            }
            testing::settle(&mut app);
            let header = testing::find::<HeaderPart>(&mut app, |_| true);
            let buttons = testing::entities::<FileButton>(&mut app);
            assert!(!buttons.is_empty());
            for root in std::iter::once(header).chain(buttons) {
                assert_eq!(
                    testing::texts_past_right(&mut app, root),
                    Vec::<String>::new(),
                    "{mode:?} large={large}"
                );
            }
            let switch = testing::find::<ModeButton>(&mut app, |b| b.mode == DiffMode::Split);
            let row = parent_of(&app, parent_of(&app, switch));
            let (rx, _, rw, _) = testing::frame(&app, row);
            let (sx, _, sw, _) = testing::frame(&app, switch);
            assert!(
                sx + sw <= rx + rw,
                "{mode:?} large={large}: the mode switch stays on the section row"
            );
        }
    }

    #[test]
    fn the_mode_track_scrolls_sideways_when_narrow_and_shows_the_selected_mode() {
        // Narrower than the smallest window, so the section tabs leave the track less room than
        // its four segments take, though more than one.
        let mut app = testing::layout_app(fixture::demo(NOW), UVec2::new(760, 640));
        testing::set_config_locally(&mut app, "appearance.text_size", "large");
        testing::open_ready(&mut app, false);
        testing::settle(&mut app);
        set_mode(&mut app, DiffMode::Calls);
        testing::settle(&mut app);
        let selected = testing::find::<ModeButton>(&mut app, |b| b.mode == DiffMode::Calls);
        let track = app.world().get::<ChildOf>(selected).unwrap().parent();
        assert!(
            app.world()
                .get::<crate::ui::kit::SideTrack>(track)
                .is_some()
        );
        let laid = app.world().get::<bevy::ui::ComputedNode>(track).unwrap();
        assert!(
            laid.content_size.x > laid.size.x,
            "the four segments do not fit"
        );
        let (sx, _, sw, _) = testing::frame(&app, selected);
        let (tx, _, tw, _) = testing::frame(&app, track);
        assert!(
            sx >= tx - 1.0 && sx + sw <= tx + tw + 1.0,
            "Calls is in view"
        );
        let scrolled = app.world().get::<ScrollPosition>(track).unwrap().x;
        assert!(scrolled > 0.0, "the track scrolled to the last mode");
        testing::wheel(&mut app, track, 3.0);
        assert!(app.world().get::<ScrollPosition>(track).unwrap().x < scrolled);
    }

    #[test]
    fn the_switch_sits_on_the_section_row_at_the_right() {
        let mut app = wrap_app();
        let story = testing::find::<ModeButton>(&mut app, |b| b.mode == DiffMode::Story);
        let track = parent_of(&app, story);
        let row = parent_of(&app, track);
        let diff = testing::find::<SectionTab>(&mut app, |t| t.section == ReviewSection::Diff);
        let tests = testing::find::<SectionTab>(&mut app, |t| t.section == ReviewSection::Tests);
        assert_eq!(
            parent_of(&app, diff),
            row,
            "the tabs and the switch share a row"
        );
        assert!(testing::center(&app, track).x > testing::center(&app, tests).x);
        let (diff_y, track_y) = (
            testing::center(&app, diff).y,
            testing::center(&app, track).y,
        );
        assert!(
            (diff_y - track_y).abs() <= 1.0,
            "tab axis {diff_y}, switch axis {track_y}"
        );
        let (tx, _, tw, _) = testing::frame(&app, track);
        let (rx, _, rw, _) = testing::frame(&app, row);
        assert!(
            ((tx + tw) - (rx + rw - 20.0)).abs() <= 1.0,
            "20 px from the right edge"
        );
        let modes: Vec<DiffMode> = {
            let mut q = app.world_mut().query::<&ModeButton>();
            let mut found: Vec<DiffMode> = q.iter(app.world()).map(|b| b.mode).collect();
            found.sort_by_key(|m| *m as u8);
            found
        };
        assert_eq!(
            modes,
            [
                DiffMode::Unified,
                DiffMode::Split,
                DiffMode::Story,
                DiffMode::Calls
            ]
        );
        assert!(!testing::shows(&mut app, "Arrives with SP3"));
    }

    #[test]
    fn each_mode_writes_its_own_config_value() {
        assert_eq!(diff_view_key(DiffMode::Unified), "unified");
        assert_eq!(diff_view_key(DiffMode::Split), "split");
        assert_eq!(diff_view_key(DiffMode::Story), "story");
        assert_eq!(diff_view_key(DiffMode::Calls), "calls");
    }

    #[test]
    fn the_switch_is_only_on_the_diff_section() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        testing::set_section(&mut app, &pr, ReviewSection::Comments);
        assert_eq!(testing::count::<ModeButton>(&mut app), 0);
        testing::set_section(&mut app, &pr, ReviewSection::Diff);
        assert_eq!(testing::count::<ModeButton>(&mut app), 4);
    }

    #[test]
    fn story_and_calls_get_their_own_frame_and_unified_and_split_share_one() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        let files = testing::find::<DiffRegion>(&mut app, |r| r.kind == FrameKind::Files);
        let split = testing::find::<ModeButton>(&mut app, |m| m.mode == DiffMode::Split);
        testing::activate(&mut app, split);
        testing::settle(&mut app);
        assert_eq!(
            testing::find::<DiffRegion>(&mut app, |r| r.kind == FrameKind::Files),
            files,
            "Unified and Split keep one frame"
        );

        let story = testing::find::<ModeButton>(&mut app, |m| m.mode == DiffMode::Story);
        testing::activate(&mut app, story);
        testing::settle(&mut app);
        assert_eq!(testing::tab(&app, &pr).ui.mode, DiffMode::Story);
        assert_eq!(testing::count::<DiffRegion>(&mut app), 1);
        assert_eq!(testing::count::<StoryRoot>(&mut app), 1);
        assert_eq!(
            testing::count::<FileButton>(&mut app),
            0,
            "no file list on Story"
        );

        let calls = testing::find::<ModeButton>(&mut app, |m| m.mode == DiffMode::Calls);
        testing::activate(&mut app, calls);
        testing::settle(&mut app);
        assert_eq!(testing::count::<StoryRoot>(&mut app), 0);
        assert_eq!(testing::count::<CallsRoot>(&mut app), 1);
        assert_eq!(testing::count::<DiffRegion>(&mut app), 1);

        let unified = testing::find::<ModeButton>(&mut app, |m| m.mode == DiffMode::Unified);
        testing::activate(&mut app, unified);
        testing::settle(&mut app);
        assert_eq!(testing::count::<CallsRoot>(&mut app), 0);
        assert_eq!(testing::count::<FileButton>(&mut app), 7);

        let stored: Vec<Ask> = testing::recorded(&mut app)
            .into_iter()
            .filter(|a| matches!(a, Ask::SetConfig { .. }))
            .collect();
        let value = |v: &str| Ask::SetConfig {
            key: "appearance.diff_view".into(),
            value: v.into(),
        };
        assert_eq!(
            stored,
            [
                value("split"),
                value("story"),
                value("calls"),
                value("unified")
            ]
        );
    }

    #[test]
    fn a_long_line_wraps_at_the_column_and_loses_nothing() {
        let mut app = wrap_app();
        let (text, row) = code_of(&mut app, LONG);
        let want = {
            let r = ready();
            let v = view_of(&r, &TabUi::default());
            let line = unified(&v)
                .iter()
                .find(|(l, _)| text_of(&l.spans).contains(LONG))
                .expect("the demo has the long line");
            text_of(&line.0.spans)
        };
        assert!(want.len() > 150, "the demo line is long");
        assert_eq!(text_of_line(&app, text), want, "no character is lost");
        let layout = app.world().get::<TextLayout>(text).unwrap();
        assert_ne!(layout.linebreak, LineBreak::NoWrap);
        let (_, _, rw, rh) = testing::frame(&app, row);
        let (_, _, tw, th) = testing::frame(&app, text);
        assert!(tw < rw, "the code stays inside its row: {tw} in {rw}");
        assert!(rh >= 2.0 * 22.0 - 1.0, "two lines make a taller row: {rh}");
        assert!(th >= 2.0 * 13.0, "the text itself has two lines: {th}");
        let kids = app.world().get::<Children>(row).unwrap();
        assert_eq!(
            kids.len(),
            4,
            "one number column per side, the sign, the code"
        );
    }

    /// The text of the number cell and the sign cell that sit before the code `text` in a unified
    /// row, and the text of the code itself.
    fn tops(app: &App, row: Entity, text: Entity) -> Vec<f32> {
        let kids = app.world().get::<Children>(row).unwrap();
        let mut tops: Vec<f32> = kids[..kids.len() - 1]
            .iter()
            .filter_map(|cell| app.world().get::<Children>(*cell))
            .filter_map(|c| c.first().copied())
            .map(|t| testing::frame(app, t).1)
            .collect();
        tops.push(testing::frame(app, text).1);
        tops
    }

    #[test]
    fn the_numbers_and_the_sign_sit_on_the_first_line_of_a_wrapped_row() {
        let mut app = wrap_app();
        let (text, row) = code_of(&mut app, LONG);
        let (_, _, _, rh) = testing::frame(&app, row);
        assert!(rh >= 2.0 * 22.0 - 1.0, "the row wrapped: {rh}");
        let tops = tops(&app, row, text);
        assert_eq!(tops.len(), 4, "old number, new number, sign, code");
        let code = *tops.last().unwrap();
        for (i, t) in tops.iter().enumerate() {
            assert!((t - code).abs() <= 0.5, "cell {i} at {t}, code at {code}");
        }
    }

    #[test]
    fn split_numbers_and_sign_sit_on_the_first_line_of_a_wrapped_side() {
        let mut app = wrap_app();
        set_mode(&mut app, DiffMode::Split);
        let (text, cell) = code_of(&mut app, LONG);
        let (_, _, _, ch) = testing::frame(&app, cell);
        assert!(ch >= 2.0 * 22.0 - 1.0, "the side wrapped: {ch}");
        let tops = tops(&app, cell, text);
        assert_eq!(tops.len(), 3, "number, sign, code");
        let code = *tops.last().unwrap();
        for (i, t) in tops.iter().enumerate() {
            assert!((t - code).abs() <= 0.5, "cell {i} at {t}, code at {code}");
        }
    }

    /// A press and release on `target`, as the picking backend sends them; they bubble up to the
    /// row's button the way a click on a continuation line does.
    fn click_on(app: &mut App, target: Entity) {
        press_on(app, target, Vec2::ZERO);
        release_on(app, target);
    }

    /// A press on `target` at `at`, with the button widgets listening.
    fn press_on(app: &mut App, target: Entity, at: Vec2) {
        use bevy::ui_widgets::ButtonPlugin;
        if !app.is_plugin_added::<ButtonPlugin>() {
            app.add_plugins(ButtonPlugin);
        }
        testing::press_code(app, target, at);
    }

    /// The release that follows a press on `target`: the click the backend sends.
    fn release_on(app: &mut App, target: Entity) {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::picking::backend::HitData;
        use bevy::picking::events::{Click, Pointer};
        use bevy::picking::pointer::{Location, PointerButton, PointerId};
        let location = Location {
            target: NormalizedRenderTarget::None {
                width: 1280,
                height: 800,
            },
            position: Vec2::ZERO,
        };
        let hit = HitData::new(Entity::PLACEHOLDER, 0.0, None, None);
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location,
            Click {
                button: PointerButton::Primary,
                hit,
                duration: std::time::Duration::ZERO,
                count: 1,
            },
            target,
        ));
        app.update();
    }

    #[test]
    fn a_click_on_a_wrapped_line_comments_on_its_line() {
        let mut app = wrap_app();
        let pr = fixture::demo_pr();
        let (text, row) = code_of(&mut app, LONG);
        let button = app
            .world()
            .get::<LineButton>(row)
            .expect("a commentable row");
        assert_eq!((button.side, button.line), (Side::Right, 40));
        click_on(&mut app, text);
        assert_eq!(
            testing::tab(&app, &pr).ui.editor.map(|e| e.target),
            Some(EditTarget::Line {
                path: REFRESH.into(),
                side: Side::Right,
                start: None,
                line: 40
            })
        );
    }

    #[test]
    fn a_shift_click_on_a_wrapped_line_extends_the_range() {
        let mut app = wrap_app();
        let pr = fixture::demo_pr();
        let first =
            testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == 39);
        testing::activate(&mut app, first);
        testing::settle(&mut app);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::ShiftLeft);
        let (text, _) = code_of(&mut app, LONG);
        click_on(&mut app, text);
        assert_eq!(
            testing::tab(&app, &pr).ui.editor.map(|e| e.target),
            Some(EditTarget::Line {
                path: REFRESH.into(),
                side: Side::Right,
                start: Some(39),
                line: 40
            })
        );
    }

    /// The code lines of the screen as (entity, line), top to bottom and left side first.
    fn code_lines(app: &mut App) -> Vec<(Entity, crate::ui::code_select::CodeLine)> {
        let mut q = app
            .world_mut()
            .query::<(Entity, &crate::ui::code_select::CodeLine)>();
        let mut lines: Vec<_> = q.iter(app.world()).map(|(e, l)| (e, l.clone())).collect();
        lines.sort_by_key(|(_, l)| (l.key.order, l.key.column));
        lines
    }

    fn selected(app: &App) -> String {
        let mut q = app
            .world()
            .try_query::<&crate::ui::code_select::CodeLine>()
            .unwrap();
        let lines: Vec<_> = q.iter(app.world()).collect();
        app.world().resource::<CodeSelection>().text(lines)
    }

    fn drag_between(app: &mut App, from: (Entity, usize), to: (Entity, usize)) {
        let a = testing::code_point(app, from.0, from.1);
        let b = testing::code_point(app, to.0, to.1);
        press_on(app, from.0, a);
        testing::drag_code(app, from.0, a, b);
    }

    fn line_with<'a>(
        lines: &'a [(Entity, crate::ui::code_select::CodeLine)],
        needle: &str,
        column: u8,
    ) -> &'a (Entity, crate::ui::code_select::CodeLine) {
        lines
            .iter()
            .find(|(_, l)| l.key.column == column && l.text.contains(needle))
            .expect("the code line")
    }

    #[test]
    fn dragging_across_lines_selects_their_code_and_nothing_else() {
        let mut app = wrap_app();
        let lines = code_lines(&mut app);
        let first = line_with(&lines, "Refresh one minute early", 0).clone();
        let next = lines
            .iter()
            .find(|(_, l)| l.key.order == first.1.key.order + 1)
            .expect("a line below")
            .clone();
        drag_between(&mut app, (first.0, 8), (next.0, 5));
        let expected = format!(
            "{}\n{}",
            &first.1.text[8..],
            &next.1.text[..5.min(next.1.text.len())]
        );
        assert_eq!(selected(&app), expected);
        assert!(!selected(&app).contains("+"), "no sign: {}", selected(&app));
    }

    #[test]
    fn a_selection_made_upwards_reads_top_down() {
        let mut app = wrap_app();
        let lines = code_lines(&mut app);
        let first = line_with(&lines, "Refresh one minute early", 0).clone();
        let next = lines
            .iter()
            .find(|(_, l)| l.key.order == first.1.key.order + 1)
            .unwrap()
            .clone();
        drag_between(&mut app, (next.0, 3.min(next.1.text.len())), (first.0, 4));
        let expected = format!(
            "{}\n{}",
            &first.1.text[4..],
            &next.1.text[..3.min(next.1.text.len())]
        );
        assert_eq!(selected(&app), expected);
    }

    #[test]
    fn a_split_selection_stays_on_the_side_it_began_on() {
        let mut app = wrap_app();
        set_mode(&mut app, DiffMode::Split);
        let lines = code_lines(&mut app);
        let right: Vec<_> = lines.iter().filter(|(_, l)| l.key.column == 1).collect();
        assert!(right.len() >= 3, "the demo has three right-hand rows");
        let (from, to) = (right[0].clone(), right[2].clone());
        // The pointer ends over the left side of the last row.
        let (left_of_to, _) = lines
            .iter()
            .find(|(_, l)| l.key.column == 0 && l.key.order == to.1.key.order)
            .expect("a left-hand line on the last row");
        let end = testing::code_point(&app, *left_of_to, 2);
        let start = testing::code_point(&app, from.0, 0);
        testing::press_code(&mut app, from.0, start);
        testing::drag_code(&mut app, from.0, start, end);
        let copied = selected(&app);
        let own: Vec<&str> = right
            .iter()
            .filter(|(_, l)| l.key.order <= to.1.key.order)
            .map(|(_, l)| l.text.as_str())
            .collect();
        let pieces: Vec<&str> = copied.split('\n').collect();
        assert_eq!(
            pieces.len(),
            own.len(),
            "one piece per right-hand row: {copied:?}"
        );
        let last = own.len() - 1;
        assert_eq!(pieces[..last], own[..last]);
        assert!(own[last].starts_with(pieces[last]));
    }

    #[test]
    fn a_wrapped_line_copies_as_its_one_original_line() {
        let mut app = wrap_app();
        let lines = code_lines(&mut app);
        let (text, long) = line_with(&lines, "Refresh one minute early", 0).clone();
        let (_, _, _, h) = testing::frame(&app, text);
        assert!(h >= 2.0 * 22.0 - 1.0 - 8.0, "the line wrapped: {h}");
        drag_between(&mut app, (text, 0), (text, long.text.len()));
        assert_eq!(selected(&app), long.text);
        assert!(!selected(&app).contains('\n'));
    }

    #[test]
    fn a_double_click_selects_a_word_and_a_triple_click_the_line() {
        let mut app = wrap_app();
        let lines = code_lines(&mut app);
        let (text, line) = line_with(&lines, "Refresh one minute early", 0).clone();
        let at = line.text.find("minute").unwrap() + 2;
        let p = testing::code_point(&app, text, at);
        testing::press_code(&mut app, text, p);
        assert_eq!(selected(&app), "");
        testing::press_code(&mut app, text, p);
        assert_eq!(selected(&app), "minute");
        testing::press_code(&mut app, text, p);
        assert_eq!(selected(&app), line.text);
    }

    #[test]
    fn a_click_still_comments_but_a_drag_or_a_multi_click_does_not() {
        let mut app = wrap_app();
        let pr = fixture::demo_pr();
        let lines = code_lines(&mut app);
        let (text, long) = line_with(&lines, "Refresh one minute early", 0).clone();
        click_on(&mut app, text);
        assert!(
            testing::tab(&app, &pr).ui.editor.is_some(),
            "a click comments at once"
        );
        ui_mut(&mut app, &pr).0.get_mut(&pr).unwrap().ui.editor = None;
        testing::settle(&mut app);

        let lines = code_lines(&mut app);
        let (text, _) = line_with(&lines, "Refresh one minute early", 0).clone();
        drag_between(&mut app, (text, 0), (text, long.text.len()));
        release_on(&mut app, text);
        assert!(
            testing::tab(&app, &pr).ui.editor.is_none(),
            "a drag selects and does not comment"
        );

        let lines = code_lines(&mut app);
        let (text, _) = line_with(&lines, "Refresh one minute early", 0).clone();
        let p = testing::code_point(&app, text, 3);
        press_on(&mut app, text, p);
        press_on(&mut app, text, p);
        release_on(&mut app, text);
        assert!(
            testing::tab(&app, &pr).ui.editor.is_none(),
            "a double click selects and does not comment"
        );
    }

    #[test]
    fn cmd_c_copies_the_selection_through_the_clipboard_path() {
        let mut app = wrap_app();
        let lines = code_lines(&mut app);
        let (text, long) = line_with(&lines, "Refresh one minute early", 0).clone();
        drag_between(&mut app, (text, 0), (text, long.text.len()));
        testing::chord(&mut app, KeyCode::KeyC);
        assert_eq!(app.world().resource::<CopiedText>().0, [long.text]);
    }

    #[test]
    fn cmd_c_with_nothing_selected_copies_nothing() {
        let mut app = wrap_app();
        testing::chord(&mut app, KeyCode::KeyC);
        assert!(app.world().resource::<CopiedText>().0.is_empty());
    }

    #[test]
    fn the_selected_text_is_highlighted_in_the_theme_color() {
        let mut app = wrap_app();
        let lines = code_lines(&mut app);
        let (text, long) = line_with(&lines, "Refresh one minute early", 0).clone();
        let rects = |app: &mut App| {
            let mut q = app.world_mut().query::<(&Fill, &Node)>();
            q.iter(app.world())
                .filter(|(f, n)| {
                    f.0 == Swatch::TextSelect && n.position_type == PositionType::Absolute
                })
                .count()
        };
        assert_eq!(rects(&mut app), 0);
        drag_between(&mut app, (text, 0), (text, long.text.len()));
        testing::settle(&mut app);
        assert!(rects(&mut app) >= 1, "the selection is drawn");
    }

    #[test]
    fn the_highlight_covers_exactly_the_selected_characters() {
        let mut app = wrap_app();
        let lines = code_lines(&mut app);
        let (text, _) = line_with(&lines, "Refresh one minute early", 0).clone();
        drag_between(&mut app, (text, 7), (text, 15));
        testing::settle(&mut app);
        let rect = testing::find::<Fill>(&mut app, |f| f.0 == Swatch::TextSelect);
        let (left, top, width, height) = testing::frame(&app, rect);
        let (from, to) = (
            testing::code_point(&app, text, 7),
            testing::code_point(&app, text, 15),
        );
        let (_, text_top, _, _) = testing::frame(&app, text);
        assert!((left - from.x).abs() <= 1.0, "left {left} vs {}", from.x);
        assert!(
            (left + width - to.x).abs() <= 1.0,
            "right {} vs {}",
            left + width,
            to.x
        );
        assert!((top - text_top).abs() <= 1.0, "top {top} vs {text_top}");
        assert!(height > 8.0 && height < 30.0, "one visual line: {height}");
    }

    fn highlights(app: &mut App) -> usize {
        let mut q = app.world_mut().query::<&Fill>();
        q.iter(app.world())
            .filter(|f| f.0 == Swatch::TextSelect)
            .count()
    }

    #[test]
    fn a_click_elsewhere_ends_the_selection_and_its_highlight() {
        let mut app = wrap_app();
        let lines = code_lines(&mut app);
        let (text, _) = line_with(&lines, "Refresh one minute early", 0).clone();
        drag_between(&mut app, (text, 7), (text, 15));
        testing::settle(&mut app);
        assert_eq!(selected(&app).len(), 8);
        assert!(highlights(&mut app) >= 1);
        testing::left_press(&mut app);
        testing::settle(&mut app);
        assert_eq!(selected(&app), "");
        assert_eq!(highlights(&mut app), 0);
    }

    #[test]
    fn a_press_on_code_replaces_the_selection() {
        let mut app = wrap_app();
        let lines = code_lines(&mut app);
        let (text, _) = line_with(&lines, "Refresh one minute early", 0).clone();
        drag_between(&mut app, (text, 7), (text, 15));
        assert_eq!(selected(&app).len(), 8);
        let p = testing::code_point(&app, text, 3);
        press_on(&mut app, text, p);
        assert_eq!(selected(&app), "", "the new press collapsed it");
    }

    /// The code `Text` that holds `needle` now: a click that opens an editor rebuilds the rows.
    fn fresh_line(app: &mut App, needle: &str) -> Entity {
        let lines = code_lines(app);
        line_with(&lines, needle, 0).0
    }

    #[test]
    fn a_single_click_opens_the_editor_with_no_wait() {
        let mut app = wrap_app();
        let pr = fixture::demo_pr();
        let text = fresh_line(&mut app, "Refresh one minute early");
        click_on(&mut app, text);
        assert!(testing::tab(&app, &pr).ui.editor.is_some());
    }

    #[test]
    fn a_double_click_ends_with_the_word_selected_copied_and_no_editor() {
        let mut app = wrap_app();
        let pr = fixture::demo_pr();
        let needle = "Refresh one minute early";
        let text = fresh_line(&mut app, needle);
        let line = code_lines(&mut app)
            .iter()
            .find(|(e, _)| *e == text)
            .unwrap()
            .1
            .clone();
        let at = line.text.find("minute").unwrap() + 2;
        let p = testing::code_point(&app, text, at);
        press_on(&mut app, text, p);
        release_on(&mut app, text);
        assert!(
            testing::tab(&app, &pr).ui.editor.is_some(),
            "the first click"
        );
        testing::settle(&mut app);
        // The pointer has not moved.
        let text = fresh_line(&mut app, needle);
        press_on(&mut app, text, p);
        release_on(&mut app, text);
        testing::settle(&mut app);
        assert!(testing::tab(&app, &pr).ui.editor.is_none(), "no editor");
        assert_eq!(selected(&app), "minute");
        testing::chord(&mut app, KeyCode::KeyC);
        assert_eq!(app.world().resource::<CopiedText>().0, ["minute"]);
    }

    #[test]
    fn a_quick_click_on_another_line_opens_that_line() {
        let mut app = wrap_app();
        let pr = fixture::demo_pr();
        let first =
            testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == 39);
        testing::activate(&mut app, first);
        testing::settle(&mut app);
        let text = fresh_line(&mut app, "Refresh one minute early");
        click_on(&mut app, text);
        assert_eq!(
            testing::tab(&app, &pr).ui.editor.map(|e| e.target),
            Some(EditTarget::Line {
                path: REFRESH.into(),
                side: Side::Right,
                start: None,
                line: 40
            })
        );
    }

    #[test]
    fn a_quick_shift_click_extends_the_range_of_the_line_just_clicked() {
        let mut app = wrap_app();
        let pr = fixture::demo_pr();
        let lines = code_lines(&mut app);
        let a = lines
            .iter()
            .find(|(_, l)| l.key.column == 0 && l.text.contains("Refresh one minute early"))
            .unwrap()
            .0;
        // A click on the line before it, then a shift-click on this one, right after.
        let before = lines
            .iter()
            .find(|(e, l)| *e != a && l.key.order + 1 == code_order(&lines, a))
            .unwrap()
            .0;
        click_on(&mut app, before);
        testing::settle(&mut app);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::ShiftLeft);
        let a = fresh_line(&mut app, "Refresh one minute early");
        click_on(&mut app, a);
        let target = testing::tab(&app, &pr).ui.editor.map(|e| e.target);
        let Some(EditTarget::Line { start, line, .. }) = target else {
            panic!("a line editor");
        };
        assert_eq!((start.is_some(), line), (true, 40), "{target:?}");
    }

    fn code_order(lines: &[(Entity, crate::ui::code_select::CodeLine)], of: Entity) -> usize {
        lines.iter().find(|(e, _)| *e == of).unwrap().1.key.order
    }

    #[test]
    fn a_double_click_on_a_line_whose_editor_has_text_keeps_the_editor() {
        let mut app = wrap_app();
        let pr = fixture::demo_pr();
        let needle = "Refresh one minute early";
        let text = fresh_line(&mut app, needle);
        let p = testing::code_point(&app, text, 4);
        press_on(&mut app, text, p);
        release_on(&mut app, text);
        testing::settle(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        testing::type_into(&mut app, area, "keep this");
        testing::settle(&mut app);
        let text = fresh_line(&mut app, needle);
        press_on(&mut app, text, p);
        release_on(&mut app, text);
        testing::settle(&mut app);
        let editor = testing::tab(&app, &pr).ui.editor;
        assert!(editor.is_some(), "the editor stays");
    }

    #[test]
    fn the_flags_of_a_drag_do_not_swallow_a_later_gutter_click() {
        let mut app = wrap_app();
        let pr = fixture::demo_pr();
        let lines = code_lines(&mut app);
        let (text, long) = line_with(&lines, "Refresh one minute early", 0).clone();
        drag_between(&mut app, (text, 0), (text, long.text.len()));
        // The release is never seen by the line (the pointer left it); a press on a gutter
        // follows, then the click on the row's button.
        testing::left_press(&mut app);
        let row = testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == 44);
        testing::activate(&mut app, row);
        assert!(
            testing::tab(&app, &pr).ui.editor.is_some(),
            "the click comments"
        );
    }

    #[test]
    fn a_selection_whose_lines_changed_or_are_gone_ends() {
        let mut app = wrap_app();
        let lines = code_lines(&mut app);
        let (text, _) = line_with(&lines, "Refresh one minute early", 0).clone();
        drag_between(&mut app, (text, 7), (text, 15));
        testing::settle(&mut app);
        assert!(!selected(&app).is_empty());
        // One line's text changes under the selection.
        app.world_mut()
            .get_mut::<crate::ui::code_select::CodeLine>(text)
            .unwrap()
            .text
            .push('!');
        testing::settle(&mut app);
        assert_eq!(selected(&app), "", "changed content");
        assert_eq!(highlights(&mut app), 0);

        let lines = code_lines(&mut app);
        let (text, _) = line_with(&lines, "Refresh one minute early", 0).clone();
        drag_between(&mut app, (text, 7), (text, 15));
        testing::settle(&mut app);
        assert!(!selected(&app).is_empty());
        let all: Vec<Entity> = code_lines(&mut app).iter().map(|(e, _)| *e).collect();
        for e in all {
            app.world_mut().entity_mut(e).despawn();
        }
        testing::settle(&mut app);
        assert!(
            app.world().resource::<CodeSelection>().is_empty(),
            "gone lines"
        );
    }

    #[test]
    fn a_selection_survives_a_rebuild_that_keeps_the_lines() {
        let mut app = wrap_app();
        let pr = fixture::demo_pr();
        let lines = code_lines(&mut app);
        let (text, long) = line_with(&lines, "Refresh one minute early", 0).clone();
        drag_between(&mut app, (text, 7), (text, 15));
        testing::settle(&mut app);
        // Opening a comment elsewhere rebuilds the body.
        ui_mut(&mut app, &pr).0.get_mut(&pr).unwrap().ui.editor = Some(Editor {
            target: EditTarget::Line {
                path: REFRESH.into(),
                side: Side::Right,
                start: None,
                line: 44,
            },
            text: String::new(),
            error: None,
            ticket: None,
            mode: ComposerMode::Write,
        });
        testing::settle(&mut app);
        assert!(testing::tab(&app, &pr).ui.editor.is_some());
        assert_eq!(selected(&app), long.text[7..15]);
    }

    #[test]
    fn copy_needs_the_command_key_alone() {
        use bevy::input::ButtonState;
        use bevy::input::keyboard::{Key, KeyboardInput, NativeKey};
        let mut app = wrap_app();
        let lines = code_lines(&mut app);
        let (text, long) = line_with(&lines, "Refresh one minute early", 0).clone();
        drag_between(&mut app, (text, 0), (text, long.text.len()));
        // Cmd-Shift-C is another command.
        for key in [KeyCode::ShiftLeft, KeyCode::SuperLeft, KeyCode::KeyC] {
            app.world_mut().write_message(KeyboardInput {
                key_code: key,
                logical_key: Key::Unidentified(NativeKey::Unidentified),
                state: ButtonState::Pressed,
                text: None,
                repeat: false,
                window: Entity::PLACEHOLDER,
            });
        }
        app.update();
        assert!(app.world().resource::<CopiedText>().0.is_empty());
    }

    #[test]
    fn copy_is_left_to_a_focused_text_field() {
        use bevy::input_focus::InputFocus;
        let mut app = wrap_app();
        let lines = code_lines(&mut app);
        let (text, long) = line_with(&lines, "Refresh one minute early", 0).clone();
        drag_between(&mut app, (text, 0), (text, long.text.len()));
        let field = app.world_mut().spawn(EditableText::default()).id();
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(field, bevy::input_focus::FocusCause::Navigated);
        testing::chord(&mut app, KeyCode::KeyC);
        assert!(app.world().resource::<CopiedText>().0.is_empty());
    }

    #[test]
    fn a_scroll_by_the_user_before_the_row_is_laid_out_is_left_alone() {
        let mut app = wrap_app();
        let pr = fixture::demo_pr();
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            assert!(show_line(
                tabs.0.get_mut(&pr).unwrap(),
                REFRESH,
                Some((44, 44))
            ));
        }
        app.update();
        let mut q = app
            .world_mut()
            .query_filtered::<&mut ScrollPosition, With<BodyPart>>();
        for mut p in q.iter_mut(app.world_mut()) {
            p.y = 1.0;
        }
        testing::settle(&mut app);
        assert_eq!(
            body_scroll(&mut app),
            1.0,
            "the focus does not pull it back"
        );
    }

    #[test]
    fn a_focus_that_never_finds_its_row_gives_up_and_a_later_rebuild_does_not_scroll() {
        let mut app = wrap_app();
        let body = testing::find::<BodyPart>(&mut app, |_| true);
        app.world_mut()
            .entity_mut(body)
            .insert(FocusPending::at(0.0));
        for _ in 0..=FOCUS_FRAMES {
            app.update();
        }
        assert!(app.world().get::<FocusPending>(body).is_none());
        set_mode(&mut app, DiffMode::Split);
        assert_eq!(body_scroll(&mut app), 0.0);
    }

    #[test]
    fn split_sides_wrap_on_their_own_and_stay_the_same_height() {
        let mut app = wrap_app();
        set_mode(&mut app, DiffMode::Split);
        let (text, cell) = code_of(&mut app, LONG);
        let pair = app.world().get::<ChildOf>(cell).unwrap().parent();
        let cells = app.world().get::<Children>(pair).unwrap().to_vec();
        assert_eq!(cells.len(), 2);
        let heights: Vec<f32> = cells.iter().map(|c| testing::frame(&app, *c).3).collect();
        assert_eq!(
            heights[0], heights[1],
            "the two sides are as tall as the taller"
        );
        assert!(
            heights[0] >= 2.0 * 22.0 - 1.0,
            "and it wrapped: {heights:?}"
        );
        let (_, _, cw, _) = testing::frame(&app, cell);
        let (_, _, tw, _) = testing::frame(&app, text);
        assert!(tw < cw, "inside its half: {tw} in {cw}");
        let (_, _, pw, _) = testing::frame(&app, pair);
        assert!((cw - pw / 2.0).abs() <= 1.0, "a half each");
    }

    #[test]
    fn a_hunk_header_wraps_too() {
        let mut app = wrap_app();
        {
            let pr = fixture::demo_pr();
            let mut tabs = ui_mut(&mut app, &pr);
            let Phase::Ready(ready) = &mut tabs.0.get_mut(&pr).unwrap().phase else {
                panic!("ready")
            };
            let f = ready
                .view
                .diff
                .iter_mut()
                .find(|f| f.path == REFRESH)
                .unwrap();
            let patch = f.patch.take().unwrap();
            let long = " impl TokenStore where Self: Send + Sync + 'static, Token: Clone + Send + Sync + 'static, AuthError: std::error::Error + Send + Sync + 'static, Duration: Copy {";
            f.patch = Some(patch.replacen(
                "@@ -38,9 +38,11 @@ impl TokenStore {",
                &format!("@@ -38,9 +38,11 @@{long}"),
                1,
            ));
        }
        testing::settle(&mut app);
        let (text, row) = code_of(&mut app, "Token: Clone");
        let (_, _, _, rh) = testing::frame(&app, row);
        assert!(rh >= 2.0 * 22.0 - 1.0, "the hunk header wrapped: {rh}");
        assert!(
            text_of_line(&app, text).contains("Duration: Copy {"),
            "nothing lost"
        );
    }

    #[test]
    fn wrapping_follows_the_text_size() {
        let mut app = wrap_app();
        let lines = |app: &mut App| {
            let (text, _) = code_of(app, LONG);
            let (_, _, tw, th) = testing::frame(app, text);
            (th, tw)
        };
        let (medium, _) = lines(&mut app);
        testing::set_config_locally(&mut app, "appearance.text_size", "large");
        testing::settle(&mut app);
        let (large, _) = lines(&mut app);
        assert!(
            large > medium,
            "a bigger size wraps into more or taller lines"
        );
        testing::set_config_locally(&mut app, "appearance.text_size", "small");
        testing::settle(&mut app);
        let (small, tw) = lines(&mut app);
        assert!(small < large);
        let (_, row) = code_of(&mut app, LONG);
        let (_, _, rw, _) = testing::frame(&app, row);
        assert!(tw < rw);
    }

    #[test]
    fn a_focused_line_below_wrapped_rows_still_scrolls_into_view() {
        let mut app = wrap_app();
        let pr = fixture::demo_pr();
        {
            let mut tabs = ui_mut(&mut app, &pr);
            let Phase::Ready(ready) = &mut tabs.0.get_mut(&pr).unwrap().phase else {
                panic!("ready")
            };
            let long = "x".repeat(400);
            let body: String = (1..=400).map(|n| format!("+{n} {long}\n")).collect();
            ready.view.diff.push(FileDiff {
                path: "wide.txt".into(),
                previous_path: None,
                status: "added".into(),
                additions: 400,
                deletions: 0,
                patch: Some(format!("@@ -0,0 +1,400 @@\n{body}")),
            });
        }
        show_in(&mut app, &pr, "wide.txt", Some((300, 300)));
        testing::settle(&mut app);
        let line = testing::find::<LineButton>(&mut app, |b| b.line == 300);
        let body = testing::find::<BodyPart>(&mut app, |_| true);
        let (_, by, _, bh) = testing::frame(&app, body);
        let (_, ly, _, lh) = testing::frame(&app, line);
        assert!(
            ly >= by && ly + lh <= by + bh,
            "the line at {ly}..{} is inside the view {by}..{}",
            ly + lh,
            by + bh
        );
    }

    fn rows_of(path: &str) -> Vec<Row> {
        parse_patch(&patch(&ready(), path))
    }

    #[test]
    fn a_step_takes_the_head_lines_in_its_range_and_the_removed_run_before_them() {
        let rows = rows_of(REFRESH);
        let picked = rows_in_ranges(&rows, &[(39, 41)]);
        let kinds: Vec<RowKind> = picked.iter().map(|&i| rows[i].kind).collect();
        assert_eq!(
            kinds,
            [
                RowKind::Removed,
                RowKind::Removed,
                RowKind::Added,
                RowKind::Added,
                RowKind::Added
            ]
        );
        let news: Vec<Option<u32>> = picked.iter().map(|&i| rows[i].new).collect();
        assert_eq!(news, [None, None, Some(39), Some(40), Some(41)]);
        assert!(picked.windows(2).all(|w| w[0] < w[1]), "in file order");
    }

    #[test]
    fn a_range_keeps_context_lines_and_skips_the_hunk_header() {
        let rows = rows_of(REFRESH);
        let picked = rows_in_ranges(&rows, &[(42, 44)]);
        let news: Vec<Option<u32>> = picked.iter().map(|&i| rows[i].new).collect();
        assert_eq!(news, [Some(42), Some(43), Some(44)]);
        assert!(picked.iter().all(|&i| rows[i].kind != RowKind::Hunk));
    }

    #[test]
    fn several_ranges_join_and_a_range_outside_the_patch_picks_nothing() {
        let rows = rows_of(REFRESH);
        let both = rows_in_ranges(&rows, &[(44, 44), (67, 67)]);
        let news: Vec<Option<u32>> = both.iter().map(|&i| rows[i].new).collect();
        assert_eq!(news, [Some(44), Some(67)]);
        assert!(rows_in_ranges(&rows, &[(500, 510)]).is_empty());
        assert!(rows_in_ranges(&[], &[(1, 5)]).is_empty());
    }

    #[test]
    fn an_excerpt_is_built_like_the_unified_rows_and_leaves_drafts_to_the_notes() {
        let r = ready();
        let mut ui = TabUi::default();
        let mut cache = HighlightCache::default();
        let ex = excerpt(&r, &ui, &mut cache, REFRESH, &[(43, 45)]).expect("an excerpt");
        assert_eq!(ex.path, REFRESH);
        assert_eq!(ex.hidden, 0);
        let news: Vec<Option<u32>> = ex.lines.iter().map(|(l, _)| l.new).collect();
        assert_eq!(news, [Some(43), Some(44), Some(45)]);
        let (line44, under) = &ex.lines[1];
        assert_eq!(line44.kind, RowKind::Added);
        assert!(line44.commentable && line44.anchor == Some((Side::Right, 44)));
        assert!(
            under.iter().all(|b| !matches!(b, Below::Draft(_))),
            "the draft comment on line 44 is a note, not a card under the row"
        );
        // An open line editor still shows under its row.
        ui.editor = Some(Editor {
            target: EditTarget::Line {
                path: REFRESH.into(),
                side: Side::Right,
                start: None,
                line: 44,
            },
            text: String::new(),
            error: None,
            ticket: None,
            mode: ComposerMode::Write,
        });
        let ex = excerpt(&r, &ui, &mut cache, REFRESH, &[(43, 45)]).expect("an excerpt");
        assert!(ex.lines[1].0.selected);
        assert!(ex.lines[1].1.contains(&Below::Editor));
        // mona's thread is on line 41.
        let ex = excerpt(&r, &ui, &mut cache, REFRESH, &[(41, 41)]).expect("an excerpt");
        assert!(ex.lines[0].1.iter().any(|b| matches!(b, Below::Thread(_))));
    }

    #[test]
    fn an_excerpt_is_cut_at_the_cap_and_says_how_many_lines_it_left_out() {
        let r = ready();
        let ex = excerpt(
            &r,
            &TabUi::default(),
            &mut HighlightCache::default(),
            "src/auth/store.rs",
            &[(1, 500)],
        )
        .expect("an excerpt");
        assert_eq!(ex.lines.len(), STEP_ROWS);
        assert!(
            ex.hidden > 0,
            "the store.rs hunk has more than {STEP_ROWS} rows"
        );
        assert!(
            excerpt(
                &r,
                &TabUi::default(),
                &mut HighlightCache::default(),
                "nope.rs",
                &[(1, 5)]
            )
            .is_none()
        );
    }

    #[test]
    fn a_unified_row_still_has_its_two_numbers_the_sign_and_the_code() {
        let mut app = wrap_app();
        let (text, both) = code_of(&mut app, LONG);
        assert_eq!(app.world().get::<Children>(both).unwrap().len(), 4);
        assert!(!text_of_line(&app, text).is_empty());
    }

    fn descendants(app: &App, root: Entity) -> HashSet<Entity> {
        let mut out = HashSet::new();
        let mut todo = vec![root];
        while let Some(e) = todo.pop() {
            if let Some(kids) = app.world().get::<Children>(e) {
                for &k in kids {
                    out.insert(k);
                    todo.push(k);
                }
            }
        }
        out
    }

    /// The body's rows: what draws a line of code, and the line buttons.
    fn row_entities(app: &mut App) -> Vec<Entity> {
        let body = testing::find::<BodyPart>(app, |_| true);
        let mut lines: Vec<(Entity, crate::ui::code_select::LineKey)> = code_lines(app)
            .into_iter()
            .filter(|(_, l)| l.key.group == body)
            .map(|(e, l)| (e, l.key))
            .collect();
        lines.sort_by_key(|(_, k)| (k.order, k.column));
        let mut out: Vec<Entity> = lines.into_iter().map(|(e, _)| e).collect();
        let mut q = app.world_mut().query_filtered::<Entity, With<LineButton>>();
        let mut buttons: Vec<Entity> = q.iter(app.world()).collect();
        buttons.sort();
        out.extend(buttons);
        out
    }

    /// The nodes under the body that `change` spawned and despawned.
    fn churn(app: &mut App, change: impl FnOnce(&mut App)) -> (HashSet<Entity>, HashSet<Entity>) {
        let body = testing::find::<BodyPart>(app, |_| true);
        let before = descendants(app, body);
        change(app);
        let after = descendants(app, body);
        (
            after.difference(&before).copied().collect(),
            before.difference(&after).copied().collect(),
        )
    }

    /// Asserts that nothing `churn` saw is a row, and that what it saw hangs under at most
    /// `slots` containers placed straight under the body.
    fn only_under_rows(
        app: &mut App,
        rows: &[Entity],
        (spawned, gone): &(HashSet<Entity>, HashSet<Entity>),
        slots: usize,
    ) {
        for &row in rows {
            assert!(app.world().get_entity(row).is_ok(), "a row was despawned");
            assert!(!spawned.contains(&row));
        }
        assert_eq!(row_entities(app), rows, "no row was spawned or moved");
        let body = testing::find::<BodyPart>(app, |_| true);
        let tops = spawned
            .iter()
            .filter(|&&e| parent_of(app, e) == body)
            .count();
        assert!(tops <= slots, "{tops} containers spawned under the body");
        assert!(gone.len() < 200, "{} nodes despawned", gone.len());
    }

    fn fill_of_line(app: &mut App, side: Side, line: u32) -> Swatch {
        let button = testing::find::<LineButton>(app, |b| b.side == side && b.line == line);
        app.world().get::<Fill>(button).unwrap().0
    }

    #[test]
    fn a_line_click_opens_its_editor_without_respawning_a_row() {
        for mode in [DiffMode::Unified, DiffMode::Split] {
            let mut app = review_app();
            set_mode(&mut app, mode);
            let rows = row_entities(&mut app);
            assert!(!rows.is_empty());
            let line =
                testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == 44);
            let seen = churn(&mut app, |app| {
                testing::activate(app, line);
                testing::settle(app);
            });
            assert!(
                seen.1.is_empty(),
                "{mode:?}: opening an editor despawns nothing"
            );
            assert!(!seen.0.is_empty(), "{mode:?}: the editor is spawned");
            only_under_rows(&mut app, &rows, &seen, 1);
            assert_eq!(testing::count::<EditorArea>(&mut app), 1);
            assert_eq!(fill_of_line(&mut app, Side::Right, 44), Swatch::Selected);
        }
    }

    #[test]
    fn extending_the_selection_relights_rows_in_place() {
        let mut app = review_app();
        let first =
            testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == 39);
        testing::activate(&mut app, first);
        testing::settle(&mut app);
        let rows = row_entities(&mut app);
        assert_eq!(fill_of_line(&mut app, Side::Right, 40), Swatch::AddedBg);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::ShiftLeft);
        let last = testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == 41);
        let seen = churn(&mut app, |app| {
            testing::activate(app, last);
            testing::settle(app);
        });
        // The editor moves from under line 39 to under line 41; no row is touched.
        only_under_rows(&mut app, &rows, &seen, 1);
        for line in [39, 40, 41] {
            assert_eq!(fill_of_line(&mut app, Side::Right, line), Swatch::Selected);
        }
        assert_eq!(testing::count::<EditorArea>(&mut app), 1);
    }

    #[test]
    fn closing_the_editor_removes_it_alone_and_unlights_its_row() {
        let mut app = review_app();
        let line = testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == 44);
        testing::activate(&mut app, line);
        testing::settle(&mut app);
        let rows = row_entities(&mut app);
        let cancel = testing::find::<EditorCancel>(&mut app, |_| true);
        let seen = churn(&mut app, |app| {
            testing::activate(app, cancel);
            testing::settle(app);
        });
        assert!(seen.0.is_empty(), "closing spawns nothing");
        only_under_rows(&mut app, &rows, &seen, 0);
        assert_eq!(testing::count::<EditorArea>(&mut app), 0);
        assert_ne!(fill_of_line(&mut app, Side::Right, 44), Swatch::Selected);
    }

    #[test]
    fn a_focus_line_moves_its_light_and_marker_without_respawning_rows() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        let rows = row_entities(&mut app);
        let seen = churn(&mut app, |app| show_in(app, &pr, REFRESH, Some((44, 44))));
        assert!(
            seen.0.is_empty() && seen.1.is_empty(),
            "nothing is spawned or despawned"
        );
        only_under_rows(&mut app, &rows, &seen, 0);
        assert_eq!(fill_of_line(&mut app, Side::Right, 44), Swatch::Selected);
        let marked = testing::find::<FocusRow>(&mut app, |_| true);
        assert_eq!(
            app.world().get::<LineButton>(marked).map(|b| b.line),
            Some(44)
        );
        show_in(&mut app, &pr, REFRESH, Some((45, 45)));
        assert_eq!(testing::count::<FocusRow>(&mut app), 1);
        assert_ne!(fill_of_line(&mut app, Side::Right, 44), Swatch::Selected);
        assert_eq!(row_entities(&mut app), rows);
    }

    #[test]
    fn show_more_appends_rows_and_keeps_the_ones_shown() {
        for mode in [DiffMode::Unified, DiffMode::Split] {
            let mut app = review_app();
            let pr = fixture::demo_pr();
            {
                let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
                let tab = tabs.0.get_mut(&pr).expect("a tab");
                let Phase::Ready(ready) = &mut tab.phase else {
                    panic!("ready")
                };
                big_file(ready);
                tab.ui.file = Some("big.txt".into());
                tab.ui.mode = mode;
            }
            testing::settle(&mut app);
            let rows = row_entities(&mut app);
            let lines = |rows: &[Entity], app: &mut App| {
                let mut q = app.world_mut().query::<&crate::ui::code_select::CodeLine>();
                rows.iter()
                    .filter(|&&e| q.get(app.world(), e).is_ok())
                    .count()
            };
            // A Split hunk header spans both sides and holds no selectable code.
            let shown = lines(&rows, &mut app);
            assert!((SHOW_STEP - 1..=SHOW_STEP).contains(&shown), "{mode:?}");
            let more = testing::find::<ShowMore>(&mut app, |_| true);
            let (spawned, gone) = churn(&mut app, |app| {
                testing::activate(app, more);
                testing::settle(app);
            });
            let after = row_entities(&mut app);
            assert_eq!(lines(&after, &mut app), shown + SHOW_STEP, "{mode:?}");
            for row in &rows {
                assert!(after.contains(row), "{mode:?}: a shown row was respawned");
            }
            assert!(
                gone.len() < 10,
                "{mode:?}: only the Show more footer goes, not {} nodes",
                gone.len()
            );
            assert!(spawned.len() > SHOW_STEP);
        }
    }

    /// Asserts that the body's rows sit in the order of their lines.
    fn rows_in_line_order(app: &mut App) {
        let body = testing::find::<BodyPart>(app, |_| true);
        let kids = app.world().get::<Children>(body).unwrap().to_vec();
        let mut placed: Vec<(usize, usize)> = code_lines(app)
            .into_iter()
            .filter(|(_, l)| l.key.group == body)
            .map(|(mut e, l)| {
                while parent_of(app, e) != body {
                    e = parent_of(app, e);
                }
                let at = kids.iter().position(|k| *k == e).unwrap();
                (l.key.order, at)
            })
            .collect();
        placed.sort();
        // Both sides of a Split line share its row.
        placed.dedup();
        assert!(
            placed
                .windows(2)
                .all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1),
            "{placed:?}"
        );
    }

    /// Code spans inked with a syntax color other than plain, under the body.
    fn colored_spans(app: &mut App) -> usize {
        use clusia_highlight::Class;
        let body = testing::find::<BodyPart>(app, |_| true);
        let under = descendants(app, body);
        let mut q = app.world_mut().query::<(Entity, &crate::ui::kit::Ink)>();
        q.iter(app.world())
            .filter(|(e, ink)| {
                under.contains(e) && matches!(ink.0, Swatch::Code(c) if c != Class::Plain)
            })
            .count()
    }

    /// The review with its highlights in: computed right away (`background` false) or in the
    /// background and landed.
    fn lit_review_app(background: bool) -> App {
        let mut app = review_app();
        if !background {
            app.insert_resource(HighlightCache::default());
            app.world_mut().resource_mut::<ReviewTabs>().set_changed();
        }
        finish_highlights(app.world_mut());
        testing::settle(&mut app);
        assert!(colored_spans(&mut app) > 0);
        app
    }

    #[test]
    fn a_changed_line_is_respawned_alone_in_its_place() {
        for (mode, background) in [
            (DiffMode::Unified, false),
            (DiffMode::Split, false),
            (DiffMode::Unified, true),
            (DiffMode::Split, true),
        ] {
            let mut app = lit_review_app(background);
            set_mode(&mut app, mode);
            let pr = fixture::demo_pr();
            let before = row_entities(&mut app);
            {
                let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
                let Phase::Ready(ready) = &mut tabs.0.get_mut(&pr).expect("a tab").phase else {
                    panic!("ready")
                };
                let file = ready
                    .view
                    .diff
                    .iter_mut()
                    .find(|f| f.path == REFRESH)
                    .unwrap();
                let patch = file.patch.take().unwrap();
                assert!(patch.contains(LONG));
                file.patch = Some(patch.replace(LONG, "Refresh two minutes early"));
                // A new patch comes with a new head, which highlights it anew.
                ready.view.pr.head_sha = "4b5c6d7e8f9a0b1c2d3e".into();
            }
            testing::settle(&mut app);
            // In the background the new head shows plain first, then its colors land.
            for landed in [false, true] {
                if landed {
                    finish_highlights(app.world_mut());
                    testing::settle(&mut app);
                    assert!(colored_spans(&mut app) > 0, "{mode:?}");
                }
                let after = row_entities(&mut app);
                assert_eq!(before.len(), after.len(), "{mode:?} {background}");
                let fresh = after.iter().filter(|e| !before.contains(e)).count();
                assert!(
                    (1..=4).contains(&fresh),
                    "{mode:?} background {background} landed {landed}: only the changed \
                     line's row is new, not {fresh} entities"
                );
                assert!(
                    code_lines(&mut app)
                        .iter()
                        .any(|(_, l)| l.text.contains("Refresh two minutes early")),
                    "{mode:?}"
                );
                rows_in_line_order(&mut app);
            }
        }
    }

    /// The entities alive in the world.
    fn live(app: &mut App) -> usize {
        let mut q = app.world_mut().query::<Entity>();
        q.iter(app.world()).count()
    }

    #[test]
    fn landing_colors_restyles_the_rows_in_place() {
        for mode in [DiffMode::Unified, DiffMode::Split] {
            let mut app = lit_review_app(true);
            set_mode(&mut app, mode);
            let body = testing::find::<BodyPart>(&mut app, |_| true);
            let rows = row_entities(&mut app);
            let nodes = descendants(&app, body).len();
            let entities = live(&mut app);
            let in_color = colored_spans(&mut app);
            // As if the review were seen for the first time: shown plain, then in color.
            app.insert_resource(HighlightCache::in_background(HIGHLIGHT_CACHE_BYTES));
            app.world_mut().resource_mut::<ReviewTabs>().set_changed();
            app.update();
            assert_eq!(
                colored_spans(&mut app),
                0,
                "{mode:?}: plain inside the frame"
            );
            assert_eq!(
                row_entities(&mut app),
                rows,
                "{mode:?}: going plain keeps the rows"
            );
            finish_highlights(app.world_mut());
            testing::settle(&mut app);
            assert_eq!(
                colored_spans(&mut app),
                in_color,
                "{mode:?}: in color again"
            );
            assert_eq!(
                row_entities(&mut app),
                rows,
                "{mode:?}: the landing keeps the rows"
            );
            assert_eq!(
                descendants(&app, body).len(),
                nodes,
                "{mode:?}: nothing multiplied"
            );
            assert_eq!(live(&mut app), entities, "{mode:?}: nothing leaked");
            rows_in_line_order(&mut app);
        }
    }

    #[test]
    fn expanding_a_thread_respawns_its_card_alone() {
        let mut app = review_app();
        let rows = row_entities(&mut app);
        let toggle = testing::find::<ThreadToggle>(&mut app, |_| true);
        let seen = churn(&mut app, |app| {
            testing::activate(app, toggle);
            testing::settle(app);
        });
        only_under_rows(&mut app, &rows, &seen, 0);
        assert!(testing::shows(
            &mut app,
            "A minute is safer while the runners drift."
        ));
        rows_in_line_order(&mut app);
    }

    #[test]
    fn editors_and_cards_keep_rows_in_line_order() {
        let mut app = review_app();
        for line in [39, 44, 41] {
            let b =
                testing::find::<LineButton>(&mut app, |b| b.side == Side::Right && b.line == line);
            testing::activate(&mut app, b);
            testing::settle(&mut app);
            rows_in_line_order(&mut app);
            let body = testing::find::<BodyPart>(&mut app, |_| true);
            let area = testing::find::<EditorArea>(&mut app, |_| true);
            let mut slot = area;
            while parent_of(&app, slot) != body {
                slot = parent_of(&app, slot);
            }
            let kids = app.world().get::<Children>(body).unwrap().to_vec();
            let at = kids.iter().position(|k| *k == slot).unwrap();
            let above = app.world().get::<LineButton>(kids[at - 1]).map(|b| b.line);
            assert_eq!(above, Some(line), "the editor sits right under its line");
            let cancel = testing::find::<EditorCancel>(&mut app, |_| true);
            testing::activate(&mut app, cancel);
            testing::settle(&mut app);
        }
    }

    #[test]
    fn a_hovered_row_keeps_its_hover_tint_when_its_selection_ends() {
        let mut app = review_app();
        let tokens = app.world().resource::<Theme>().tokens;
        let tint = tokens.get(Swatch::Selected);
        // A line whose own fill reads apart from the hover tint.
        let mut q = app.world_mut().query::<(Entity, &LineButton, &Fill)>();
        let (line, side, number, own) = q
            .iter(app.world())
            .find(|(_, _, f)| tokens.get(f.0) != tint)
            .map(|(e, b, f)| (e, b.side, b.line, f.0))
            .expect("a line that is not tinted like a hover");
        testing::activate(&mut app, line);
        testing::settle(&mut app);
        assert_eq!(fill_of_line(&mut app, side, number), Swatch::Selected);
        app.world_mut().entity_mut(line).insert(Hovered(true));
        testing::settle(&mut app);
        let cancel = testing::find::<EditorCancel>(&mut app, |_| true);
        testing::activate(&mut app, cancel);
        testing::settle(&mut app);
        assert_eq!(app.world().get::<Fill>(line).unwrap().0, own);
        assert_eq!(
            app.world().get::<BackgroundColor>(line).unwrap().0,
            tint,
            "the pointer is still on the row"
        );
    }
}
