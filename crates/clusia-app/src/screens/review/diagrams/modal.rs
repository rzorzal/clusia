//! The dialog that opens on a diagram box (mockup `DiagramNode.png`): the code the box stands
//! for, read by the daemon at the version's commit, a comment field, and the way the comment
//! gets into the draft.
//!
//! `TabUi.modal == Some(Modal::DiagramNode)` with `TabUi.diagram_node` (version number, diagram
//! key, box id) asks for it. The comment is written in `TabUi.editor` (target
//! `EditTarget::DiagramNode`), so the typed text, the ticket and the daemon's `Saved` and
//! `Refused` work as in every other editor. The daemon takes the anchor from its stored drawing
//! and decides whether the comment lands on the lines or in the review body.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, ScrollArea, observe};
use clusia_core::diagram::{Change, CodeAnchor, DiagramVersion, EntryState, Node as BoxNode};
use clusia_core::draft::{Anchor, DraftKind, Origin, Side};
use clusia_core::keywords::{KeywordMap, parse_place_node};
use clusia_core::printable::{printable, printable_lines};
use clusia_core::{PrRef, Review, commentable_lines};
use clusia_diagram::keywords::ELSEWHERE;
use clusia_highlight::highlight;
use clusia_protocol::DiagramCode;

use crate::bridge::{Ask, Asks, Connection, Model, Toasts};
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen};
use crate::review_state::{EditTarget, Editor, Modal, Phase, Ready, ReviewTabs, Tab, TabUi};
use crate::screens::review::ReviewSystems;
use crate::screens::review::diagrams::keywords::use_case_name;
use crate::screens::review::diagrams::{Diagrams, DiagramsModel};
use crate::screens::review::diff::{in_diff, show_line, unsent};
use crate::screens::review::editor::{
    AreaTarget, EDITOR_AREA, EditorArea, EditorSubmit, not_sent, on_submit_button, read_only_reason,
};
use crate::screens::review::shell::ModalFor;
use crate::theme::Swatch;
use crate::ui::code::{CodeSpan, code_line, spans_for};
use crate::ui::composer::ComposerMode;
use crate::ui::kit::{Fill, Stroke, Type, Variant, button, disabled_button, panel, text};
use crate::ui::modal::{escape_pressed, modal_card, modal_root};
use crate::ui::text_area::text_area;

/// Said in place of the code when a box is not tied to any.
pub const NO_ANCHOR: &str = "This box is not tied to code. Your comment goes to the review body.";

/// Why *Open in diff* is off.
pub const NOT_IN_DIFF: &str = "This file is not in the diff";

/// Rows of code the dialog shows at most (the daemon sends no more than this).
const MAX_CODE_ROWS: usize = 200;

/// The tallest the code block grows: nine 21 px rows and its padding. Longer code scrolls
/// inside it, so the comment field and the footer stay in the window.
const CODE_MAX_HEIGHT: f32 = 9.0 * 21.0 + 20.0;

/// What the dialog shows in place of the code.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeBody {
    /// The daemon has not answered yet.
    Loading,
    /// There is no connection to ask.
    Offline,
    /// The daemon could not read the code ("The code at 3f2a1c4 is no longer available.").
    Unavailable(String),
    NoAnchor,
    Code(Vec<CodeRow>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodeRow {
    pub number: u32,
    pub spans: Vec<CodeSpan>,
    /// A line the pull request adds at the newest commit: tinted.
    pub changed: bool,
}

/// *Open in diff*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffJump {
    /// A box with no code has nothing to open.
    Hidden,
    Ready,
    NotInDiff,
}

/// Everything the dialog draws, already printable.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeView {
    pub label: String,
    pub change: Change,
    /// `src/auth/refresh.rs · 38–48`.
    pub place: Option<String>,
    pub note: Option<String>,
    pub body: NodeBody,
    /// Where the comment lands, under the field.
    pub hint: Option<String>,
    pub jump: DiffJump,
    /// The daemon's refusal.
    pub error: Option<String>,
    /// The comment waits for the daemon.
    pub saving: bool,
    /// Why nothing can be added (the cached copy, no connection).
    pub blocked: Option<&'static str>,
}

/// The box `node` of the drawing `key` in `version`.
pub fn node_of<'a>(version: &'a DiagramVersion, key: &str, node: &str) -> Option<&'a BoxNode> {
    match &version.entry(key)?.state {
        EntryState::Ready { spec, .. } => spec.nodes.iter().find(|n| n.id == node),
        _ => None,
    }
}

/// The code the box stands for, when it has any.
pub fn node_anchor<'a>(
    version: &'a DiagramVersion,
    key: &str,
    node: &str,
) -> Option<&'a CodeAnchor> {
    node_of(version, key, node)?.anchor.as_ref()
}

/// Said under the comment field of a place the pull request does not change.
pub const PLACE_NOT_IN_DIFF: &str = "Not in the diff: the comment goes to the review body.";

/// The place `node` names (`p<N>`) in the keyword map of the entry `key`, with its index.
pub fn place_of<'a>(
    version: &'a DiagramVersion,
    key: &str,
    node: &str,
) -> Option<(&'a KeywordMap, usize)> {
    let index = parse_place_node(node)?;
    match &version.entry(key)?.state {
        EntryState::ReadyMap { map, .. } if index < map.places.len() => Some((&**map, index)),
        _ => None,
    }
}

/// The code `node` stands for: a box's anchor, or the line of a place.
pub fn anchor_of(version: &DiagramVersion, key: &str, node: &str) -> Option<CodeAnchor> {
    node_anchor(version, key, node).cloned().or_else(|| {
        let (map, index) = place_of(version, key, node)?;
        map.anchor(index)
    })
}

/// Whether `node` is a box or a place of `key` in `version`.
pub fn node_exists(version: &DiagramVersion, key: &str, node: &str) -> bool {
    node_of(version, key, node).is_some() || place_of(version, key, node).is_some()
}

/// Whether `line` of `path` is a line of the pull request's diff a comment can be put on.
pub fn line_in_diff(ready: &Ready, path: &str, line: u32) -> bool {
    ready
        .view
        .diff
        .iter()
        .find(|f| f.path == path)
        .and_then(|f| f.patch.as_deref())
        .is_some_and(|patch| commentable_lines(patch).1.contains(&line))
}

/// What the dialog is about: a box of a drawing or a place of a keyword map.
struct Subject {
    label: String,
    change: Change,
    note: Option<String>,
    anchor: Option<CodeAnchor>,
    /// A place: its hint says where the comment lands by the diff, not by the daemon's words.
    place: bool,
}

fn subject(version: &DiagramVersion, key: &str, node: &str) -> Option<Subject> {
    if let Some(n) = node_of(version, key, node) {
        return Some(Subject {
            label: printable(&n.label),
            change: n.change,
            note: n.note.as_deref().map(printable).filter(|n| !n.is_empty()),
            anchor: n.anchor.clone(),
            place: false,
        });
    }
    let (map, index) = place_of(version, key, node)?;
    let place = map.places.get(index)?;
    let use_case = map
        .use_case_of(place)
        .and_then(|u| u16::try_from(u).ok())
        .unwrap_or(ELSEWHERE);
    let family = map.plan.families.get(map.family_of(place))?;
    Some(Subject {
        label: printable(&map.words.get(usize::from(place.word))?.text),
        change: if place.changed() {
            Change::Changed
        } else {
            Change::Unchanged
        },
        note: Some(format!(
            "{} · {}",
            printable(&use_case_name(map, use_case)),
            printable(&family.label)
        )),
        anchor: map.anchor(index),
        place: true,
    })
}

/// Where a comment on a place lands: on its line when the drawing is of the newest commit and
/// the line is in the diff, in the review body when it is not, and wherever the line has moved
/// to (or the body) when the drawing is older.
fn place_hint(commit: &str, head: &str, a: &CodeAnchor, in_diff: bool) -> String {
    let line = a.start_line;
    if commit != head {
        format!(
            "Lands in the draft as a comment on line {line} of the code at {}: it moves to the \
             newest commit when that line is still in the change, and goes to the review body \
             otherwise.",
            short(commit)
        )
    } else if in_diff {
        format!("Lands in the draft as a comment on line {line}, the code this place stands for.")
    } else {
        PLACE_NOT_IN_DIFF.to_string()
    }
}

/// `38–48`, or `38` for one line.
pub fn lines_text(a: &CodeAnchor) -> String {
    if a.start_line == a.end_line {
        a.start_line.to_string()
    } else {
        format!("{}–{}", a.start_line, a.end_line)
    }
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

pub(crate) fn code_rows(code: &DiagramCode) -> Vec<CodeRow> {
    let lines: Vec<String> = code
        .lines
        .iter()
        .take(MAX_CODE_ROWS)
        .map(|l| printable_lines(&l.text).replace('\n', "\\n"))
        .collect();
    let lit = highlight(&code.path, &lines.join("\n"));
    code.lines
        .iter()
        .zip(&lines)
        .enumerate()
        .map(|(i, (line, text))| CodeRow {
            number: line.number,
            spans: spans_for(text, lit.get(i).map_or(&[][..], |s| s.as_slice()), None),
            changed: line.changed,
        })
        .collect()
}

/// The dialog for `target` (version number, key, box or place id), or `None` when the drawing
/// or what it names is not there.
pub fn node_view(
    tab: &Tab,
    target: &(u32, String, String),
    version: &DiagramVersion,
    code: Option<&DiagramCode>,
    live: bool,
    read_only: Option<&'static str>,
) -> Option<NodeView> {
    let ready = tab.ready()?;
    let subject = subject(version, &target.1, &target.2)?;
    let head = &ready.view.review.head_sha;
    let body = match (&subject.anchor, code) {
        (None, _) => NodeBody::NoAnchor,
        (Some(_), Some(c)) => match &c.unavailable {
            Some(why) => NodeBody::Unavailable(printable(why)),
            None => NodeBody::Code(code_rows(c)),
        },
        (Some(_), None) if !live => NodeBody::Offline,
        (Some(_), None) => NodeBody::Loading,
    };
    let hint = subject.anchor.as_ref().map(|a| {
        if subject.place {
            return place_hint(
                &version.commit,
                head,
                a,
                line_in_diff(ready, &a.path, a.start_line),
            );
        }
        let lines = lines_text(a);
        if &version.commit == head {
            format!(
                "Lands in the draft as a comment on lines {lines}, the code this box stands for."
            )
        } else {
            format!(
                "Lands in the draft as a comment on lines {lines} of the code at {}: it moves to \
                 the newest commit when those lines are still in the change, and goes to the \
                 review body otherwise.",
                short(&version.commit)
            )
        }
    });
    let jump = match &subject.anchor {
        None => DiffJump::Hidden,
        Some(a) if in_diff(ready, &a.path) => DiffJump::Ready,
        Some(_) => DiffJump::NotInDiff,
    };
    let editor = tab.ui.editor.as_ref().filter(|e| {
        matches!(&e.target, EditTarget::DiagramNode { number, key, node }
            if (*number, key, node) == (target.0, &target.1, &target.2))
    });
    Some(NodeView {
        label: subject.label,
        change: subject.change,
        place: subject
            .anchor
            .as_ref()
            .map(|a| format!("{} · {}", printable(&a.path), lines_text(a))),
        note: subject.note,
        body,
        hint,
        jump,
        error: editor.and_then(|e| e.error.clone()),
        saving: editor.is_some_and(|e| e.ticket.is_some()),
        blocked: read_only,
    })
}

/// The dot before the title: the box's fill and ring.
fn dot_of(change: Change) -> (Swatch, Swatch) {
    match change {
        Change::Changed => (Swatch::ChangedBg, Swatch::Green),
        Change::New => (Swatch::GreenSoft, Swatch::Green),
        Change::Risk => (Swatch::RiskBg, Swatch::Orange),
        Change::Unchanged => (Swatch::Chrome, Swatch::Line),
    }
}

// --- the demo daemon -------------------------------------------------------------------

/// What the demo daemon answers to `ReadDiagramCode`: the code of the anchor of the box or the
/// place, taken from the patch the demo review holds (the head is the only commit the demo has);
/// a place in a file the demo holds no patch of gets `fixture::demo_place_code`.
pub fn demo_code_for(
    model: Option<&DiagramsModel>,
    ready: Option<&Ready>,
    number: u32,
    key: &str,
    node: &str,
) -> DiagramCode {
    let version = model.and_then(|m| m.loaded.get(&number));
    let commit = version.map(|v| v.commit.clone()).unwrap_or_default();
    let anchor = version.and_then(|v| anchor_of(v, key, node));
    let (Some(anchor), Some(ready)) = (anchor, ready) else {
        return DiagramCode {
            path: String::new(),
            commit: commit.clone(),
            start_line: 0,
            lines: Vec::new(),
            unavailable: Some(format!(
                "The code at {} is no longer available.",
                short(&commit)
            )),
        };
    };
    let at_head = commit == ready.view.review.head_sha;
    let patch = ready
        .view
        .diff
        .iter()
        .find(|f| f.path == anchor.path)
        .and_then(|f| f.patch.as_deref());
    let is_place = version.is_some_and(|v| place_of(v, key, node).is_some());
    // A place's window starts one line above it, as the daemon's does.
    let first = if is_place {
        anchor.start_line.saturating_sub(1).max(1)
    } else {
        anchor.start_line
    };
    let code = crate::fixture::demo_diagram_code(
        &anchor.path,
        &commit,
        patch,
        (first, anchor.end_line),
        at_head,
    );
    if code.unavailable.is_some() && is_place {
        return crate::fixture::demo_place_code(&anchor.path, &commit, anchor.start_line);
    }
    code
}

/// What the demo daemon does with a comment on a box: a line comment at the head when every
/// line is in the change, else a general comment that names `path:lines`.
pub fn diagram_comment(
    review: &mut Review,
    anchor: Option<&CodeAnchor>,
    patch: Option<&str>,
    body: &str,
    now: i64,
) -> Result<(), String> {
    let on_diff = |a: &CodeAnchor| {
        patch.is_some_and(|p| {
            let (_, right) = commentable_lines(p);
            (a.start_line..=a.end_line).all(|n| right.contains(&n))
        })
    };
    let added = match anchor {
        Some(a) if on_diff(a) => review.draft.add_as(
            Origin::Diagram,
            DraftKind::LineComment,
            Some(Anchor {
                commit: review.head_sha.clone(),
                path: a.path.clone(),
                line: a.end_line,
                start_line: (a.start_line < a.end_line).then_some(a.start_line),
                side: Side::Right,
            }),
            None,
            body,
            now,
        ),
        Some(a) => {
            let lines = if a.start_line == a.end_line {
                a.start_line.to_string()
            } else {
                format!("{}-{}", a.start_line, a.end_line)
            };
            review.draft.add_as(
                Origin::Diagram,
                DraftKind::General,
                None,
                None,
                &format!("{}:{lines}: {body}", a.path),
                now,
            )
        }
        None => review
            .draft
            .add_as(Origin::Diagram, DraftKind::General, None, None, body, now),
    };
    added.map(|_| ()).map_err(|e| e.to_string())
}

// --- the dialog ---------------------------------------------------------------------------

/// The root of the dialog of `pr`.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct NodeModal(pub PrRef);

#[derive(Component)]
struct NodePart {
    pr: PrRef,
    built: Option<NodeView>,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct NodeCancel(pub PrRef);

/// The scrolling block that holds the code.
#[derive(Component)]
struct NodeCode;

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct NodeOpenDiff(pub PrRef);

pub struct NodeModalPlugin;

impl Plugin for NodeModalPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                open_node,
                ask_code,
                rebuild_modal,
                close_when_done,
                escape_closes,
            )
                .chain()
                .after(ReviewSystems),
        );
    }
}

/// Puts the dialog away: the modal, the box it was for and the comment being written.
fn close(ui: &mut TabUi) {
    ui.modal = None;
    ui.diagram_node = None;
    if matches!(
        ui.editor.as_ref().map(|e| &e.target),
        Some(EditTarget::DiagramNode { .. })
    ) {
        ui.editor = None;
    }
}

fn open_node(
    mut commands: Commands,
    nav: Res<Nav>,
    mut tabs: ResMut<ReviewTabs>,
    modals: Query<&ModalFor>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Screen::Review(pr) = &nav.screen else {
        return;
    };
    let requested = tabs
        .0
        .get(pr)
        .is_some_and(|t| t.ui.diagram_node.is_some() && t.ui.modal.is_none());
    if requested && let Some(tab) = tabs.0.get_mut(pr) {
        tab.ui.modal = Some(Modal::DiagramNode);
    }
    let Some(tab) = tabs.0.get(pr) else { return };
    if tab.ui.modal != Some(Modal::DiagramNode) || !matches!(tab.phase, Phase::Ready(_)) {
        return;
    }
    if modals
        .iter()
        .any(|m| &m.pr == pr && m.modal == Modal::DiagramNode)
    {
        return;
    }
    let Some((number, key, node)) = tab.ui.diagram_node.clone() else {
        if let Some(tab) = tabs.0.get_mut(pr) {
            tab.ui.modal = None;
        }
        return;
    };
    if unsent(tab.ui.editor.as_ref()) {
        not_sent(
            &mut toasts,
            &time,
            "Finish or cancel the comment you are writing first",
        );
        if let Some(tab) = tabs.0.get_mut(pr) {
            tab.ui.modal = None;
            tab.ui.diagram_node = None;
        }
        return;
    }
    if let Some(tab) = tabs.0.get_mut(pr) {
        tab.ui.editor = Some(Editor {
            target: EditTarget::DiagramNode { number, key, node },
            text: String::new(),
            error: None,
            ticket: None,
            mode: ComposerMode::Write,
        });
    }
    commands
        .spawn((
            modal_root(),
            ModalFor {
                pr: pr.clone(),
                modal: Modal::DiagramNode,
            },
            NodeModal(pr.clone()),
        ))
        .with_children(|root| {
            root.spawn((
                modal_card(620.0),
                NodePart {
                    pr: pr.clone(),
                    built: None,
                },
            ));
        });
}

/// Asks the daemon for the box's code once per opening, as soon as there is a connection, and
/// again when a redraw took the answer away.
fn ask_code(
    nav: Res<Nav>,
    tabs: Res<ReviewTabs>,
    diagrams: Res<Diagrams>,
    model: Res<Model>,
    mut asked: Local<Option<(PrRef, u32, String, String)>>,
    mut asks: ResMut<Asks>,
) {
    let wanted = match &nav.screen {
        Screen::Review(pr) => tabs.0.get(pr).and_then(|t| {
            let target = t.ui.diagram_node.clone()?;
            (t.ui.modal == Some(Modal::DiagramNode)).then_some((pr.clone(), target))
        }),
        _ => None,
    };
    let Some((pr, (number, key, node))) = wanted else {
        *asked = None;
        return;
    };
    let has_anchor = diagrams
        .0
        .get(&pr)
        .and_then(|m| m.loaded.get(&number))
        .is_some_and(|v| anchor_of(v, &key, &node).is_some());
    let known = diagrams
        .0
        .get(&pr)
        .is_some_and(|m| m.code.contains_key(&(number, key.clone(), node.clone())));
    // Once answered, a redraw that clears the code makes it wanted again.
    if known {
        *asked = None;
        return;
    }
    let id = (pr.clone(), number, key.clone(), node.clone());
    if !has_anchor || model.connection != Connection::Live || asked.as_ref() == Some(&id) {
        return;
    }
    *asked = Some(id);
    asks.send(Ask::ReadDiagramCode {
        pr,
        number,
        key,
        node,
    });
}

fn rebuild_modal(
    mut commands: Commands,
    mut tabs: ResMut<ReviewTabs>,
    diagrams: Res<Diagrams>,
    model: Res<Model>,
    fonts: Res<UiFonts>,
    mut parts: Query<(Entity, &mut NodePart)>,
) {
    let mut gone = Vec::new();
    for (entity, mut part) in &mut parts {
        if part.built.is_some()
            && !tabs.is_changed()
            && !diagrams.is_changed()
            && !model.is_changed()
        {
            continue;
        }
        let Some(tab) = tabs.0.get(&part.pr) else {
            continue;
        };
        let Some(target) = tab.ui.diagram_node.clone() else {
            continue;
        };
        let Some(dm) = diagrams.0.get(&part.pr) else {
            continue;
        };
        let Some(version) = dm.loaded.get(&target.0) else {
            continue;
        };
        // A redraw without the box leaves nothing for the comment to land on.
        if !node_exists(version, &target.1, &target.2) {
            gone.push(part.pr.clone());
            continue;
        }
        let live = model.connection == Connection::Live;
        let read_only = read_only_reason(&tabs, &model, &part.pr);
        let Some(view) = node_view(tab, &target, version, dm.code.get(&target), live, read_only)
        else {
            continue;
        };
        if part.built.as_ref() == Some(&view) {
            continue;
        }
        let pr = part.pr.clone();
        let typed = tab
            .ui
            .editor
            .as_ref()
            .map(|e| e.text.clone())
            .unwrap_or_default();
        let area = AreaTarget::new(EditTarget::DiagramNode {
            number: target.0,
            key: target.1.clone(),
            node: target.2.clone(),
        });
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| card_content(p, &fonts, &pr, &view, &typed, area));
        part.built = Some(view);
    }
    for pr in gone {
        if let Some(tab) = tabs.0.get_mut(&pr) {
            close(&mut tab.ui);
        }
    }
}

fn card_content(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    v: &NodeView,
    typed: &str,
    area: AreaTarget,
) {
    p.spawn(Node {
        align_items: AlignItems::Center,
        column_gap: px(10),
        padding: UiRect {
            left: px(24),
            right: px(24),
            top: px(20),
            bottom: px(12),
        },
        ..default()
    })
    .with_children(|h| {
        let (fill, ring) = dot_of(v.change);
        h.spawn((
            Node {
                width: px(10),
                height: px(10),
                flex_shrink: 0.0,
                border: UiRect::all(px(1.5)),
                border_radius: BorderRadius::all(px(3)),
                ..default()
            },
            BackgroundColor::default(),
            Fill(fill),
            BorderColor::default(),
            Stroke(ring),
        ));
        h.spawn(text(fonts, v.label.clone(), Type::HEADING.size(16.0)));
        if let Some(place) = &v.place {
            h.spawn(text(fonts, place.clone(), Type::MONO));
        }
        h.spawn(Node {
            flex_grow: 1.0,
            ..default()
        });
        match v.jump {
            DiffJump::Hidden => {}
            DiffJump::Ready => {
                h.spawn((
                    button(fonts, "Open in diff", Variant::Secondary),
                    NodeOpenDiff(pr.clone()),
                    observe(on_open_diff),
                ));
            }
            DiffJump::NotInDiff => {
                h.spawn(disabled_button(fonts, "Open in diff"));
            }
        }
    });
    if let Some(note) = &v.note {
        p.spawn(Node {
            padding: UiRect {
                left: px(24),
                right: px(24),
                top: px(0),
                bottom: px(8),
            },
            ..default()
        })
        .with_children(|n| {
            n.spawn(text(fonts, note.clone(), Type::MUTED));
        });
    }
    body_block(p, fonts, &v.body, 24.0);
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(8),
        padding: UiRect {
            left: px(24),
            right: px(24),
            top: px(16),
            bottom: px(8),
        },
        ..default()
    })
    .with_children(|c| {
        c.spawn(text(fonts, "Comment", Type::STRONG));
        c.spawn((
            text_area(fonts, typed, 3.0, EDITOR_AREA),
            EditorArea(pr.clone()),
            area,
        ))
        .insert(Stroke(Swatch::Green));
        if let Some(error) = &v.error {
            c.spawn(text(fonts, error.clone(), Type::BODY.ink(Swatch::Orange)));
        }
        if let Some(reason) = v.blocked {
            c.spawn(text(fonts, reason, Type::META.ink(Swatch::Orange)));
        }
        if let Some(hint) = &v.hint {
            c.spawn(text(fonts, hint.clone(), Type::META));
        }
    });
    p.spawn((
        panel(
            Node {
                justify_content: JustifyContent::FlexEnd,
                column_gap: px(8),
                padding: UiRect::axes(px(24), px(14)),
                border: UiRect::top(px(1)),
                ..default()
            },
            Swatch::Chrome,
        ),
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|f| {
        f.spawn((
            button(fonts, "Cancel", Variant::Secondary),
            NodeCancel(pr.clone()),
            observe(on_cancel),
        ));
        if v.saving {
            f.spawn(disabled_button(fonts, "Saving…"));
        } else if v.blocked.is_some() {
            f.spawn(disabled_button(fonts, "Add to draft"));
        } else {
            f.spawn((
                button(fonts, "Add to draft", Variant::Primary),
                EditorSubmit(pr.clone()),
                observe(on_submit_button),
            ));
        }
    });
}

pub(crate) fn body_block(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    body: &NodeBody,
    inset: f32,
) {
    let said = match body {
        NodeBody::Loading => Some("Reading the code…".to_string()),
        NodeBody::Offline => Some("Not connected to clusiad: the code cannot be read.".to_string()),
        NodeBody::Unavailable(why) => Some(why.clone()),
        NodeBody::NoAnchor => Some(NO_ANCHOR.to_string()),
        NodeBody::Code(_) => None,
    };
    let frame = Node {
        flex_direction: FlexDirection::Column,
        margin: UiRect::horizontal(px(inset)),
        padding: UiRect::axes(px(0), px(10)),
        border_radius: BorderRadius::all(px(6)),
        overflow: Overflow::clip(),
        ..default()
    };
    if let Some(said) = said {
        p.spawn(panel(
            Node {
                padding: UiRect::axes(px(12), px(10)),
                ..frame
            },
            Swatch::Bg,
        ))
        .with_children(|b| {
            b.spawn(text(fonts, said, Type::MUTED));
        });
        return;
    }
    let NodeBody::Code(rows) = body else { return };
    let scrolled = Node {
        max_height: px(CODE_MAX_HEIGHT),
        flex_shrink: 0.0,
        overflow: Overflow::scroll_y(),
        ..frame
    };
    p.spawn((panel(scrolled, Swatch::Bg), ScrollArea, NodeCode))
        .with_children(|b| {
            for row in rows {
                b.spawn((
                    Node {
                        min_height: px(21),
                        flex_shrink: 0.0,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BackgroundColor::default(),
                    Fill(if row.changed {
                        Swatch::AddedBg
                    } else {
                        Swatch::Clear
                    }),
                ))
                .with_children(|r| {
                    r.spawn(Node {
                        width: px(40),
                        flex_shrink: 0.0,
                        justify_content: JustifyContent::FlexEnd,
                        padding: UiRect::right(px(12)),
                        ..default()
                    })
                    .with_children(|g| {
                        g.spawn(text(
                            fonts,
                            row.number.to_string(),
                            Type::MONO.size(12.0).ink(Swatch::Faint),
                        ));
                    });
                    r.spawn(code_line(fonts, 12.0, row.spans.clone()));
                });
            }
        });
}

fn on_cancel(activate: On<Activate>, buttons: Query<&NodeCancel>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(NodeCancel(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(pr) {
        close(&mut tab.ui);
    }
}

/// Shows the box's file in Diff, and its first line when the drawing is of the newest commit
/// (the lines of an older drawing may have moved).
fn on_open_diff(
    activate: On<Activate>,
    buttons: Query<&NodeOpenDiff>,
    mut tabs: ResMut<ReviewTabs>,
    diagrams: Res<Diagrams>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(NodeOpenDiff(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let Some(tab) = tabs.0.get_mut(pr) else {
        return;
    };
    let Some((number, key, node)) = tab.ui.diagram_node.clone() else {
        return;
    };
    let Some(version) = diagrams.0.get(pr).and_then(|m| m.loaded.get(&number)) else {
        return;
    };
    let Some(anchor) = anchor_of(version, &key, &node) else {
        return;
    };
    let at_head = tab
        .ready()
        .is_some_and(|r| r.view.review.head_sha == version.commit);
    let lines = at_head.then_some((anchor.start_line, anchor.end_line));
    if show_line(tab, &anchor.path, lines) {
        close(&mut tab.ui);
    } else {
        not_sent(&mut toasts, &time, NOT_IN_DIFF);
    }
}

/// Ends the dialog once its comment is saved (the editor is gone) or its editor was replaced.
fn close_when_done(modals: Query<&NodeModal>, mut tabs: ResMut<ReviewTabs>) {
    let done: Vec<PrRef> = modals
        .iter()
        .filter(|NodeModal(pr)| {
            tabs.0.get(pr).is_some_and(|t| {
                t.ui.modal == Some(Modal::DiagramNode)
                    && !matches!(
                        t.ui.editor.as_ref().map(|e| &e.target),
                        Some(EditTarget::DiagramNode { .. })
                    )
            })
        })
        .map(|NodeModal(pr)| pr.clone())
        .collect();
    for pr in done {
        if let Some(tab) = tabs.0.get_mut(&pr) {
            close(&mut tab.ui);
        }
    }
}

fn escape_closes(keys: Res<ButtonInput<KeyCode>>, nav: Res<Nav>, mut tabs: ResMut<ReviewTabs>) {
    if !escape_pressed(&keys) {
        return;
    }
    let Screen::Review(pr) = &nav.screen else {
        return;
    };
    // Read first: every Esc on any screen would otherwise mark the tabs changed.
    let closes = tabs.0.get(pr).is_some_and(|tab| {
        tab.ui.modal == Some(Modal::DiagramNode)
            && tab
                .ui
                .editor
                .as_ref()
                .is_none_or(|e| e.ticket.is_none() && e.text.trim().is_empty())
    });
    if closes && let Some(tab) = tabs.0.get_mut(pr) {
        close(&mut tab.ui);
    }
}

/// A drawing, version 2 of the commit `commit`, with the boxes the tests click: `refresh`,
/// whose code is `anchor`, and `plain`, which has none.
#[cfg(test)]
pub(crate) fn test_version(commit: &str, anchor: Option<(&str, u32, u32)>) -> DiagramVersion {
    use clusia_core::diagram::{
        DiagramEntry, DiagramKind, DiagramSpec, PlanEntry, Shape, VersionPhase,
    };
    let node = |id: &str, label: &str, anchor: Option<CodeAnchor>| BoxNode {
        id: id.into(),
        label: label.into(),
        sub: None,
        shape: Shape::Box,
        rows: Vec::new(),
        divider_after: None,
        anchor,
        change: Change::Changed,
        note: None,
        history: None,
    };
    let spec = DiagramSpec {
        key: "flow".into(),
        kind: DiagramKind::Flow,
        title: "Refresh flow".into(),
        caption: "How a token is refreshed".into(),
        nodes: vec![
            node(
                "refresh",
                "TokenStore::refresh",
                anchor.map(|(path, start_line, end_line)| CodeAnchor {
                    path: path.into(),
                    start_line,
                    end_line,
                }),
            ),
            node("plain", "Docs", None),
        ],
        edges: Vec::new(),
        groups: Vec::new(),
        messages: Vec::new(),
    };
    DiagramVersion {
        number: 2,
        commit: commit.into(),
        harness: "Claude Code".into(),
        drawn_at: 1_790_000_000,
        phase: VersionPhase::Done,
        entries: vec![DiagramEntry {
            plan: PlanEntry {
                key: "flow".into(),
                kind: DiagramKind::Flow,
                title: "Refresh flow".into(),
                purpose: "The refresh path".into(),
            },
            state: EntryState::Ready {
                spec,
                notes: Vec::new(),
            },
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{DiagramsTell, Tell};
    use crate::fixture;
    use crate::review_state::ReviewSection;
    use crate::screens::review::editor::EditorArea;
    use crate::testing::{self, NOW};
    use bevy::text::EditableText;
    use clusia_core::keywords::{KeywordMap, place_node};
    use clusia_protocol::CodeLine;

    const REFRESH: &str = "src/auth/refresh.rs";

    fn demo_ready() -> Ready {
        let (view, news) = fixture::demo_review(NOW);
        Ready {
            view,
            news,
            cached_at: None,
        }
    }

    fn head() -> String {
        demo_ready().view.review.head_sha
    }

    fn patch_of(ready: &Ready, path: &str) -> Option<String> {
        ready
            .view
            .diff
            .iter()
            .find(|f| f.path == path)
            .and_then(|f| f.patch.clone())
    }

    fn code_at(commit: &str, at_head: bool) -> DiagramCode {
        let ready = demo_ready();
        fixture::demo_diagram_code(
            REFRESH,
            commit,
            patch_of(&ready, REFRESH).as_deref(),
            (38, 48),
            at_head,
        )
    }

    fn ready_tab() -> Tab {
        Tab {
            phase: Phase::Ready(Box::new(demo_ready())),
            ui: TabUi::default(),
        }
    }

    fn target() -> (u32, String, String) {
        (2, "flow".into(), "refresh".into())
    }

    #[test]
    fn a_box_with_code_shows_where_it_is_and_where_the_comment_lands() {
        let tab = ready_tab();
        let version = test_version(&head(), Some((REFRESH, 38, 48)));
        let code = code_at(&head(), true);
        let v = node_view(&tab, &target(), &version, Some(&code), true, None).expect("a view");
        assert_eq!(v.label, "TokenStore::refresh");
        assert_eq!(v.place.as_deref(), Some("src/auth/refresh.rs · 38–48"));
        assert_eq!(
            v.hint.as_deref(),
            Some("Lands in the draft as a comment on lines 38–48, the code this box stands for.")
        );
        assert_eq!(v.jump, DiffJump::Ready);
        let NodeBody::Code(rows) = &v.body else {
            panic!("code: {:?}", v.body)
        };
        let numbers: Vec<u32> = rows.iter().map(|r| r.number).collect();
        assert_eq!(numbers, (38..=48).collect::<Vec<_>>());
        let changed: Vec<u32> = rows
            .iter()
            .filter(|r| r.changed)
            .map(|r| r.number)
            .collect();
        assert_eq!(changed, [39, 40, 41, 44]);
        let text: String = rows[0].spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            text,
            "    pub async fn refresh(&self) -> Result<Token, AuthError> {"
        );
        assert!(!v.saving);
        assert_eq!(v.error, None);
    }

    #[test]
    fn an_older_drawing_says_which_commit_its_code_is_from() {
        let tab = ready_tab();
        let version = test_version(
            "0123456789abcdef0123456789abcdef01234567",
            Some((REFRESH, 38, 48)),
        );
        let v = node_view(&tab, &target(), &version, None, true, None).expect("a view");
        let hint = v.hint.expect("a hint");
        assert!(
            hint.contains("lines 38–48 of the code at 0123456"),
            "{hint}"
        );
        assert!(hint.contains("review body otherwise"), "{hint}");
    }

    #[test]
    fn the_body_says_what_is_missing_instead_of_code() {
        let tab = ready_tab();
        let version = test_version(&head(), Some((REFRESH, 38, 48)));
        let body = |code: Option<&DiagramCode>, live: bool| {
            node_view(&tab, &target(), &version, code, live, None)
                .expect("a view")
                .body
        };
        assert_eq!(body(None, true), NodeBody::Loading);
        assert_eq!(body(None, false), NodeBody::Offline);
        let mut gone = code_at(&head(), true);
        gone.lines.clear();
        gone.unavailable = Some("The code at 9be1d07 is no longer available.\u{1b}[2J".into());
        assert_eq!(
            body(Some(&gone), true),
            NodeBody::Unavailable("The code at 9be1d07 is no longer available.\\u{1b}[2J".into())
        );
    }

    #[test]
    fn code_from_a_repository_is_shown_as_plain_safe_text() {
        let mut code = code_at(&head(), true);
        code.lines = vec![CodeLine {
            number: 38,
            text: "let a = 1; \u{1b}[31m\u{202e}x\t// y".into(),
            changed: false,
        }];
        let rows = code_rows(&code);
        let shown: String = rows[0].spans.iter().map(|s| s.text.as_str()).collect();
        assert!(
            !shown.contains('\u{1b}') && !shown.contains('\u{202e}'),
            "{shown:?}"
        );
        assert!(shown.contains("\\u{1b}[31m"), "{shown:?}");
        assert!(
            shown.contains("    // y"),
            "a tab is four spaces: {shown:?}"
        );
    }

    #[test]
    fn a_box_without_code_has_nothing_to_open_and_says_so() {
        let tab = ready_tab();
        let version = test_version(&head(), Some((REFRESH, 38, 48)));
        let target = (2, "flow".to_string(), "plain".to_string());
        let v = node_view(&tab, &target, &version, None, true, None).expect("a view");
        assert_eq!(v.body, NodeBody::NoAnchor);
        assert_eq!((v.place, v.hint, v.jump), (None, None, DiffJump::Hidden));
        assert!(
            node_view(
                &tab,
                &(2, "flow".into(), "ghost".into()),
                &version,
                None,
                true,
                None
            )
            .is_none()
        );
    }

    #[test]
    fn a_file_outside_the_diff_cannot_be_opened_in_it() {
        let tab = ready_tab();
        let version = test_version(&head(), Some(("src/missing.rs", 3, 9)));
        let v = node_view(&tab, &target(), &version, None, true, None).expect("a view");
        assert_eq!(v.jump, DiffJump::NotInDiff);
    }

    #[test]
    fn the_editor_of_this_box_gives_the_error_and_the_saving_state() {
        let mut tab = ready_tab();
        let version = test_version(&head(), Some((REFRESH, 38, 48)));
        let editor = |number: u32, error: Option<&str>, ticket: Option<u64>| Editor {
            target: EditTarget::DiagramNode {
                number,
                key: "flow".into(),
                node: "refresh".into(),
            },
            text: "x".into(),
            error: error.map(String::from),
            ticket,
            mode: ComposerMode::Write,
        };
        tab.ui.editor = Some(editor(2, Some("No"), Some(7)));
        let v = node_view(&tab, &target(), &version, None, true, None).expect("a view");
        assert_eq!((v.error.as_deref(), v.saving), (Some("No"), true));
        tab.ui.editor = Some(editor(1, Some("Other box"), Some(8)));
        let v = node_view(&tab, &target(), &version, None, true, None).expect("a view");
        assert_eq!((v.error, v.saving), (None, false));
    }

    #[test]
    fn the_demo_code_starts_on_the_first_line_of_the_box_and_tints_only_its_own_lines() {
        let ready = demo_ready();
        let code = fixture::demo_diagram_code(
            REFRESH,
            &head(),
            patch_of(&ready, REFRESH).as_deref(),
            (40, 41),
            true,
        );
        let numbers: Vec<u32> = code.lines.iter().map(|l| l.number).collect();
        assert_eq!(numbers, [40, 41, 42, 43], "two lines of context after");
        let changed: Vec<u32> = code
            .lines
            .iter()
            .filter(|l| l.changed)
            .map(|l| l.number)
            .collect();
        assert_eq!(changed, [40, 41], "the added line 44 is context only");
        assert_eq!(code.start_line, 40);
    }

    #[test]
    fn the_demo_code_is_the_new_side_of_the_patch() {
        let code = code_at("9be1d07c3a2f4e5d6c7b8a9f0e1d2c3b4a5f6e7d", true);
        assert_eq!((code.path.as_str(), code.start_line), (REFRESH, 38));
        assert_eq!(code.unavailable, None);
        let numbers: Vec<u32> = code.lines.iter().map(|l| l.number).collect();
        assert_eq!(numbers, (38..=48).collect::<Vec<_>>());
        let changed: Vec<u32> = code
            .lines
            .iter()
            .filter(|l| l.changed)
            .map(|l| l.number)
            .collect();
        assert_eq!(changed, [39, 40, 41, 44]);
        assert_eq!(
            code.lines[1].text,
            "        let token = self.load().await?;"
        );
        let older = code_at("9be1d07c3a2f4e5d6c7b8a9f0e1d2c3b4a5f6e7d", false);
        assert!(
            older.lines.iter().all(|l| !l.changed),
            "no line is changed in an older version"
        );
        let gone = fixture::demo_diagram_code(REFRESH, "9be1d07c3a", None, (38, 48), true);
        assert_eq!(
            gone.unavailable.as_deref(),
            Some("The code at 9be1d07 is no longer available.")
        );
        assert!(gone.lines.is_empty());
    }

    fn comment(anchor: Option<CodeAnchor>, body: &str) -> clusia_core::DraftItem {
        let ready = demo_ready();
        let mut review = ready.view.review.clone();
        let patch = anchor.as_ref().and_then(|a| patch_of(&ready, &a.path));
        diagram_comment(&mut review, anchor.as_ref(), patch.as_deref(), body, NOW).expect("added");
        review.draft.items.last().cloned().expect("an item")
    }

    fn anchor(start_line: u32, end_line: u32) -> CodeAnchor {
        CodeAnchor {
            path: REFRESH.into(),
            start_line,
            end_line,
        }
    }

    #[test]
    fn a_comment_on_lines_in_the_change_is_a_line_comment_from_a_diagram() {
        let item = comment(Some(anchor(38, 48)), "Can the lock end sooner?");
        assert_eq!(
            (item.kind, item.origin),
            (DraftKind::LineComment, Origin::Diagram)
        );
        let a = item.anchor.expect("an anchor");
        assert_eq!(
            (a.path.as_str(), a.start_line, a.line, a.side),
            (REFRESH, Some(38), 48, Side::Right)
        );
        assert_eq!(a.commit, head());
        let one = comment(Some(anchor(40, 40)), "Why one minute?");
        assert_eq!(one.anchor.map(|a| (a.start_line, a.line)), Some((None, 40)));
    }

    #[test]
    fn a_comment_on_lines_outside_the_change_names_them_in_the_review_body() {
        let item = comment(Some(anchor(30, 33)), "Why?");
        assert_eq!(
            (item.kind, item.origin),
            (DraftKind::General, Origin::Diagram)
        );
        assert_eq!(item.anchor, None);
        assert_eq!(item.body, "src/auth/refresh.rs:30-33: Why?");
        assert_eq!(
            comment(Some(anchor(30, 30)), "Why?").body,
            "src/auth/refresh.rs:30: Why?"
        );
        let none = comment(None, "A general thought");
        assert_eq!(
            (none.kind, none.body.as_str()),
            (DraftKind::General, "A general thought")
        );
        assert_eq!(none.origin, Origin::Diagram);
    }

    // --- the dialog in the app --------------------------------------------------------------

    fn put_model(app: &mut App, pr: &PrRef, version: DiagramVersion) {
        let mut diagrams = app.world_mut().resource_mut::<Diagrams>();
        let model = diagrams.0.entry(pr.clone()).or_default();
        model.asked = true;
        model.versions = vec![version.info()];
        model.head = version.commit.clone();
        model.loaded.insert(version.number, version);
    }

    fn put_code(app: &mut App, pr: &PrRef, node: &str, code: DiagramCode) {
        let mut diagrams = app.world_mut().resource_mut::<Diagrams>();
        let model = diagrams.0.get_mut(pr).expect("a model");
        model.code.insert((2, "flow".into(), node.into()), code);
    }

    fn open_box(app: &mut App, version: DiagramVersion, node: &str) -> PrRef {
        let pr = testing::open_ready(app, false);
        put_model(app, &pr, version);
        edit_ui(app, &pr, |ui| {
            ui.section = ReviewSection::Diagrams;
            ui.modal = Some(Modal::DiagramNode);
            ui.diagram_node = Some((2, "flow".into(), node.into()));
        });
        testing::settle(app);
        pr
    }

    fn edit_ui(app: &mut App, pr: &PrRef, edit: impl FnOnce(&mut TabUi)) {
        let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
        edit(&mut tabs.0.get_mut(pr).expect("a tab").ui);
    }

    fn reads(app: &mut App) -> Vec<Ask> {
        testing::recorded(app)
            .into_iter()
            .filter(|a| matches!(a, Ask::ReadDiagramCode { .. }))
            .collect()
    }

    fn adds(app: &mut App) -> Vec<Ask> {
        testing::recorded(app)
            .into_iter()
            .filter(|a| matches!(a, Ask::AddDiagramComment { .. }))
            .collect()
    }

    fn app_with_box() -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_box(
            &mut app,
            test_version(&head(), Some((REFRESH, 38, 48))),
            "refresh",
        );
        (app, pr)
    }

    #[test]
    fn long_code_scrolls_inside_a_capped_block_so_the_footer_stays_in_an_800_px_window() {
        let (mut app, pr) = app_with_box();
        let long = DiagramCode {
            path: REFRESH.into(),
            commit: head(),
            start_line: 1,
            lines: (1..=200)
                .map(|number| CodeLine {
                    number,
                    text: format!("let line_{number} = {number};"),
                    changed: false,
                })
                .collect(),
            unavailable: None,
        };
        put_code(&mut app, &pr, "refresh", long);
        testing::settle(&mut app);
        let mut q = app
            .world_mut()
            .query_filtered::<&Node, (With<NodeCode>, With<ScrollArea>)>();
        let frames: Vec<Node> = q.iter(app.world()).cloned().collect();
        assert_eq!(frames.len(), 1, "one scrolling code block");
        assert_eq!(frames[0].max_height, px(CODE_MAX_HEIGHT));
        assert_eq!(frames[0].overflow, Overflow::scroll_y());
        // The parts around the code at their tallest (title, comment field with its error and
        // hint, footer) plus the capped code fit the card's 90 % of an 800 px window.
        let around = 64.0 + 32.0 + 196.0 + 62.0;
        assert!(around + CODE_MAX_HEIGHT <= 800.0 * 0.9);
        assert!(testing::shows(&mut app, "Add to draft"));
    }

    #[test]
    fn opening_asks_for_the_code_once_and_then_shows_it() {
        let (mut app, pr) = app_with_box();
        assert_eq!(
            reads(&mut app),
            [Ask::ReadDiagramCode {
                pr: pr.clone(),
                number: 2,
                key: "flow".into(),
                node: "refresh".into(),
            }]
        );
        testing::settle(&mut app);
        assert!(reads(&mut app).is_empty(), "asked once");
        assert!(testing::shows(&mut app, "Reading the code…"));
        put_code(&mut app, &pr, "refresh", code_at(&head(), true));
        testing::settle(&mut app);
        for needle in [
            "TokenStore::refresh",
            "src/auth/refresh.rs · 38–48",
            "Open in diff",
            "Comment",
            "Cancel",
            "Add to draft",
            "Lands in the draft as a comment on lines 38–48, the code this box stands for.",
            "44",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert!(!testing::shows(&mut app, "Reading the code…"));
        assert_eq!(testing::count::<NodeModal>(&mut app), 1);
        let editor = testing::tab(&app, &pr)
            .ui
            .editor
            .expect("the comment editor");
        assert_eq!(
            editor.target,
            EditTarget::DiagramNode {
                number: 2,
                key: "flow".into(),
                node: "refresh".into()
            }
        );
    }

    #[test]
    fn add_to_draft_sends_the_comment_and_saved_closes_the_dialog() {
        let (mut app, pr) = app_with_box();
        put_code(&mut app, &pr, "refresh", code_at(&head(), true));
        testing::settle(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |a| a.0 == pr);
        testing::type_into(&mut app, area, "Can the critical section end sooner?");
        let submit = testing::find::<EditorSubmit>(&mut app, |s| s.0 == pr);
        testing::activate(&mut app, submit);
        assert_eq!(
            adds(&mut app),
            [Ask::AddDiagramComment {
                pr: pr.clone(),
                number: 2,
                key: "flow".into(),
                node: "refresh".into(),
                body: "Can the critical section end sooner?".into(),
                ticket: 1,
            }]
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Saving…"));
        testing::tell(
            &mut app,
            crate::bridge::Tell::Saved {
                pr: pr.clone(),
                ticket: 1,
            },
        );
        testing::settle(&mut app);
        let tab = testing::tab(&app, &pr);
        assert_eq!(
            (tab.ui.modal, tab.ui.diagram_node, tab.ui.editor),
            (None, None, None)
        );
        assert_eq!(testing::count::<NodeModal>(&mut app), 0);
    }

    #[test]
    fn a_refusal_keeps_the_dialog_and_the_text() {
        let (mut app, pr) = app_with_box();
        put_code(&mut app, &pr, "refresh", code_at(&head(), true));
        testing::settle(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |a| a.0 == pr);
        testing::type_into(&mut app, area, "Why?");
        let submit = testing::find::<EditorSubmit>(&mut app, |s| s.0 == pr);
        testing::activate(&mut app, submit);
        adds(&mut app);
        testing::tell(
            &mut app,
            crate::bridge::Tell::Refused {
                pr: pr.clone(),
                ticket: 1,
                message: "This review is not open".into(),
            },
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "This review is not open"));
        assert!(
            testing::shows(&mut app, "Add to draft"),
            "it can be sent again"
        );
        let tab = testing::tab(&app, &pr);
        assert_eq!(tab.ui.modal, Some(Modal::DiagramNode));
        let editor = tab.ui.editor.expect("the editor stays");
        assert_eq!((editor.text.as_str(), editor.ticket), ("Why?", None));
        let area = testing::find::<EditorArea>(&mut app, |a| a.0 == pr);
        let kept = app
            .world()
            .get::<EditableText>(area)
            .expect("a text")
            .value()
            .to_string();
        assert_eq!(kept, "Why?", "the rebuilt field starts with what was typed");
    }

    #[test]
    fn cancel_and_escape_close_the_dialog_and_drop_the_comment() {
        let (mut app, pr) = app_with_box();
        let cancel = testing::find::<NodeCancel>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, cancel);
        let tab = testing::tab(&app, &pr);
        assert_eq!(
            (tab.ui.modal, tab.ui.diagram_node, tab.ui.editor),
            (None, None, None)
        );
        testing::settle(&mut app);
        assert_eq!(testing::count::<NodeModal>(&mut app), 0);

        let (mut app, pr) = app_with_box();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset_all();
        testing::settle(&mut app);
        let tab = testing::tab(&app, &pr);
        assert_eq!((tab.ui.modal, tab.ui.editor), (None, None));
        assert_eq!(testing::count::<NodeModal>(&mut app), 0);
    }

    #[test]
    fn escape_keeps_the_dialog_while_there_is_text_in_the_comment() {
        let (mut app, pr) = app_with_box();
        put_code(&mut app, &pr, "refresh", code_at(&head(), true));
        testing::settle(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |a| a.0 == pr);
        testing::type_into(&mut app, area, "Half a thought");
        testing::settle(&mut app);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();
        let tab = testing::tab(&app, &pr);
        assert_eq!(tab.ui.modal, Some(Modal::DiagramNode));
        assert_eq!(
            tab.ui.editor.map(|e| e.text),
            Some("Half a thought".to_string())
        );
    }

    #[test]
    fn escape_waits_while_the_comment_is_being_saved() {
        let (mut app, pr) = app_with_box();
        put_code(&mut app, &pr, "refresh", code_at(&head(), true));
        testing::settle(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |a| a.0 == pr);
        testing::type_into(&mut app, area, "Why?");
        let submit = testing::find::<EditorSubmit>(&mut app, |s| s.0 == pr);
        testing::activate(&mut app, submit);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();
        assert_eq!(testing::tab(&app, &pr).ui.modal, Some(Modal::DiagramNode));
    }

    #[test]
    fn a_box_without_code_says_so_asks_for_nothing_and_still_takes_a_comment() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_box(
            &mut app,
            test_version(&head(), Some((REFRESH, 38, 48))),
            "plain",
        );
        assert!(reads(&mut app).is_empty());
        assert!(testing::shows(&mut app, NO_ANCHOR));
        assert!(testing::shows(&mut app, "Add to draft"));
        assert!(!testing::shows(&mut app, "Open in diff"));
        assert_eq!(testing::count::<NodeOpenDiff>(&mut app), 0);
        assert_eq!(testing::count::<NodeModal>(&mut app), 1);
        assert!(testing::tab(&app, &pr).ui.editor.is_some());
    }

    #[test]
    fn an_unsent_comment_is_not_replaced_by_the_dialog() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        put_model(
            &mut app,
            &pr,
            test_version(&head(), Some((REFRESH, 38, 48))),
        );
        let half = Editor {
            target: EditTarget::General,
            text: "half written".into(),
            error: None,
            ticket: None,
            mode: ComposerMode::Write,
        };
        edit_ui(&mut app, &pr, |ui| {
            ui.editor = Some(half.clone());
            ui.modal = Some(Modal::DiagramNode);
            ui.diagram_node = Some(target());
        });
        testing::settle(&mut app);
        let tab = testing::tab(&app, &pr);
        assert_eq!((tab.ui.modal, tab.ui.diagram_node), (None, None));
        assert_eq!(tab.ui.editor, Some(half));
        assert_eq!(testing::count::<NodeModal>(&mut app), 0);
        assert!(app.world().resource::<Toasts>().0.iter().any(|t| {
            t.text
                .contains("Finish or cancel the comment you are writing first")
        }));
    }

    #[test]
    fn open_in_diff_goes_to_the_file_and_its_first_line_at_the_head() {
        let (mut app, pr) = app_with_box();
        let jump = testing::find::<NodeOpenDiff>(&mut app, |j| j.0 == pr);
        testing::activate(&mut app, jump);
        let tab = testing::tab(&app, &pr);
        assert_eq!(tab.ui.section, ReviewSection::Diff);
        assert_eq!(tab.ui.file.as_deref(), Some(REFRESH));
        assert_eq!(tab.ui.focus_line, Some((Side::Right, 38)));
        assert_eq!((tab.ui.modal, tab.ui.editor), (None, None));
    }

    #[test]
    fn open_in_diff_from_an_older_drawing_shows_the_file_without_a_line() {
        let mut app = testing::app(fixture::demo(NOW));
        let old = "0123456789abcdef0123456789abcdef01234567";
        let pr = open_box(
            &mut app,
            test_version(old, Some((REFRESH, 38, 48))),
            "refresh",
        );
        let jump = testing::find::<NodeOpenDiff>(&mut app, |j| j.0 == pr);
        testing::activate(&mut app, jump);
        let tab = testing::tab(&app, &pr);
        assert_eq!(tab.ui.file.as_deref(), Some(REFRESH));
        assert_eq!(tab.ui.focus_line, None);
        assert_eq!(tab.ui.modal, None);
    }

    #[test]
    fn a_file_outside_the_diff_has_a_disabled_open_in_diff() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_box(
            &mut app,
            test_version(&head(), Some(("src/missing.rs", 3, 9))),
            "refresh",
        );
        assert!(testing::shows(&mut app, "Open in diff"));
        assert_eq!(testing::count::<NodeOpenDiff>(&mut app), 0);
        assert_eq!(testing::tab(&app, &pr).ui.modal, Some(Modal::DiagramNode));
    }

    #[test]
    fn without_a_connection_the_code_is_not_asked_and_nothing_can_be_added() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        app.world_mut().resource_mut::<Model>().connection = Connection::Lost("gone".into());
        put_model(
            &mut app,
            &pr,
            test_version(&head(), Some((REFRESH, 38, 48))),
        );
        edit_ui(&mut app, &pr, |ui| {
            ui.modal = Some(Modal::DiagramNode);
            ui.diagram_node = Some(target());
        });
        testing::settle(&mut app);
        assert!(reads(&mut app).is_empty());
        assert!(testing::shows(
            &mut app,
            "Not connected to clusiad: the code cannot be read."
        ));
        assert!(testing::shows(
            &mut app,
            "Not connected to clusiad — not sent"
        ));
        assert_eq!(testing::count::<EditorSubmit>(&mut app), 0);
        app.world_mut().resource_mut::<Model>().connection = Connection::Live;
        testing::settle(&mut app);
        assert_eq!(
            reads(&mut app).len(),
            1,
            "asked as soon as there is a connection"
        );
    }

    #[test]
    fn the_code_is_asked_for_again_once_a_redraw_has_cleared_it() {
        let (mut app, pr) = app_with_box();
        assert_eq!(reads(&mut app).len(), 1);
        put_code(&mut app, &pr, "refresh", code_at(&head(), true));
        testing::settle(&mut app);
        assert!(reads(&mut app).is_empty());
        app.world_mut()
            .resource_mut::<Diagrams>()
            .0
            .get_mut(&pr)
            .expect("a model")
            .code
            .clear();
        testing::settle(&mut app);
        assert_eq!(reads(&mut app).len(), 1, "the dialog does not wait forever");
    }

    #[test]
    fn the_dialog_closes_when_a_redraw_takes_its_box_away() {
        let (mut app, pr) = app_with_box();
        assert_eq!(testing::count::<NodeModal>(&mut app), 1);
        {
            let mut diagrams = app.world_mut().resource_mut::<Diagrams>();
            let model = diagrams.0.get_mut(&pr).expect("a model");
            let entry = model.loaded.get_mut(&2).unwrap().entries.iter_mut();
            for e in entry {
                if let EntryState::Ready { spec, .. } = &mut e.state {
                    spec.nodes.retain(|n| n.id != "refresh");
                }
            }
        }
        testing::settle(&mut app);
        let tab = testing::tab(&app, &pr);
        assert_eq!((tab.ui.modal, tab.ui.diagram_node), (None, None));
        assert_eq!(testing::count::<NodeModal>(&mut app), 0);
    }

    #[test]
    fn escape_without_the_dialog_leaves_the_tabs_unchanged() {
        let pr = fixture::demo_pr();
        let mut tabs = ReviewTabs::default();
        tabs.0.insert(pr.clone(), ready_tab());
        let mut keys = ButtonInput::<KeyCode>::default();
        keys.press(KeyCode::Escape);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(Nav {
                screen: Screen::Review(pr.clone()),
                reviews: vec![pr],
                section: crate::nav::Section::Appearance,
                focus: None,
                requests: 0,
            })
            .insert_resource(tabs)
            .insert_resource(keys)
            .init_resource::<TabsChanged>()
            .add_systems(
                Update,
                (
                    escape_closes,
                    |tabs: Res<ReviewTabs>, mut seen: ResMut<TabsChanged>| {
                        seen.0 = tabs.is_changed();
                    },
                )
                    .chain(),
            );
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .release(KeyCode::Escape);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();
        assert!(
            !app.world().resource::<TabsChanged>().0,
            "Esc with no dialog open must not mark the tabs changed"
        );
    }

    #[derive(Resource, Default)]
    struct TabsChanged(bool);

    #[test]
    fn a_frame_without_a_click_leaves_the_tabs_unchanged() {
        let pr = fixture::demo_pr();
        let mut tabs = ReviewTabs::default();
        tabs.0.insert(pr.clone(), ready_tab());
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(Nav {
                screen: Screen::Review(pr.clone()),
                reviews: vec![pr],
                section: crate::nav::Section::Appearance,
                focus: None,
                requests: 0,
            })
            .insert_resource(tabs)
            .insert_resource(Toasts::default())
            .init_resource::<TabsChanged>()
            .add_systems(
                Update,
                (
                    open_node,
                    |tabs: Res<ReviewTabs>, mut seen: ResMut<TabsChanged>| {
                        seen.0 = tabs.is_changed();
                    },
                )
                    .chain(),
            );
        app.update();
        app.update();
        assert!(
            !app.world().resource::<TabsChanged>().0,
            "looking for a request must not mark the tabs changed"
        );
    }

    #[test]
    fn clicking_a_box_on_the_canvas_opens_the_dialog_and_asks_for_its_code() {
        use crate::screens::review::diagrams::canvas::DiagramBox;
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::open_diagrams(&mut app, &pr);
        testing::set_section(&mut app, &pr, ReviewSection::Diagrams);
        let refresh = testing::find::<DiagramBox>(&mut app, |b| b.node == "refresh");
        assert_eq!(testing::count::<NodeModal>(&mut app), 0);
        let tabs_changed = app
            .world_mut()
            .register_system(|tabs: Res<ReviewTabs>| tabs.is_changed());
        testing::settle(&mut app);
        app.world_mut().run_system(tabs_changed).expect("runs");
        app.update();
        assert!(
            !app.world_mut().run_system(tabs_changed).expect("runs"),
            "a settled frame with no click leaves ReviewTabs unchanged"
        );
        testing::activate(&mut app, refresh);
        testing::settle(&mut app);
        assert_eq!(testing::tab(&app, &pr).ui.modal, Some(Modal::DiagramNode));
        assert_eq!(testing::count::<NodeModal>(&mut app), 1);
        assert_eq!(reads(&mut app).len(), 1, "the box's code was asked for");
    }

    // --- the dialog on a place of the keyword map ---------------------------------------------

    fn open_place(app: &mut App, map: KeywordMap, place: u32) -> PrRef {
        let pr = testing::open_keywords_with(app, map, |_| {});
        edit_ui(app, &pr, |ui| {
            ui.modal = Some(Modal::DiagramNode);
            ui.diagram_node = Some((2, "keywords".into(), place_node(place as usize)));
        });
        testing::settle(app);
        pr
    }

    fn put_place_code(app: &mut App, pr: &PrRef, node: &str, code: DiagramCode) {
        testing::tell(
            app,
            Tell::Diagrams(DiagramsTell::Code {
                pr: pr.clone(),
                number: 2,
                key: "keywords".into(),
                node: node.into(),
                code,
            }),
        );
        testing::settle(app);
    }

    fn place_code(path: &str, line: u32, commit: &str) -> DiagramCode {
        fixture::demo_place_code(path, commit, line)
    }

    #[test]
    fn a_place_resolves_to_its_line_and_a_box_still_resolves_to_its_anchor() {
        let map = fixture::keywords_map_at(REFRESH, 44, true);
        let version =
            fixture::with_keywords_map(vec![test_version(&head(), Some((REFRESH, 38, 48)))], map)
                .pop()
                .expect("a version");
        assert_eq!(
            anchor_of(&version, "keywords", "p0"),
            Some(CodeAnchor {
                path: REFRESH.into(),
                start_line: 44,
                end_line: 44
            })
        );
        assert_eq!(
            anchor_of(&version, "flow", "refresh").map(|a| (a.start_line, a.end_line)),
            Some((38, 48))
        );
        for bad in ["p1", "p", "q0", "p-1", "refresh"] {
            assert_eq!(anchor_of(&version, "keywords", bad), None, "{bad}");
            assert!(!node_exists(&version, "keywords", bad), "{bad}");
        }
        assert!(node_exists(&version, "keywords", "p0"));
        assert!(node_exists(&version, "flow", "refresh"));
        assert_eq!(anchor_of(&version, "gone", "p0"), None);
    }

    #[test]
    fn the_dialog_of_a_changed_place_in_the_diff_asks_for_its_code_and_lands_on_the_line() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_place(&mut app, fixture::keywords_map_at(REFRESH, 44, true), 0);
        assert_eq!(
            reads(&mut app),
            [Ask::ReadDiagramCode {
                pr: pr.clone(),
                number: 2,
                key: "keywords".into(),
                node: "p0".into(),
            }]
        );
        testing::settle(&mut app);
        assert!(reads(&mut app).is_empty(), "asked once");
        put_place_code(&mut app, &pr, "p0", place_code(REFRESH, 44, &head()));
        assert!(!testing::shows(&mut app, "Reading the code…"));
        for needle in [
            "token",
            "src/auth/refresh.rs · 44",
            "Everything · token",
            "Open in diff",
            "Add to draft",
            "Lands in the draft as a comment on line 44, the code this place stands for.",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
    }

    #[test]
    fn a_place_outside_the_diff_says_the_comment_goes_to_the_review_body() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_place(
            &mut app,
            fixture::keywords_map_at("src/session.rs", 9, false),
            0,
        );
        put_place_code(
            &mut app,
            &pr,
            "p0",
            place_code("src/session.rs", 9, &head()),
        );
        assert!(testing::shows(
            &mut app,
            "Not in the diff: the comment goes to the review body."
        ));
        assert!(testing::shows(&mut app, "src/session.rs · 9"));
        assert!(!testing::shows(
            &mut app,
            "Lands in the draft as a comment on line"
        ));
    }

    #[test]
    fn a_place_of_an_older_version_says_it_moves_and_a_force_push_says_the_code_is_gone() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_place(&mut app, fixture::keywords_map_at(REFRESH, 44, true), 0);
        let old = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
        {
            let mut diagrams = app.world_mut().resource_mut::<Diagrams>();
            diagrams
                .0
                .get_mut(&pr)
                .and_then(|m| m.loaded.get_mut(&2))
                .expect("version 2")
                .commit = old.into();
        }
        testing::settle(&mut app);
        assert!(testing::shows(
            &mut app,
            "Lands in the draft as a comment on line 44 of the code at a1b2c3d: it moves to the newest commit when that line is still in the change, and goes to the review body otherwise."
        ));
        let gone = DiagramCode {
            path: REFRESH.into(),
            commit: old.into(),
            start_line: 44,
            lines: Vec::new(),
            unavailable: Some("The code at a1b2c3d is no longer available.".into()),
        };
        put_place_code(&mut app, &pr, "p0", gone);
        assert!(testing::shows(
            &mut app,
            "The code at a1b2c3d is no longer available."
        ));
        assert!(
            testing::shows(&mut app, "Add to draft"),
            "the comment can still be written"
        );
    }

    #[test]
    fn a_comment_on_a_place_asks_the_daemon_with_the_place_node() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_place(&mut app, fixture::keywords_map_at(REFRESH, 44, true), 0);
        testing::settle(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |a| a.0 == pr);
        testing::type_into(&mut app, area, "Why one minute?");
        let submit = testing::find::<EditorSubmit>(&mut app, |s| s.0 == pr);
        testing::activate(&mut app, submit);
        assert_eq!(
            adds(&mut app),
            [Ask::AddDiagramComment {
                pr,
                number: 2,
                key: "keywords".into(),
                node: "p0".into(),
                body: "Why one minute?".into(),
                ticket: 1,
            }]
        );
    }

    #[test]
    fn the_demo_daemon_files_a_place_in_the_diff_on_its_line_and_the_others_in_the_body() {
        let on_line = fixture::with_keywords_map(
            vec![test_version(&head(), None)],
            fixture::keywords_map_at(REFRESH, 44, true),
        );
        let item = comment(anchor_of(&on_line[0], "keywords", "p0"), "Why?");
        assert_eq!(
            (item.kind, item.origin),
            (DraftKind::LineComment, Origin::Diagram)
        );
        assert_eq!(
            item.anchor.map(|a| (a.start_line, a.line)),
            Some((None, 44))
        );
        let elsewhere = fixture::with_keywords_map(
            vec![test_version(&head(), None)],
            fixture::keywords_map_at("src/session.rs", 9, false),
        );
        let item = comment(anchor_of(&elsewhere[0], "keywords", "p0"), "Why?");
        assert_eq!((item.kind, item.anchor), (DraftKind::General, None));
        assert_eq!(item.body, "src/session.rs:9: Why?");
    }

    #[test]
    fn the_demo_reads_a_place_the_demo_diff_does_not_hold() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_place(
            &mut app,
            fixture::keywords_map_at("src/session.rs", 9, false),
            0,
        );
        let diagrams = app.world().resource::<Diagrams>();
        let ready = testing::ready(&app, &pr);
        let code = demo_code_for(diagrams.0.get(&pr), Some(&ready), 2, "keywords", "p0");
        assert_eq!(code.unavailable, None);
        assert_eq!(
            (code.path.as_str(), code.start_line, code.lines.len()),
            ("src/session.rs", 8, 4)
        );
        let known = demo_code_for(diagrams.0.get(&pr), Some(&ready), 2, "keywords", "p7");
        assert!(known.unavailable.is_some(), "no such place");
    }
}
