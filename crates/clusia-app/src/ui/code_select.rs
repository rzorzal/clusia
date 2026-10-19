//! Selecting and copying code.
//!
//! Every selectable line is a `CodeLine` (a box holding the line's text and the layer that draws
//! its highlight). Lines that belong together share a `group` (one diff, one dialog, one code
//! block); a drag selects between two points of one group, and in a split view stays on the
//! column where it began. The selection lives in `CodeSelection`; Cmd-C copies its text.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::time::Duration;

use bevy::clipboard::Clipboard;
use bevy::input_focus::InputFocus;
use bevy::picking::events::{Drag, Pointer, Press};
use bevy::picking::pointer::PointerButton;
use bevy::prelude::*;
use bevy::text::{ComputedTextBlock, EditableText, TextLayoutInfo};
use bevy::ui::{ComputedNode, UiGlobalTransform};
use bevy::ui_widgets::observe;
use parley::editing::{Cursor, Selection};
use parley::layout::{Affinity, Cluster, ClusterSide};

use crate::theme::Swatch;
use crate::ui::kit::Fill;
use crate::ui::markdown::{CopiedText, copy_text};

/// A pointer that travels this far (in physical pixels) between press and release dragged; any
/// less is a click.
pub const DRAG_SLOP: f32 = 4.0;

/// Presses this close in time, on the same line and near each other, count as one multi-click.
pub const MULTI_CLICK: Duration = Duration::from_millis(500);

/// How far (in physical pixels) a second press may land from the first and still be a
/// multi-click.
const MULTI_CLICK_REACH: f32 = 6.0;

/// A line in the order of its group, and the column it sits in (0 unless the group has two
/// sides).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineKey {
    pub group: Entity,
    pub order: usize,
    pub column: u8,
}

/// A point in a group's text: a byte offset in the line at `order`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct At {
    pub order: usize,
    pub offset: usize,
}

/// A line's text, for working out what a selection holds.
#[derive(Debug, Clone, Copy)]
pub struct SelLine<'a> {
    pub order: usize,
    pub column: u8,
    pub text: &'a str,
}

/// Whether a pointer that moved by `distance` between press and now dragged rather than clicked.
pub fn is_drag(distance: Vec2) -> bool {
    distance.length() >= DRAG_SLOP
}

/// The last press, to tell a second and third press of a multi-click.
#[derive(Debug, Clone, Copy)]
pub struct LastPress {
    pub at: Duration,
    pub key: LineKey,
    pub position: Vec2,
    pub count: u8,
}

/// The count of a press at `now`: 1 for a first press, 2 and 3 for those that follow it closely
/// on the same line; a press after the third starts over.
pub fn click_count(last: Option<&LastPress>, now: Duration, key: LineKey, position: Vec2) -> u8 {
    match last {
        Some(last)
            if last.count < 3
                && last.key == key
                && now.saturating_sub(last.at) < MULTI_CLICK
                && last.position.distance(position) <= MULTI_CLICK_REACH =>
        {
            last.count + 1
        }
        _ => 1,
    }
}

/// The part of the line at `order` (of `len` bytes) that a selection from `from` to `to` covers,
/// in either direction. `None` when the line is outside it.
pub fn line_range(order: usize, len: usize, from: At, to: At) -> Option<Range<usize>> {
    let (start, end) = if from <= to { (from, to) } else { (to, from) };
    if order < start.order || order > end.order {
        return None;
    }
    let first = if order == start.order {
        start.offset
    } else {
        0
    };
    let last = if order == end.order { end.offset } else { len };
    Some(first.min(len)..last.min(len))
}

/// The text a selection from `from` to `to` holds among the lines of one `column`: the covered
/// part of each line, one per row.
pub fn selected_text(lines: &[SelLine], column: u8, from: At, to: At) -> String {
    let mut own: Vec<&SelLine> = lines.iter().filter(|l| l.column == column).collect();
    own.sort_by_key(|l| l.order);
    own.iter()
        .filter_map(|l| {
            let r = line_range(l.order, l.text.len(), from, to)?;
            let (a, b) = (
                floor_boundary(l.text, r.start),
                floor_boundary(l.text, r.end),
            );
            Some(&l.text[a..b])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The nearest character boundary of `text` at or before `at`.
pub fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The word around byte `offset` of `text`: a run of letters, digits and `_`, or of blanks, or
/// one other character.
pub fn word_range(text: &str, offset: usize) -> Range<usize> {
    #[derive(PartialEq)]
    enum Kind {
        Word,
        Blank,
        Other(char),
    }
    fn kind(c: char) -> Kind {
        if c.is_alphanumeric() || c == '_' {
            Kind::Word
        } else if c.is_whitespace() {
            Kind::Blank
        } else {
            Kind::Other(c)
        }
    }
    let offset = floor_boundary(text, offset);
    // At the end of the line the word is the one before the point.
    let Some(c) = text[offset..]
        .chars()
        .next()
        .or_else(|| text.chars().next_back())
    else {
        return 0..0;
    };
    let at = if offset == text.len() {
        text.len() - c.len_utf8()
    } else {
        offset
    };
    let here = kind(c);
    let before = text[..at]
        .char_indices()
        .rev()
        .take_while(|(_, c)| kind(*c) == here)
        .last()
        .map_or(at, |(i, _)| i);
    let after = text[at..]
        .char_indices()
        .find(|(_, c)| kind(*c) != here)
        .map_or(text.len(), |(i, _)| at + i);
    before..after
}

/// A line of selectable code: the `Text` it is on, the text a copy takes from it, and the layer
/// that draws its highlight.
#[derive(Component, Debug, Clone)]
pub struct CodeLine {
    pub key: LineKey,
    pub text: String,
    layer: Entity,
}

/// What a line's highlight layer shows: the selected bytes, and the box they were measured in.
#[derive(Component, Debug, Default, Clone, PartialEq)]
struct Highlight {
    range: Option<Range<usize>>,
    size: Vec2,
    origin: Vec2,
}

/// The current selection: a group, a column of it, and two points. Only one exists at a time.
#[derive(Resource, Debug, Default)]
pub struct CodeSelection {
    group: Option<Entity>,
    column: u8,
    anchor: At,
    focus: At,
    dragged: bool,
    multi: bool,
    pressed: bool,
    /// A press on code is waiting for the click that ends it.
    armed: bool,
    /// What the group's lines said when the selection was first seen; a change ends it.
    fingerprint: Option<u64>,
    last: Option<LastPress>,
}

/// What a click on a line that holds code means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickKind {
    /// It ended a drag or a multi-click: it selected, and is no click on the line.
    Selected,
    /// It follows a press on the code, which may be the first of a double click: act on it once
    /// no further press (the one at this time) has come.
    Pending(Option<Duration>),
    /// It follows no press on the code: act now.
    Plain,
}

impl CodeSelection {
    /// Selects from `anchor` to `focus` in `column` of `group`.
    pub fn select(&mut self, group: Entity, column: u8, anchor: At, focus: At) {
        self.group = Some(group);
        self.column = column;
        self.anchor = anchor;
        self.focus = focus;
        self.fingerprint = None;
    }

    /// The time of the latest press on code.
    pub fn last_press(&self) -> Option<Duration> {
        self.last.map(|l| l.at)
    }

    /// Whether nothing is selected.
    pub fn is_empty(&self) -> bool {
        self.group.is_none() || self.anchor == self.focus
    }

    /// Reads the click that ends a press, and consumes what the press left for it: a gesture
    /// that selected (it dragged, or was the second or third of a multi-click) is no click.
    pub fn take_click(&mut self) -> ClickKind {
        let armed = std::mem::take(&mut self.armed);
        if std::mem::take(&mut self.dragged) | std::mem::take(&mut self.multi) {
            ClickKind::Selected
        } else if armed {
            ClickKind::Pending(self.last_press())
        } else {
            ClickKind::Plain
        }
    }

    /// The text of the selection, one row per line.
    pub fn text<'a>(&self, lines: impl IntoIterator<Item = &'a CodeLine>) -> String {
        let Some(group) = self.group else {
            return String::new();
        };
        let lines: Vec<SelLine> = lines
            .into_iter()
            .filter(|l| l.key.group == group)
            .map(|l| SelLine {
                order: l.key.order,
                column: l.key.column,
                text: &l.text,
            })
            .collect();
        selected_text(&lines, self.column, self.anchor, self.focus)
    }
}

/// Spawns a line of selectable code in `p`: the highlight layer, then `text`, which draws over it.
/// `copy` is what copying the line takes: its code alone. The text must lay out as one `Text`
/// whose bytes are `copy`'s.
pub fn selectable(p: &mut ChildSpawnerCommands, key: LineKey, copy: String, text: impl Bundle) {
    let layer = p
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                ..default()
            },
            Pickable::IGNORE,
            Highlight::default(),
        ))
        .id();
    p.spawn((
        text,
        CodeLine {
            key,
            text: copy,
            layer,
        },
        observe(on_press),
        observe(on_drag),
    ));
}

pub struct CodeSelectPlugin;

impl Plugin for CodeSelectPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CodeSelection>()
            .add_systems(Update, (forget, drop_stale, copy_selection, paint).chain());
    }
}

/// Where `window` (a pointer position in logical window pixels) falls in the box of `node`,
/// in physical pixels from its top-left corner: the unit of the text layout.
fn local(node: &ComputedNode, transform: &UiGlobalTransform, window: Vec2) -> Vec2 {
    let physical = window / node.inverse_scale_factor;
    let inside = transform
        .try_inverse()
        .map_or(Vec2::ZERO, |inverse| inverse.transform_point2(physical));
    inside + node.size / 2.0
}

/// The byte of a line's text the point `at` (from the top-left corner of its box) is nearest to.
/// Above the box that is the start; below it, the end.
fn offset_in(block: &ComputedTextBlock, node: &ComputedNode, at: Vec2, len: usize) -> usize {
    if at.y < 0.0 {
        return 0;
    }
    if at.y >= node.size.y {
        return len;
    }
    let offset = match Cluster::from_point(block.buffer(), at.x, at.y) {
        Some((cluster, ClusterSide::Left)) => cluster.text_range().start,
        Some((cluster, ClusterSide::Right)) => cluster.text_range().end,
        None if at.x < node.size.x / 2.0 => 0,
        None => len,
    };
    offset.min(len)
}

type Measured<'w> = (
    &'w CodeLine,
    &'w ComputedTextBlock,
    &'w ComputedNode,
    &'w UiGlobalTransform,
);

fn on_press(
    press: On<Pointer<Press>>,
    lines: Query<Measured>,
    time: Res<Time<Real>>,
    mut selection: ResMut<CodeSelection>,
) {
    if press.button != PointerButton::Primary {
        return;
    }
    let Ok((line, block, node, transform)) = lines.get(press.event_target()) else {
        return;
    };
    let position = press.pointer_location.position;
    let offset = offset_in(
        block,
        node,
        local(node, transform, position),
        line.text.len(),
    );
    let key = line.key;
    let count = click_count(selection.last.as_ref(), time.elapsed(), key, position);
    let span = match count {
        2 => word_range(&line.text, offset),
        3 => 0..line.text.len(),
        _ => offset..offset,
    };
    selection.group = Some(key.group);
    selection.column = key.column;
    selection.anchor = At {
        order: key.order,
        offset: span.start,
    };
    selection.focus = At {
        order: key.order,
        offset: span.end,
    };
    selection.dragged = false;
    selection.multi = count > 1;
    selection.pressed = true;
    selection.armed = true;
    selection.fingerprint = None;
    selection.last = Some(LastPress {
        at: time.elapsed(),
        key,
        position,
        count,
    });
}

fn on_drag(drag: On<Pointer<Drag>>, lines: Query<Measured>, mut selection: ResMut<CodeSelection>) {
    if drag.button != PointerButton::Primary || !is_drag(drag.distance) {
        return;
    }
    let Ok((origin, ..)) = lines.get(drag.event_target()) else {
        return;
    };
    let Some(group) = selection.group.filter(|g| *g == origin.key.group) else {
        return;
    };
    selection.dragged = true;
    let (column, position) = (selection.column, drag.pointer_location.position);
    // The line on the pointer's row of this column, or the nearest one above or below it.
    let nearest = lines
        .iter()
        .filter(|(l, ..)| l.key.group == group && l.key.column == column)
        .map(|m @ (l, _, node, transform)| {
            let y = position.y / node.inverse_scale_factor;
            let gap = ((y - transform.translation.y).abs() - node.size.y / 2.0).max(0.0);
            (gap, l.key.order, m)
        })
        .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let Some((_, _, (line, block, node, transform))) = nearest else {
        return;
    };
    let offset = offset_in(
        block,
        node,
        local(node, transform, position),
        line.text.len(),
    );
    let focus = At {
        order: line.key.order,
        offset,
    };
    if selection.focus != focus {
        selection.focus = focus;
    }
}

/// A press that landed on no code ends the selection and whatever it left for a click.
fn forget(mouse: Option<Res<ButtonInput<MouseButton>>>, mut selection: ResMut<CodeSelection>) {
    // Reads first: writing marks the selection changed, which repaints it.
    if selection.pressed {
        selection.pressed = false;
    } else if mouse.is_some_and(|m| m.just_pressed(MouseButton::Left)) {
        selection.group = None;
        selection.dragged = false;
        selection.multi = false;
        selection.armed = false;
    }
}

/// Ends a selection whose lines are gone or now say something else: its offsets would point
/// into other text.
fn drop_stale(mut selection: ResMut<CodeSelection>, lines: Query<&CodeLine>) {
    let Some(group) = selection.group else {
        return;
    };
    let mut own: Vec<&CodeLine> = lines.iter().filter(|l| l.key.group == group).collect();
    if own.is_empty() {
        selection.group = None;
        return;
    }
    own.sort_by_key(|l| (l.key.order, l.key.column));
    let mut hasher = DefaultHasher::new();
    for l in own {
        (l.key.order, l.key.column, &l.text).hash(&mut hasher);
    }
    let seen = hasher.finish();
    match selection.fingerprint {
        None => selection.fingerprint = Some(seen),
        Some(then) if then != seen => selection.group = None,
        Some(_) => {}
    }
}

/// Cmd-C (Ctrl-C elsewhere) copies the selection, unless a text field has the keyboard: it
/// copies its own.
fn copy_selection(
    keys: Res<ButtonInput<KeyCode>>,
    selection: Res<CodeSelection>,
    lines: Query<&CodeLine>,
    focus: Option<Res<InputFocus>>,
    fields: Query<(), With<EditableText>>,
    recorder: Option<ResMut<CopiedText>>,
    clipboard: Option<ResMut<Clipboard>>,
) {
    let chord = keys.any_pressed([
        KeyCode::SuperLeft,
        KeyCode::SuperRight,
        KeyCode::ControlLeft,
        KeyCode::ControlRight,
    ]) && !keys.any_pressed([
        KeyCode::ShiftLeft,
        KeyCode::ShiftRight,
        KeyCode::AltLeft,
        KeyCode::AltRight,
    ]);
    if !chord || !keys.just_pressed(KeyCode::KeyC) || selection.is_empty() {
        return;
    }
    if focus
        .and_then(|f| f.get())
        .is_some_and(|entity| fields.contains(entity))
    {
        return;
    }
    let text = selection.text(lines);
    if !text.is_empty() {
        copy_text(recorder, clipboard, &text);
    }
}

/// Draws each line's highlight: one rectangle per visual line the selection covers, placed in
/// the line's layer. A line is redrawn only when what it shows changed.
fn paint(
    mut commands: Commands,
    selection: Res<CodeSelection>,
    lines: Query<(
        &CodeLine,
        &ComputedTextBlock,
        &ComputedNode,
        &UiGlobalTransform,
        &TextLayoutInfo,
    )>,
    mut layers: Query<(&mut Highlight, &ComputedNode, &UiGlobalTransform), Without<CodeLine>>,
    moved: Query<
        (),
        Or<(
            Added<CodeLine>,
            Changed<TextLayoutInfo>,
            Changed<ComputedNode>,
        )>,
    >,
) {
    if !selection.is_changed() && moved.is_empty() {
        return;
    }
    for (line, block, node, transform, info) in &lines {
        let Ok((mut shown, layer_node, layer_at)) = layers.get_mut(line.layer) else {
            continue;
        };
        let wanted = selection
            .group
            .filter(|g| *g == line.key.group && selection.column == line.key.column)
            .and_then(|_| {
                line_range(
                    line.key.order,
                    line.text.len(),
                    selection.anchor,
                    selection.focus,
                )
            })
            .map(|r| {
                let len = line.text.len();
                floor_boundary(&line.text, r.start)..floor_boundary(&line.text, r.end).min(len)
            })
            .filter(|r| r.start < r.end);
        // Where the text starts, from the layer's own corner, in logical pixels.
        let scale = node.inverse_scale_factor;
        let origin = ((transform.translation - node.size / 2.0)
            - (layer_at.translation - layer_node.size / 2.0))
            * scale;
        let now = Highlight {
            range: wanted,
            size: info.size,
            origin,
        };
        if *shown == now {
            continue;
        }
        commands.entity(line.layer).despawn_related::<Children>();
        if let Some(range) = &now.range {
            let layout = block.buffer();
            let selected = Selection::new(
                Cursor::from_byte_index(layout, range.start, Affinity::Downstream),
                Cursor::from_byte_index(layout, range.end, Affinity::Upstream),
            );
            commands.entity(line.layer).with_children(|c| {
                for (b, _) in selected.geometry(layout) {
                    c.spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(origin.x + b.x0 as f32 * scale),
                            top: px(origin.y + b.y0 as f32 * scale),
                            width: px((b.x1 - b.x0) as f32 * scale),
                            height: px((b.y1 - b.y0) as f32 * scale),
                            ..default()
                        },
                        BackgroundColor::default(),
                        Fill(Swatch::TextSelect),
                        Pickable::IGNORE,
                    ));
                }
            });
        }
        *shown = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(order: usize, offset: usize) -> At {
        At { order, offset }
    }

    fn line(order: usize, column: u8, text: &str) -> SelLine<'_> {
        SelLine {
            order,
            column,
            text,
        }
    }

    fn key(order: usize) -> LineKey {
        LineKey {
            group: Entity::PLACEHOLDER,
            order,
            column: 0,
        }
    }

    #[test]
    fn a_short_move_is_a_click_and_a_long_one_a_drag() {
        assert!(!is_drag(Vec2::ZERO));
        assert!(!is_drag(Vec2::new(2.0, 2.0)));
        assert!(is_drag(Vec2::new(DRAG_SLOP, 0.0)));
        assert!(is_drag(Vec2::new(-30.0, 1.0)));
    }

    #[test]
    fn the_range_on_one_line_is_the_text_between_the_points_in_either_direction() {
        assert_eq!(line_range(3, 10, at(3, 2), at(3, 6)), Some(2..6));
        assert_eq!(line_range(3, 10, at(3, 6), at(3, 2)), Some(2..6));
        assert_eq!(line_range(3, 10, at(3, 4), at(3, 4)), Some(4..4));
    }

    #[test]
    fn a_range_over_lines_takes_the_end_of_the_first_all_of_the_middle_and_the_start_of_the_last() {
        assert_eq!(line_range(1, 8, at(1, 3), at(4, 5)), Some(3..8));
        assert_eq!(line_range(2, 8, at(1, 3), at(4, 5)), Some(0..8));
        assert_eq!(line_range(4, 9, at(1, 3), at(4, 5)), Some(0..5));
        assert_eq!(line_range(4, 9, at(4, 5), at(1, 3)), Some(0..5));
        assert_eq!(line_range(0, 9, at(1, 3), at(4, 5)), None);
        assert_eq!(line_range(5, 9, at(1, 3), at(4, 5)), None);
    }

    #[test]
    fn a_point_past_the_end_of_a_line_stops_at_its_end() {
        assert_eq!(line_range(1, 4, at(1, 2), at(1, 99)), Some(2..4));
    }

    #[test]
    fn unified_text_is_the_covered_part_of_each_line_one_per_row() {
        let lines = [
            line(0, 0, "fn main() {"),
            line(1, 0, "    let x = 1;"),
            line(2, 0, "}"),
        ];
        assert_eq!(
            selected_text(&lines, 0, at(0, 3), at(2, 1)),
            "main() {\n    let x = 1;\n}"
        );
        assert_eq!(selected_text(&lines, 0, at(1, 8), at(1, 13)), "x = 1");
    }

    #[test]
    fn a_collapsed_selection_holds_nothing() {
        let lines = [line(0, 0, "abc")];
        assert_eq!(selected_text(&lines, 0, at(0, 1), at(0, 1)), "");
    }

    #[test]
    fn the_text_never_holds_a_number_or_a_sign() {
        // The lines carry the code alone: numbers and signs are other nodes.
        let lines = [line(0, 0, "let a = 1;"), line(1, 0, "let b = 2;")];
        let copied = selected_text(&lines, 0, at(0, 0), at(1, 10));
        assert_eq!(copied, "let a = 1;\nlet b = 2;");
    }

    #[test]
    fn a_split_selection_stays_on_its_side() {
        let lines = [
            line(0, 0, "old one"),
            line(0, 1, "new one"),
            line(1, 0, "old two"),
            line(1, 1, "new two"),
            line(2, 1, "new three"),
        ];
        assert_eq!(
            selected_text(&lines, 1, at(0, 4), at(2, 3)),
            "one\nnew two\nnew"
        );
        assert_eq!(
            selected_text(&lines, 0, at(0, 0), at(2, 9)),
            "old one\nold two"
        );
    }

    #[test]
    fn a_wrapped_line_copies_as_its_one_original_line() {
        // Wrapping is the layout's: the line's text has no break in it.
        let long = "let message = format!(\"a long line that wraps at the edge of the box\");";
        let lines = [line(7, 0, long)];
        assert_eq!(selected_text(&lines, 0, at(7, 0), at(7, long.len())), long);
        assert!(!selected_text(&lines, 0, at(7, 4), at(7, 40)).contains('\n'));
    }

    #[test]
    fn lines_given_out_of_order_still_read_top_down() {
        let lines = [line(2, 0, "c"), line(0, 0, "a"), line(1, 0, "b")];
        assert_eq!(selected_text(&lines, 0, at(0, 0), at(2, 1)), "a\nb\nc");
    }

    #[test]
    fn a_point_inside_a_character_does_not_cut_it() {
        let lines = [line(0, 0, "aéb")];
        assert_eq!(selected_text(&lines, 0, at(0, 2), at(0, 4)), "éb");
    }

    #[test]
    fn a_word_is_a_run_of_identifier_characters() {
        let text = "let total_sum = a + 42;";
        assert_eq!(&text[word_range(text, 6)], "total_sum");
        assert_eq!(&text[word_range(text, 4)], "total_sum");
        assert_eq!(&text[word_range(text, 12)], "total_sum");
        assert_eq!(&text[word_range(text, 20)], "42");
        assert_eq!(&text[word_range(text, 0)], "let");
    }

    #[test]
    fn a_word_on_a_blank_or_a_symbol_is_that_run_or_character() {
        let text = "a   +b";
        assert_eq!(&text[word_range(text, 2)], "   ");
        assert_eq!(&text[word_range(text, 4)], "+");
        assert_eq!(&text[word_range(text, 6)], "b");
        assert_eq!(word_range("", 0), 0..0);
    }

    #[test]
    fn a_second_close_press_on_the_same_line_is_a_double_click_and_a_third_a_triple() {
        let ms = Duration::from_millis;
        let p = Vec2::new(100.0, 40.0);
        let first = click_count(None, ms(0), key(2), p);
        assert_eq!(first, 1);
        let last = LastPress {
            at: ms(0),
            key: key(2),
            position: p,
            count: first,
        };
        let second = click_count(Some(&last), ms(200), key(2), p + Vec2::new(2.0, 0.0));
        assert_eq!(second, 2);
        let last = LastPress {
            at: ms(200),
            count: second,
            ..last
        };
        assert_eq!(click_count(Some(&last), ms(400), key(2), p), 3);
        let last = LastPress {
            at: ms(400),
            count: 3,
            ..last
        };
        assert_eq!(click_count(Some(&last), ms(500), key(2), p), 1);
    }

    #[test]
    fn a_slow_or_distant_or_other_line_press_starts_over() {
        let ms = Duration::from_millis;
        let p = Vec2::new(100.0, 40.0);
        let last = LastPress {
            at: ms(0),
            key: key(2),
            position: p,
            count: 1,
        };
        assert_eq!(click_count(Some(&last), ms(900), key(2), p), 1);
        assert_eq!(click_count(Some(&last), ms(100), key(3), p), 1);
        assert_eq!(
            click_count(Some(&last), ms(100), key(2), p + Vec2::new(40.0, 0.0)),
            1
        );
    }
}
