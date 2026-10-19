//! The Calls view's side panel and the in-place expansion card, as pure views, and the panel's
//! column with its buttons.
//!
//! The panel is about the selected node, else the graph's centre: its place and signature, the
//! nearest caller's code with the call lit, how far up the callers reach, the other functions
//! whose signature changed, and a comment field. Without a graph it is about the selected file
//! the call graph cannot read.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use clusia_calls::{CallGraph, GraphNode, NodeKind, language_name};
use clusia_core::PrRef;
use clusia_protocol::DiagramCode;

use super::graph::{How, nearest_caller, pick, reach_note};
use super::list::{Tone, node_tag, selected_file};
use super::model::{Calls, CallsModel};
use crate::bridge::{Ask, Asks, Connection, Model, Toasts};
use crate::fonts::UiFonts;
use crate::review_state::{DiffMode, EditTarget, Editor, Modal, Ready, ReviewTabs, TabUi};
use crate::screens::review::diagrams::modal::{
    CodeRow, NOT_IN_DIFF, NodeBody, body_block, code_rows, line_in_diff,
};
use crate::screens::review::diff::{in_diff, show_line, unsent};
use crate::screens::review::editor::{editor_box, not_sent};
use crate::theme::Swatch;
use crate::ui::composer::ComposerMode;
use crate::ui::kit::{Clickable, Fill, Stroke, Type, Variant, button, card, text};

/// Said when a comment is asked for while another one holds typed text.
pub const FINISH_FIRST: &str = "Finish or cancel the comment you are writing first";

/// What a button of the panel does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelAction {
    /// **Show body diff** (a function) or **Show diff** (a file).
    ShowDiff,
    OpenEditor,
    Comment,
    /// **show them**, or a candidate of an ambiguous call: re-centres on that function.
    ShowThem(String),
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PanelButton {
    pub pr: PrRef,
    pub what: PanelAction,
}

/// The buttons of the card the canvas shows for a file with no call graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedWhat {
    Unified,
    Story,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedAction {
    pub pr: PrRef,
    pub what: UnsupportedWhat,
}

/// What the panel is about.
#[derive(Debug, Clone, PartialEq)]
pub enum Subject {
    Function {
        key: String,
        label: String,
        kind: NodeKind,
        /// `src/auth/refresh.rs:38`.
        place: String,
        signature: String,
        /// The function's lines are in the pull request's diff.
        in_diff: bool,
    },
    Outside {
        label: String,
        /// `tokio · outside this repo`.
        detail: String,
    },
    /// A changed file whose language has no pack.
    File {
        path: String,
        /// `+18 −4 · 2 hunks`.
        summary: String,
    },
    Nothing,
}

/// *Called by · TokenStore::token (not changed)* and the lines around the call.
#[derive(Debug, Clone, PartialEq)]
pub struct CalledBy {
    pub heading: String,
    pub body: NodeBody,
}

/// Another function whose signature changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Affected {
    pub key: String,
    pub signature: String,
    /// `4 callers outside this PR`.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PanelView {
    pub subject: Subject,
    pub called_by: Option<CalledBy>,
    pub note: Option<String>,
    pub affected: Vec<Affected>,
    /// The other functions an ambiguous call may reach, among the drawn ones: (id, label).
    pub others: Vec<(String, String)>,
    /// The open comment on a node, drawn in the panel (not while the Full view holds it, so
    /// one text area holds it at a time).
    pub editor: Option<Editor>,
    pub live: bool,
}

/// The node `ui` selects, else the graph's centre.
fn subject_node<'a>(graph: &'a CallGraph, ui: &TabUi) -> Option<&'a GraphNode> {
    let find = |key: &str| graph.nodes.iter().find(|n| n.key == key);
    ui.calls_node
        .as_deref()
        .and_then(find)
        .or_else(|| find(&graph.center))
}

fn place_of(node: &GraphNode) -> String {
    match (&node.path, node.line) {
        (Some(path), Some(line)) => format!("{path}:{line}"),
        (Some(path), None) => path.clone(),
        _ => node.sub.clone(),
    }
}

fn is_changed(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Changed | NodeKind::Signature | NodeKind::Moved | NodeKind::New
    )
}

/// What a function's code block shows: the code, why there is none, or that it is on its way.
fn body_of(code: Option<&DiagramCode>, live: bool) -> NodeBody {
    match code {
        Some(c) => match &c.unavailable {
            Some(why) => NodeBody::Unavailable(why.clone()),
            None => NodeBody::Code(code_rows(c)),
        },
        None if live => NodeBody::Loading,
        None => NodeBody::Offline,
    }
}

/// Whether the node's lines are in the diff: a changed function of a file in the diff, or any
/// function whose first line is a line of the diff.
fn node_in_diff(ready: &Ready, node: &GraphNode) -> bool {
    let Some(path) = &node.path else {
        return false;
    };
    in_diff(ready, path)
        && (is_changed(node.kind) || node.line.is_some_and(|l| line_in_diff(ready, path, l)))
}

fn plural(n: u32, one: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// The panel of `model` for the tab `ui`.
pub fn panel_view(model: &CallsModel, ui: &TabUi, ready: &Ready, live: bool) -> PanelView {
    let editor = ui
        .editor
        .clone()
        .filter(|e| matches!(e.target, EditTarget::CallsNode { .. }))
        .filter(|_| ui.modal != Some(Modal::CallsNode));
    let mut v = PanelView {
        subject: Subject::Nothing,
        called_by: None,
        note: None,
        affected: Vec::new(),
        others: Vec::new(),
        editor,
        live,
    };
    let Some(graph) = model.graph.as_ref() else {
        // A file's comment is a general one: the draft column draws its editor.
        v.editor = None;
        if let Some(path) = selected_file(ui, &model.unsupported) {
            v.subject = Subject::File {
                summary: file_summary(ready, &path),
                path,
            };
        }
        return v;
    };
    let Some(node) = subject_node(graph, ui) else {
        return v;
    };
    if node.kind == NodeKind::Outside {
        v.subject = Subject::Outside {
            label: node.label.clone(),
            detail: node.sub.clone(),
        };
        return v;
    }
    v.subject = Subject::Function {
        key: node.key.clone(),
        label: node.label.clone(),
        kind: node.kind,
        place: place_of(node),
        signature: node.signature.clone(),
        in_diff: node_in_diff(ready, node),
    };
    v.called_by = nearest_caller(graph, &node.key).map(|(caller, edge)| CalledBy {
        heading: format!(
            "Called by · {} ({})",
            caller.label,
            if is_changed(caller.kind) {
                "changed"
            } else {
                "not changed"
            }
        ),
        body: match body_of(model.codes.get(&caller.key), live) {
            NodeBody::Code(rows) => NodeBody::Code(window(rows, edge.line, 4, 1)),
            other => other,
        },
    });
    v.note = reach_note(graph, &node.key);
    v.affected = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Signature && n.key != node.key)
        .map(|n| {
            let inside = graph
                .edges
                .iter()
                .filter(|e| e.to == n.key)
                .filter(|e| {
                    graph
                        .nodes
                        .iter()
                        .any(|c| c.key == e.from && is_changed(c.kind))
                })
                .count() as u32;
            Affected {
                key: n.key.clone(),
                signature: n.signature.clone(),
                detail: format!(
                    "{} outside this PR",
                    plural(n.callers.saturating_sub(inside), "caller")
                ),
            }
        })
        .collect();
    v.others = node
        .ambiguous
        .iter()
        .map(|id| {
            let label = graph
                .nodes
                .iter()
                .find(|n| n.key == id.0)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.0.split('#').nth(1).unwrap_or(&id.0).to_string());
            (id.0.clone(), label)
        })
        .collect();
    v
}

/// `+18 −4 · 2 hunks` for a file of the diff.
fn file_summary(ready: &Ready, path: &str) -> String {
    let Some(f) = ready.view.diff.iter().find(|f| f.path == path) else {
        return String::new();
    };
    let hunks = f
        .patch
        .as_deref()
        .map_or(0, |p| p.lines().filter(|l| l.starts_with("@@")).count()) as u32;
    format!(
        "+{} −{} · {}",
        f.additions,
        f.deletions,
        plural(hunks, "hunk")
    )
}

/// The rows from `before` lines above `line` to `after` lines under it, with `line` lit; none
/// when the code does not hold `line`.
pub fn window(rows: Vec<CodeRow>, line: u32, before: usize, after: usize) -> Vec<CodeRow> {
    if !rows.iter().any(|r| r.number == line) {
        return Vec::new();
    }
    let from = line.saturating_sub(before as u32);
    let to = line.saturating_add(after as u32);
    rows.into_iter()
        .filter(|r| (from..=to).contains(&r.number))
        .map(|r| CodeRow {
            changed: r.number == line,
            ..r
        })
        .collect()
}

/// The expansion card of the selected node.
#[derive(Debug, Clone, PartialEq)]
pub struct CardView {
    pub key: String,
    pub title: String,
    pub tag: Option<String>,
    pub tone: Tone,
    pub place: String,
    pub body: NodeBody,
    pub note: Option<String>,
}

/// The card of the node `ui` selects; none for a *+N* node or one the graph does not hold.
pub fn card_view(model: &CallsModel, ui: &TabUi, live: bool) -> Option<CardView> {
    let key = ui.calls_node.as_deref()?;
    let node = model.graph.as_ref()?.nodes.iter().find(|n| n.key == key)?;
    if node.kind == NodeKind::More {
        return None;
    }
    let tag = node_tag(node.kind);
    Some(CardView {
        key: node.key.clone(),
        title: node.label.clone(),
        tag: tag.map(|(t, _)| t.to_string()),
        tone: tag.map_or(Tone::Plain, |(_, tone)| tone),
        place: place_of(node),
        body: if node.kind == NodeKind::Outside {
            NodeBody::Unavailable("This function is outside the repository.".into())
        } else {
            body_of(model.codes.get(&node.key), live)
        },
        note: (!node.sub.is_empty()).then(|| node.sub.clone()),
    })
}

/// The title of the card shown for a file with no call graph.
pub fn unsupported_title(path: &str) -> String {
    match language_name(path) {
        Some(name) => format!("Call graph not available for {name} yet"),
        None => "Call graph not available for this file yet".into(),
    }
}

// ---- the column

/// The panel's content; rebuilt when its view changes (the typed text excluded, so typing never
/// rebuilds it).
#[derive(Component)]
pub struct PanelPart {
    pub pr: PrRef,
    built: Option<PanelView>,
}

/// The right column of the Calls frame.
pub(super) fn column(p: &mut ChildSpawnerCommands, pr: &PrRef) {
    p.spawn((
        Node {
            width: px(330),
            flex_shrink: 0.0,
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            border: UiRect::left(px(1)),
            overflow: Overflow {
                x: OverflowAxis::Clip,
                y: OverflowAxis::Scroll,
            },
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Bg),
        BorderColor::default(),
        Stroke(Swatch::Line),
        ScrollArea,
        ScrollPosition::default(),
        PanelPart {
            pr: pr.clone(),
            built: None,
        },
    ));
}

/// The view without what is typed: the key a rebuild compares.
fn without_text(v: &PanelView) -> PanelView {
    let mut v = v.clone();
    if let Some(e) = v.editor.as_mut() {
        e.text.clear();
    }
    v
}

pub(super) fn fill_panel(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    calls: Res<Calls>,
    model: Res<Model>,
    fonts: Res<UiFonts>,
    mut parts: Query<(Entity, &mut PanelPart)>,
) {
    let live = model.connection == Connection::Live;
    let empty = CallsModel::default();
    for (entity, mut part) in &mut parts {
        let Some(tab) = tabs.0.get(&part.pr) else {
            continue;
        };
        let Some(ready) = tab.ready() else { continue };
        let m = calls.0.get(&part.pr).unwrap_or(&empty);
        let view = panel_view(m, &tab.ui, ready, live);
        let key = without_text(&view);
        if part.built.as_ref() == Some(&key) {
            continue;
        }
        let pr = part.pr.clone();
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| panel_content(p, &fonts, &pr, &view));
        part.built = Some(key);
    }
}

fn panel_button(fonts: &UiFonts, pr: &PrRef, label: &str, what: PanelAction) -> impl Bundle {
    (
        button(fonts, label, Variant::Secondary),
        PanelButton {
            pr: pr.clone(),
            what,
        },
        observe(on_panel_button),
    )
}

fn buttons(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, diff: &str) {
    p.spawn(Node {
        column_gap: px(8),
        flex_wrap: FlexWrap::Wrap,
        row_gap: px(8),
        ..default()
    })
    .with_children(|b| {
        b.spawn(panel_button(fonts, pr, diff, PanelAction::ShowDiff));
        b.spawn(panel_button(
            fonts,
            pr,
            "Open in editor",
            PanelAction::OpenEditor,
        ));
        b.spawn(panel_button(fonts, pr, "Comment", PanelAction::Comment));
    });
}

fn section(
    p: &mut ChildSpawnerCommands,
    top_border: bool,
    inner: impl FnOnce(&mut ChildSpawnerCommands),
) {
    p.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(10),
            padding: UiRect::axes(px(16), px(14)),
            border: if top_border {
                UiRect::top(px(1))
            } else {
                UiRect::ZERO
            },
            flex_shrink: 0.0,
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(inner);
}

fn heading(p: &mut ChildSpawnerCommands, fonts: &UiFonts, words: &str) {
    p.spawn(text(fonts, words.to_uppercase(), Type::META));
}

fn panel_content(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, v: &PanelView) {
    let mono = |size: f32, ink: Swatch| Type {
        size,
        weight: 400,
        ink,
        mono: true,
    };
    match &v.subject {
        Subject::Nothing => return,
        Subject::Outside { label, detail } => {
            section(p, false, |s| {
                s.spawn(text(fonts, label.clone(), mono(13.0, Swatch::Fg)));
                s.spawn(text(fonts, detail.clone(), Type::MUTED));
                if let Some(e) = &v.editor {
                    editor_box(s, fonts, e, pr);
                }
            });
            return;
        }
        Subject::File { path, summary } => {
            section(p, false, |s| {
                s.spawn(text(fonts, path.clone(), Type::MONO));
                s.spawn(text(fonts, summary.clone(), Type::MUTED));
                buttons(s, fonts, pr, "Show diff");
            });
            return;
        }
        Subject::Function {
            place, signature, ..
        } => {
            section(p, false, |s| {
                s.spawn(text(fonts, place.clone(), Type::MONO));
                s.spawn(text(fonts, signature.clone(), mono(13.0, Swatch::Fg)));
                buttons(s, fonts, pr, "Show body diff");
                if let Some(e) = &v.editor {
                    editor_box(s, fonts, e, pr);
                }
            });
        }
    }
    if v.called_by.is_some() || v.note.is_some() {
        section(p, true, |s| {
            if let Some(by) = &v.called_by {
                heading(s, fonts, &by.heading);
                s.spawn(card(Node {
                    flex_direction: FlexDirection::Column,
                    overflow: Overflow::clip(),
                    ..default()
                }))
                .with_children(|c| body_block(c, fonts, &by.body, 0.0));
            }
            if let Some(note) = &v.note {
                s.spawn(text(fonts, note.clone(), Type::MUTED));
            }
        });
    }
    if !v.others.is_empty() {
        section(p, true, |s| {
            heading(
                s,
                fonts,
                "This call may also reach · among the functions drawn",
            );
            for (key, label) in &v.others {
                s.spawn(panel_button(
                    fonts,
                    pr,
                    label,
                    PanelAction::ShowThem(key.clone()),
                ));
            }
        });
    }
    if !v.affected.is_empty() {
        section(p, true, |s| {
            heading(s, fonts, "Also affected · signature changed");
            for a in &v.affected {
                s.spawn(card(Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: px(6),
                    padding: UiRect::axes(px(12), px(10)),
                    ..default()
                }))
                .insert(Stroke(Swatch::Orange))
                .with_children(|c| {
                    c.spawn(text(fonts, a.signature.clone(), mono(13.0, Swatch::Fg)));
                    c.spawn(Node {
                        column_gap: px(6),
                        align_items: AlignItems::Center,
                        flex_wrap: FlexWrap::Wrap,
                        ..default()
                    })
                    .with_children(|r| {
                        r.spawn(text(fonts, format!("{} ·", a.detail), Type::MUTED));
                        r.spawn((
                            Node::default(),
                            WidgetButton,
                            Clickable,
                            Hovered::default(),
                            PanelButton {
                                pr: pr.clone(),
                                what: PanelAction::ShowThem(a.key.clone()),
                            },
                            observe(on_panel_button),
                        ))
                        .with_children(|l| {
                            l.spawn((text(fonts, "show them", Type::BODY), Underline));
                        });
                    });
                });
            }
        });
    }
}

/// The comment editor on `target`, unless another comment holds typed text.
pub(super) fn open_comment(
    ui: &mut TabUi,
    target: EditTarget,
    toasts: &mut Toasts,
    time: &Time,
) -> bool {
    if ui.editor.as_ref().is_some_and(|e| e.target == target) {
        return true;
    }
    if unsent(ui.editor.as_ref()) {
        not_sent(toasts, time, FINISH_FIRST);
        return false;
    }
    ui.editor = Some(Editor {
        target,
        text: String::new(),
        error: None,
        ticket: None,
        mode: ComposerMode::Write,
    });
    true
}

/// Shows `path` in Unified at `lines`; `false`, changing nothing, when it is not in the diff.
pub(super) fn show_in_unified(
    tab: &mut crate::review_state::Tab,
    path: &str,
    lines: Option<(u32, u32)>,
) -> bool {
    let mode = tab.ui.mode;
    tab.ui.mode = DiffMode::Unified;
    if show_line(tab, path, lines) {
        true
    } else {
        tab.ui.mode = mode;
        false
    }
}

#[allow(clippy::too_many_arguments)]
fn on_panel_button(
    activate: On<Activate>,
    buttons: Query<&PanelButton>,
    mut tabs: ResMut<ReviewTabs>,
    calls: Res<Calls>,
    model: Res<Model>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    let Some(tab) = tabs.0.get_mut(&b.pr) else {
        return;
    };
    let Some(ready) = tab.ready() else { return };
    let empty = CallsModel::default();
    let m = calls.0.get(&b.pr).unwrap_or(&empty);
    let live = model.connection == Connection::Live;
    let view = panel_view(m, &tab.ui, ready, live);
    let worktree = ready.view.worktree.clone();
    let node = |key: &str| {
        m.graph
            .as_ref()
            .and_then(|g| g.nodes.iter().find(|n| n.key == key))
    };
    match (&b.what, &view.subject) {
        (PanelAction::ShowThem(key), _) => match node(key) {
            Some(n) => pick(&mut tab.ui, n, How::Enter),
            None => {
                tab.ui.calls_center = Some(key.clone());
                tab.ui.calls_node = Some(key.clone());
                tab.ui.calls_expand.clear();
            }
        },
        (PanelAction::ShowDiff, Subject::Function { key, in_diff, .. }) => {
            let n = node(key);
            let shown = *in_diff
                && n.and_then(|n| Some((n.path.clone()?, n.line)))
                    .is_some_and(|(path, line)| show_in_unified(tab, &path, line.map(|l| (l, l))));
            if !shown {
                not_sent(&mut toasts, &time, NOT_IN_DIFF);
            }
        }
        (PanelAction::ShowDiff, Subject::File { path, .. }) => {
            if !show_in_unified(tab, path, None) {
                not_sent(&mut toasts, &time, NOT_IN_DIFF);
            }
        }
        (PanelAction::OpenEditor, Subject::Function { key, .. }) => {
            if let (Some(n), Some(worktree)) = (node(key), worktree)
                && let Some(path) = &n.path
            {
                asks.send(Ask::OpenInEditor {
                    path: format!("{worktree}/{path}"),
                    line: n.line,
                });
            }
        }
        (PanelAction::OpenEditor, Subject::File { path, .. }) => {
            if let Some(worktree) = worktree {
                asks.send(Ask::OpenInEditor {
                    path: format!("{worktree}/{path}"),
                    line: None,
                });
            }
        }
        (PanelAction::Comment, Subject::Function { key, .. }) => {
            let target = EditTarget::CallsNode {
                function: key.clone(),
            };
            open_comment(&mut tab.ui, target, &mut toasts, &time);
        }
        (PanelAction::Comment, Subject::File { .. }) => {
            open_comment(&mut tab.ui, EditTarget::General, &mut toasts, &time);
        }
        _ => {}
    }
}

/// **Show in Unified** and **Open in Story** on the card of a file with no call graph.
pub(super) fn on_unsupported(
    activate: On<Activate>,
    buttons: Query<&UnsupportedAction>,
    mut tabs: ResMut<ReviewTabs>,
    calls: Res<Calls>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    let Some(tab) = tabs.0.get_mut(&b.pr) else {
        return;
    };
    match b.what {
        UnsupportedWhat::Story => tab.ui.mode = DiffMode::Story,
        UnsupportedWhat::Unified => {
            let file = calls
                .0
                .get(&b.pr)
                .and_then(|m| selected_file(&tab.ui, &m.unsupported));
            tab.ui.mode = DiffMode::Unified;
            if !file.is_some_and(|path| show_line(tab, &path, None)) {
                not_sent(&mut toasts, &time, NOT_IN_DIFF);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screens::review::calls::testkit::*;
    use crate::testing;
    use clusia_protocol::{CodeLine, DiagramCode};

    fn lines(path: &str, from: u32, to: u32) -> DiagramCode {
        DiagramCode {
            path: path.into(),
            commit: "9b0c7de4a1c2".into(),
            start_line: from,
            lines: (from..=to)
                .map(|n| CodeLine {
                    number: n,
                    text: format!("    step_{n}();"),
                    changed: false,
                })
                .collect(),
            unavailable: None,
        }
    }

    fn row(number: u32) -> CodeRow {
        CodeRow {
            number,
            spans: Vec::new(),
            changed: false,
        }
    }

    #[test]
    fn a_window_is_the_lines_around_the_call_with_the_call_tinted() {
        let rows: Vec<CodeRow> = (10..=40).map(row).collect();
        let w = window(rows, 25, 4, 1);
        let numbers: Vec<u32> = w.iter().map(|r| r.number).collect();
        assert_eq!(numbers, [21, 22, 23, 24, 25, 26]);
        let tinted: Vec<u32> = w.iter().filter(|r| r.changed).map(|r| r.number).collect();
        assert_eq!(tinted, [25]);
        let top = window((10..=40).map(row).collect(), 11, 4, 1);
        assert_eq!(
            top.first().map(|r| r.number),
            Some(10),
            "the window stops at the first line"
        );
        assert!(
            window((10..=12).map(row).collect(), 99, 4, 1).is_empty(),
            "a line not in the code shows nothing"
        );
    }

    #[test]
    fn the_panel_for_the_centre_has_its_place_signature_and_nearest_caller() {
        let app = testing::demo_review_app();
        let pr = crate::fixture::demo_pr();
        let ready = testing::ready(&app, &pr);
        let mut m = CallsModel {
            graph: Some(sample()),
            ..CallsModel::default()
        };
        let v = panel_view(&m, &TabUi::default(), &ready, true);
        assert_eq!(
            v.subject,
            Subject::Function {
                key: REFRESH.into(),
                label: "TokenStore::refresh".into(),
                kind: NodeKind::Changed,
                place: "src/auth/refresh.rs:38".into(),
                signature: "fn TokenStore::refresh()".into(),
                in_diff: true,
            }
        );
        let by = v.called_by.expect("a caller");
        assert_eq!(by.heading, "Called by · TokenStore::token (not changed)");
        assert_eq!(by.body, NodeBody::Loading, "the code is on its way");
        assert!(
            v.note
                .as_deref()
                .unwrap()
                .starts_with("Reached from 9 callers in 2 levels"),
            "{:?}",
            v.note
        );
        assert_eq!(
            v.affected,
            [Affected {
                key: NEW.into(),
                signature: "fn TokenStore::new()".into(),
                detail: "4 callers outside this PR".into(),
            }]
        );
        m.codes
            .insert(TOKEN.into(), lines("src/auth/store.rs", 20, 30));
        let by = panel_view(&m, &TabUi::default(), &ready, true)
            .called_by
            .unwrap();
        let NodeBody::Code(rows) = by.body else {
            panic!("code")
        };
        assert_eq!(
            rows.iter().map(|r| r.number).collect::<Vec<_>>(),
            [21, 22, 23, 24, 25, 26]
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r.changed)
                .map(|r| r.number)
                .collect::<Vec<_>>(),
            [25]
        );
        let offline = panel_view(
            &CallsModel {
                graph: Some(sample()),
                ..CallsModel::default()
            },
            &TabUi::default(),
            &ready,
            false,
        );
        assert_eq!(offline.called_by.unwrap().body, NodeBody::Offline);
    }

    #[test]
    fn a_selected_node_takes_the_panel_and_an_outside_one_has_no_code() {
        let app = testing::demo_review_app();
        let ready = testing::ready(&app, &crate::fixture::demo_pr());
        let m = CallsModel {
            graph: Some(sample()),
            ..CallsModel::default()
        };
        let ui = TabUi {
            calls_node: Some(TOKEN.into()),
            ..TabUi::default()
        };
        let v = panel_view(&m, &ui, &ready, true);
        let Subject::Function {
            label,
            in_diff,
            place,
            ..
        } = v.subject
        else {
            panic!("a function")
        };
        assert_eq!(
            (label.as_str(), in_diff, place.as_str()),
            ("TokenStore::token", false, "src/auth/store.rs:20")
        );
        assert_eq!(
            v.called_by.unwrap().heading,
            "Called by · auth_middleware (not changed)"
        );
        let ui = TabUi {
            calls_node: Some(LOCK.into()),
            ..TabUi::default()
        };
        let v = panel_view(&m, &ui, &ready, true);
        assert_eq!(
            v.subject,
            Subject::Outside {
                label: "Mutex::lock".into(),
                detail: "tokio · outside this repo".into()
            }
        );
    }

    #[test]
    fn an_ambiguous_node_lists_its_other_candidates() {
        let app = testing::demo_review_app();
        let ready = testing::ready(&app, &crate::fixture::demo_pr());
        let mut g = sample();
        g.nodes[5].ambiguous = vec![
            clusia_calls::FunctionId(TOKEN.into()),
            clusia_calls::FunctionId(SAVE.into()),
        ];
        let m = CallsModel {
            graph: Some(g),
            ..CallsModel::default()
        };
        let ui = TabUi {
            calls_node: Some(EXCHANGE.into()),
            ..TabUi::default()
        };
        let v = panel_view(&m, &ui, &ready, true);
        assert_eq!(
            v.others,
            [
                (TOKEN.to_string(), "TokenStore::token".to_string()),
                (SAVE.to_string(), "TokenStore::save".to_string())
            ]
        );
    }

    #[test]
    fn an_unsupported_file_is_the_panels_subject_with_its_counts() {
        let app = testing::demo_review_app();
        let ready = testing::ready(&app, &crate::fixture::demo_pr());
        let m = CallsModel {
            unsupported: vec!["src/auth/refresh.rs".into()],
            ..CallsModel::default()
        };
        let v = panel_view(&m, &TabUi::default(), &ready, true);
        let Subject::File { path, summary } = v.subject else {
            panic!("a file")
        };
        assert_eq!(path, "src/auth/refresh.rs");
        assert!(
            summary.starts_with('+') && summary.contains(" hunks"),
            "{summary}"
        );
        assert_eq!(
            panel_view(&CallsModel::default(), &TabUi::default(), &ready, true).subject,
            Subject::Nothing
        );
    }

    #[test]
    fn the_card_shows_loading_offline_unavailable_or_the_code() {
        let ui = TabUi {
            calls_node: Some(TOKEN.into()),
            ..TabUi::default()
        };
        let mut m = CallsModel {
            graph: Some(sample()),
            ..CallsModel::default()
        };
        let c = card_view(&m, &ui, true).expect("a card");
        assert_eq!(
            (
                c.key.as_str(),
                c.title.as_str(),
                c.tag.as_deref(),
                c.place.as_str()
            ),
            (TOKEN, "TokenStore::token", None, "src/auth/store.rs:20")
        );
        assert_eq!(c.body, NodeBody::Loading);
        assert_eq!(c.note.as_deref(), Some("not touched · 4 callers"));
        assert_eq!(card_view(&m, &ui, false).unwrap().body, NodeBody::Offline);
        let mut gone = lines("src/auth/store.rs", 1, 1);
        gone.lines.clear();
        gone.unavailable = Some("The code at 9b0c7de is no longer available.".into());
        m.codes.insert(TOKEN.into(), gone);
        assert_eq!(
            card_view(&m, &ui, true).unwrap().body,
            NodeBody::Unavailable("The code at 9b0c7de is no longer available.".into())
        );
        m.codes
            .insert(TOKEN.into(), lines("src/auth/store.rs", 20, 24));
        let NodeBody::Code(rows) = card_view(&m, &ui, true).unwrap().body else {
            panic!("code")
        };
        assert_eq!(rows.len(), 5);
    }

    #[test]
    fn the_card_has_a_tag_for_changed_nodes_none_for_more_and_a_note_for_outside() {
        let m = CallsModel {
            graph: Some(sample()),
            ..CallsModel::default()
        };
        let card = |key: &str| {
            let ui = TabUi {
                calls_node: Some(key.into()),
                ..TabUi::default()
            };
            card_view(&m, &ui, true)
        };
        assert_eq!(card(SAVE).unwrap().tag.as_deref(), Some("moved"));
        assert_eq!(card(REFRESH).unwrap().tag.as_deref(), Some("changed"));
        assert_eq!(card(NEW).unwrap().tag.as_deref(), Some("signature"));
        assert!(card(MORE).is_none(), "a +N node opens no card");
        let outside = card(LOCK).unwrap();
        assert_eq!(
            outside.body,
            NodeBody::Unavailable("This function is outside the repository.".into())
        );
        assert!(card("src/gone.rs#gone#1").is_none());
    }

    #[test]
    fn the_unsupported_card_names_the_language() {
        assert_eq!(
            unsupported_title("server/handler.go"),
            "Call graph not available for Go yet"
        );
        assert_eq!(
            unsupported_title("README.xyz"),
            "Call graph not available for this file yet"
        );
    }
}
