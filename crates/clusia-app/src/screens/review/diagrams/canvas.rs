//! The canvas of the Diagrams tab: one diagram, laid out by `clusia-diagram` and drawn with plain
//! `bevy_ui` nodes.
//!
//! `DiagramCanvas` (spawned by the tab while the selected entry is ready) holds, in order: the
//! dotted ground, a clipped `Viewport` that takes the drag, scroll and pinch, the header, the zoom
//! control and the legend. The viewport holds `Content`, the one node that carries the
//! `UiTransform`; everything the layout produced is an absolute child of it, boxes last so they
//! are on top and Tab reaches them in reading order. Only boxes take the pointer: every other
//! node is `Pickable::IGNORE`, so a drag that starts on a line pans.
//!
//! The zoom and pan live in `CanvasState.view` (pure, see `view`). `apply_view` turns them into
//! the transform, `sync_viewport` tells the view how big its viewport is after layout.

use std::collections::HashSet;

use bevy::asset::RenderAssetUsages;
use bevy::diagnostic::FrameCount;
use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::input::gestures::PinchGesture;
use bevy::input::mouse::MouseScrollUnit;
use bevy::input_focus::InputFocus;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::Pickable;
use bevy::picking::events::{Drag, DragEnd, Pointer, Press, Scroll};
use bevy::picking::hover::Hovered;
use bevy::picking::pointer::PointerButton;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::text::{Justify, LineBreak, Strikethrough};
use bevy::ui::{ComputedNode, UiGlobalTransform, UiSystems, UiTransform, Val2};
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};
use bevy::window::PrimaryWindow;

use clusia_core::PrRef;
use clusia_core::diagram::{Change, DiagramSpec, EntryState, History, LineStyle, RowKey, Shape};
use clusia_diagram::{
    Align, Arrow, BoxGeo, EdgeGeo, Fonts, Geometry, Measure, Point, Rect as GRect, TextLines,
    TextStyle, layout, style_size,
};

use super::DiagramCanvas;
use super::model::{Diagrams, picked};
use super::view::{BUTTON_STEP, CanvasView, pinch_factor, wheel_factor, wheel_pixels};
use crate::fonts::UiFonts;
use crate::review_state::ReviewTabs;
use crate::theme::{Swatch, Theme};
use crate::ui::kit::{Clickable, Fill, HoverFill, Stroke, Type, text};

/// A drag longer than this (in pixels) is a pan: the click that ends it opens nothing.
pub const DRAG_SLOP: f32 = 4.0;
/// How long after its last move a drag still swallows the click that ends it, in seconds.
pub const DRAG_GRACE: f64 = 0.25;
/// The pitch of the dots of the ground.
const DOT_PITCH: u32 = 20;
/// The border of a box that changed, is new or is at risk; a plain box has 1 px.
const RING: f32 = 1.5;
/// How far outside a box the outer edge of its dashed ring is: a 2 px gap and the ring.
const RING_GAP: f32 = 3.0;
/// The thickness of the dashed ring of a new box.
const RING_LINE: f32 = 1.0;
/// The corner radius of a box that is not round.
const BOX_RADIUS: f32 = 8.0;
const DASH_ON: f32 = 5.0;
const DASH_OFF: f32 = 3.0;
/// The finer dash of a ring.
const RING_ON: f32 = 3.0;
const RING_OFF: f32 = 2.0;
/// The thickness of a line of an edge.
const LINE: f32 = 2.0;
/// The side of the square an arrowhead glyph is centred in.
const HEAD: f32 = 10.0;
/// Space between a row's name and its `PK` or `FK` tag.
const KEY_GAP: f32 = 6.0;
/// Boxes whose tops are this close are read as one row.
const ROW_BAND: f32 = 24.0;

// ---- pure pieces

/// What the canvas draws of `spec`: while it does not compare, what is gone in the version
/// before is not drawn at all, nor the lines that touched it.
pub fn shown_spec(spec: &DiagramSpec, compare: bool) -> DiagramSpec {
    if compare {
        return spec.clone();
    }
    let gone: HashSet<&str> = spec
        .nodes
        .iter()
        .filter(|n| n.history == Some(History::Removed))
        .map(|n| n.id.as_str())
        .collect();
    let mut shown = spec.clone();
    shown.nodes.retain(|n| !gone.contains(n.id.as_str()));
    shown.edges.retain(|e| {
        e.history != Some(History::Removed)
            && !gone.contains(e.from.as_str())
            && !gone.contains(e.to.as_str())
    });
    shown.messages.retain(|m| {
        m.history != Some(History::Removed)
            && !gone.contains(m.from.as_str())
            && !gone.contains(m.to.as_str())
    });
    for group in &mut shown.groups {
        group.members.retain(|m| !gone.contains(m.as_str()));
    }
    shown
}

/// The order the boxes are spawned in, so Tab reads them like text: row by row, left to right.
pub fn reading_order(boxes: &[BoxGeo]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..boxes.len()).collect();
    order.sort_by(|&a, &b| {
        let key = |i: usize| ((boxes[i].rect.y / ROW_BAND).round() as i64, boxes[i].rect.x);
        let (ka, kb) = (key(a), key(b));
        ka.0.cmp(&kb.0)
            .then(ka.1.partial_cmp(&kb.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.cmp(&b))
    });
    order
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Right,
    Left,
    Up,
    Down,
}

/// The way a line that goes from `from` to `to` points.
pub fn direction(from: Point, to: Point) -> Dir {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    if dx.abs() >= dy.abs() {
        if dx >= 0.0 { Dir::Right } else { Dir::Left }
    } else if dy >= 0.0 {
        Dir::Down
    } else {
        Dir::Up
    }
}

/// The glyph of an arrowhead whose tip is at `tip`, and the square it is centred in. Inter has
/// all four triangles, filled, so a head needs no rotation.
pub fn arrow_head(tip: Point, dir: Dir) -> (GRect, &'static str) {
    let h = HEAD;
    match dir {
        Dir::Right => (
            GRect {
                x: tip.x - h,
                y: tip.y - h / 2.0,
                w: h,
                h,
            },
            "▶",
        ),
        Dir::Left => (
            GRect {
                x: tip.x,
                y: tip.y - h / 2.0,
                w: h,
                h,
            },
            "◀",
        ),
        Dir::Down => (
            GRect {
                x: tip.x - h / 2.0,
                y: tip.y - h,
                w: h,
                h,
            },
            "▼",
        ),
        Dir::Up => (
            GRect {
                x: tip.x - h / 2.0,
                y: tip.y,
                w: h,
                h,
            },
            "▲",
        ),
    }
}

/// How a box is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Look {
    pub fill: Swatch,
    pub ring: Swatch,
    pub ring_w: f32,
    /// A dashed ring around the box.
    pub dashed: bool,
    pub title: Swatch,
    pub sub: Swatch,
    pub strike: bool,
}

/// The look of a box. Outside a comparison its `change` decides (changed, new and at-risk boxes
/// stand out); in one only `history` does (added, changed since, removed), and the rest is plain.
pub fn look(shape: Shape, change: Change, history: Option<History>, compare: bool) -> Look {
    let base = |fill, ring, ring_w| Look {
        fill,
        ring,
        ring_w,
        dashed: false,
        title: Swatch::Fg,
        sub: Swatch::Muted,
        strike: false,
    };
    let plain = match shape {
        Shape::Store => base(Swatch::Chrome, Swatch::Line, 1.0),
        Shape::Actor | Shape::External => base(Swatch::Surface, Swatch::Edge, 1.0),
        Shape::Start => base(Swatch::Fg, Swatch::Fg, 0.0),
        _ => base(Swatch::Bg, Swatch::Line, 1.0),
    };
    if shape == Shape::Start {
        return plain;
    }
    if compare {
        return match history {
            Some(History::Added) => Look {
                dashed: true,
                sub: Swatch::Green,
                ..base(Swatch::Bg, Swatch::Green, RING)
            },
            Some(History::Changed) => Look {
                sub: Swatch::Amber,
                ..base(Swatch::AmberSoft, Swatch::Amber, RING)
            },
            Some(History::Removed) => Look {
                title: Swatch::GhostInk,
                sub: Swatch::GhostInk,
                strike: true,
                ..base(Swatch::GhostFill, Swatch::Line, 1.0)
            },
            _ => plain,
        };
    }
    match change {
        Change::Changed => Look {
            sub: Swatch::Green,
            ..base(Swatch::ChangedBg, Swatch::Green, RING)
        },
        Change::New => Look {
            dashed: true,
            sub: Swatch::Green,
            ..base(Swatch::Bg, Swatch::Green, RING)
        },
        Change::Risk => Look {
            sub: Swatch::Orange,
            ..base(Swatch::RiskBg, Swatch::Orange, RING)
        },
        Change::Unchanged => plain,
    }
}

/// How a line (an edge, or a message of a sequence) and its label are drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineLook {
    pub ink: Swatch,
    pub dashed: bool,
    pub label: Swatch,
    pub strike: bool,
}

/// The look of a line. `soft` draws an unmarked line in `EdgeSoft` (the reply of a sequence).
pub fn line_look(
    change: Change,
    history: Option<History>,
    compare: bool,
    dashed: bool,
    soft: bool,
) -> LineLook {
    let quiet = if soft { Swatch::EdgeSoft } else { Swatch::Edge };
    let plain = LineLook {
        ink: quiet,
        dashed,
        label: Swatch::Faint,
        strike: false,
    };
    if compare {
        return match history {
            Some(History::Added) => LineLook {
                ink: Swatch::Green,
                dashed: true,
                label: Swatch::Green,
                strike: false,
            },
            Some(History::Changed) => LineLook {
                ink: Swatch::Amber,
                dashed,
                label: Swatch::Amber,
                strike: false,
            },
            Some(History::Removed) => LineLook {
                ink: Swatch::EdgeSoft,
                dashed: true,
                label: Swatch::GhostInk,
                strike: true,
            },
            _ => plain,
        };
    }
    match change {
        Change::New | Change::Changed => LineLook {
            ink: Swatch::Green,
            label: Swatch::Green,
            ..plain
        },
        Change::Risk => LineLook {
            ink: Swatch::Orange,
            label: Swatch::Orange,
            ..plain
        },
        Change::Unchanged => plain,
    }
}

/// The tile of the dotted ground: one white dot of 2 × 2 pixels, tinted with `CanvasDot` by the
/// node.
pub fn dot_tile() -> Image {
    let side = DOT_PITCH;
    let mut data = vec![0_u8; (side * side * 4) as usize];
    for (row, col) in [(9, 9), (9, 10), (10, 9), (10, 10)] {
        let at = ((row * side + col) * 4) as usize;
        data[at..at + 4].copy_from_slice(&[255, 255, 255, 255]);
    }
    Image::new(
        Extent3d {
            width: side,
            height: side,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

/// The tile of a dashed line: five opaque pixels and three clear ones along its length, one
/// pixel across (the node stretches it to the line's thickness). `horizontal` runs along x.
pub fn dash_tile(horizontal: bool) -> Image {
    tile(horizontal, DASH_ON, DASH_OFF)
}

/// The tile of the dashed ring of a new box: three on, two off.
pub fn ring_dash_tile(horizontal: bool) -> Image {
    tile(horizontal, RING_ON, RING_OFF)
}

fn tile(horizontal: bool, on: f32, off: f32) -> Image {
    let pitch = (on + off) as u32;
    let mut data = Vec::with_capacity(pitch as usize * 4);
    for i in 0..pitch {
        let alpha = if (i as f32) < on { 255 } else { 0 };
        data.extend_from_slice(&[255, 255, 255, alpha]);
    }
    let (width, height) = if horizontal { (pitch, 1) } else { (1, pitch) };
    Image::new(
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

// ---- state and components

/// The tiles of the canvas, made once: the dot of the ground and the dash of a dashed line
/// along x and along y.
#[derive(Resource, Debug, Clone)]
pub struct Dots {
    pub dot: Handle<Image>,
    pub dash_x: Handle<Image>,
    pub dash_y: Handle<Image>,
    pub ring_x: Handle<Image>,
    pub ring_y: Handle<Image>,
}

/// A tiled image node whose tint is a swatch, so a theme switch recolours it.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tint(pub Swatch);

/// A dashed line or ring side: one node with a tiled dash image, whatever its length.
#[derive(Component, Debug)]
pub struct DashedPiece;

type Built = (u32, String, bool, DiagramSpec);

/// What a canvas shows and where: on the `DiagramCanvas` entity.
#[derive(Component, Debug)]
pub struct CanvasState {
    pub view: CanvasView,
    /// The view has been fitted to the viewport once.
    pub fitted: bool,
    /// The longest drag since the last press, in pixels.
    pub dragged: f32,
    /// When the last drag moved, in seconds of the app's clock.
    pub drag_at: f64,
    /// A drag with the primary button is going on (from its first move to `Pointer<DragEnd>`).
    pub dragging: bool,
    /// The frame `Pointer<DragEnd>` last arrived in.
    pub drag_end_frame: Option<u32>,
    built: Option<Built>,
}

/// The clipped area the drag, scroll and pinch land on.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport(pub Entity);

/// The node that carries the transform.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentNode(pub Entity);

/// A box of the diagram: a button that opens its code.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct DiagramBox {
    pub canvas: Entity,
    pub pr: PrRef,
    pub number: u32,
    pub key: String,
    pub node: String,
    /// In content pixels, for `reveal`.
    pub rect: GRect,
    pub anchored: bool,
}

/// **−** and **+**.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct ZoomButton {
    pub canvas: Entity,
    pub factor: f32,
}

/// The `100%` between them.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoomLabel(pub Entity);

/// The dotted ground.
#[derive(Component, Debug)]
pub struct CanvasDots;

/// A line of an edge or a message, one node per segment or dash.
#[derive(Component, Debug)]
pub struct LinePiece;

/// The legend.
#[derive(Component, Debug)]
pub struct Legend;

/// The lines `pan`, `scroll` and `press` apply, apart from the events that carry them.
pub fn press(state: &mut CanvasState) {
    state.dragged = 0.0;
    state.dragging = false;
    state.drag_end_frame = None;
}

pub fn drag(state: &mut CanvasState, delta: Vec2, distance: Vec2, now: f64) {
    state.view.pan_by(delta);
    state.dragged = state.dragged.max(distance.length());
    state.drag_at = now;
    state.dragging = true;
}

/// The drag was released in `frame`. The click that ends a drag can arrive before or after
/// this, in the same frame, so both orders are swallowed.
pub fn end_drag(state: &mut CanvasState, frame: u32) {
    state.dragging = false;
    state.drag_end_frame = Some(frame);
}

/// Whether the click or key press that activates a box at `now`, in `frame`, is the end of a
/// pan: a drag longer than `DRAG_SLOP` that is still going on, that was released in this frame
/// (however long it paused before), or that moved less than `DRAG_GRACE` ago (the fallback when
/// no release was seen). A pan that ended earlier does not count, so the next click, Enter or
/// Space opens the box it is meant for.
pub fn swallows(state: &CanvasState, now: f64, frame: u32) -> bool {
    state.dragged > DRAG_SLOP
        && (state.dragging
            || state.drag_end_frame == Some(frame)
            || now - state.drag_at < DRAG_GRACE)
}

/// A scroll: with ⌘ it zooms about `cursor` (canvas pixels), else it pans.
pub fn scroll(state: &mut CanvasState, cursor: Vec2, command: bool, dx: f32, dy: f32) {
    if command {
        state.view.zoom_at(cursor, wheel_factor(dy));
    } else {
        state.view.pan_by(Vec2::new(dx, dy));
    }
}

fn command_held(keys: &ButtonInput<KeyCode>) -> bool {
    keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight)
}

/// The top-left corner of `canvas`'s viewport on the window, in logical pixels.
fn viewport_origin(
    canvas: Entity,
    viewports: &Query<(&Viewport, &ComputedNode, &UiGlobalTransform)>,
) -> Option<Vec2> {
    viewports
        .iter()
        .find(|(v, _, _)| v.0 == canvas)
        .map(|(_, node, at)| {
            let s = node.inverse_scale_factor;
            at.translation * s - node.size * s / 2.0
        })
}

// ---- building the canvas

struct Ctx {
    canvas: Entity,
    pr: PrRef,
    number: u32,
    key: String,
    compare: bool,
    tiles: Option<Dots>,
}

/// Fills each `DiagramCanvas` with the diagram it stands for, and again when that changes (a
/// redraw, a version, the comparison). While nothing changes it does nothing, so the zoom and
/// the pan are kept.
pub(super) fn fill_canvas(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    diagrams: Res<Diagrams>,
    fonts: Res<UiFonts>,
    theme: Res<Theme>,
    dots: Option<Res<Dots>>,
    canvases: Query<(Entity, &DiagramCanvas, Option<&CanvasState>)>,
) {
    let fresh = canvases.iter().any(|(_, _, state)| state.is_none());
    if !(fresh || tabs.is_changed() || diagrams.is_changed() || theme.is_changed()) {
        return;
    }
    for (entity, canvas, state) in &canvases {
        let (Some(tab), Some(model)) = (tabs.0.get(&canvas.0), diagrams.0.get(&canvas.0)) else {
            continue;
        };
        let Some(p) = picked(model, &tab.ui) else {
            continue;
        };
        let EntryState::Ready { spec, .. } = &p.entry.state else {
            continue;
        };
        let key = &p.entry.plan.key;
        let compare = tab.ui.diagram_compare && model.compare_with(p.number, key).is_some();
        if state.is_some_and(|s| {
            s.built
                .as_ref()
                .is_some_and(|b| (b.0, b.2) == (p.number, compare) && b.1 == *key && b.3 == *spec)
        }) {
            continue;
        }
        let key = key.clone();
        let built: Built = (p.number, key.clone(), compare, spec.clone());
        let shown = shown_spec(spec, compare);
        let geometry = layout(&shown, &Fonts);
        let mut view = CanvasView::new(Vec2::new(geometry.size.0, geometry.size.1));
        if let Some(old) = state {
            view.viewport = old.view.viewport;
        }
        let ctx = Ctx {
            canvas: entity,
            pr: canvas.0.clone(),
            number: p.number,
            key,
            compare,
            tiles: dots.as_deref().cloned(),
        };
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|c| {
            spawn_canvas(c, &fonts, &theme, dots.as_deref(), &ctx, &shown, &geometry);
        });
        commands.entity(entity).insert(CanvasState {
            view,
            fitted: false,
            dragged: 0.0,
            drag_at: 0.0,
            dragging: false,
            drag_end_frame: None,
            built: Some(built),
        });
    }
}

fn ignore() -> Pickable {
    Pickable::IGNORE
}

fn type_of(style: TextStyle, ink: Swatch) -> Type {
    let (size, weight, mono) = style_size(style);
    Type {
        size,
        weight,
        ink,
        mono,
    }
}

/// Where `line` of `block` starts. A centred line is placed by its measured width: `Justify`
/// only centres a line against the widest line of its own text, not against the node.
pub fn line_left(block: &TextLines, line: &str) -> f32 {
    match block.align {
        Align::Left => block.at.x,
        Align::Center => block.at.x + (block.width - Fonts.width(line, block.style)) / 2.0,
    }
}

/// Where the `PK` or `FK` tag of a row starts: just after the row's name, in the same face.
pub fn key_left(name: &TextLines) -> f32 {
    let line = name.lines.first().map_or("", String::as_str);
    line_left(name, line) + Fonts.width(line, name.style) + KEY_GAP
}

/// One node per line of `block`, positioned from `origin` (the top-left corner of the node the
/// lines are children of).
fn text_block(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    block: &TextLines,
    ink: Swatch,
    strike: bool,
    origin: Vec2,
) {
    let line_height = Fonts.line_height(block.style);
    for (i, line) in block.lines.iter().enumerate() {
        let mut e = p.spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(line_left(block, line) - origin.x),
                top: px(block.at.y + i as f32 * line_height - origin.y),
                ..default()
            },
            text(fonts, line.clone(), type_of(block.style, ink)),
            TextLayout::new(Justify::Left, LineBreak::NoWrap),
            ignore(),
        ));
        if strike {
            e.insert(Strikethrough);
        }
    }
}

fn strip(p: &mut ChildSpawnerCommands, x: f32, y: f32, w: f32, h: f32, ink: Swatch) {
    p.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(x),
            top: px(y),
            width: px(w),
            height: px(h),
            ..default()
        },
        BackgroundColor::default(),
        Fill(ink),
        ignore(),
        LinePiece,
    ));
}

/// One node that draws a dashed line: a tiled dash image `len` long, `thickness` thick.
fn dashed_strip(
    p: &mut ChildSpawnerCommands,
    tile: &Handle<Image>,
    horizontal: bool,
    (x, y): (f32, f32),
    len: f32,
    thickness: f32,
    ink: Swatch,
) {
    let (w, h) = if horizontal {
        (len, thickness)
    } else {
        (thickness, len)
    };
    p.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(x),
            top: px(y),
            width: px(w),
            height: px(h),
            ..default()
        },
        ImageNode {
            image: tile.clone(),
            color: Color::NONE,
            image_mode: NodeImageMode::Tiled {
                tile_x: horizontal,
                tile_y: !horizontal,
                stretch_value: 1.0,
            },
            ..default()
        },
        Tint(ink),
        ignore(),
        LinePiece,
        DashedPiece,
    ));
}

/// The rectangle of the straight piece from `a` to `b`, `LINE` thick. A piece that ends at an
/// inner bend grows by half a line there, so the outer corner of the elbow is filled.
pub fn strip_rect(a: Point, b: Point, grow_a: bool, grow_b: bool) -> (GRect, bool) {
    let horizontal = (a.y - b.y).abs() <= (a.x - b.x).abs();
    let (lo, hi, at) = if horizontal {
        (a.x.min(b.x), a.x.max(b.x), a.y)
    } else {
        (a.y.min(b.y), a.y.max(b.y), a.x)
    };
    let forward = if horizontal { a.x <= b.x } else { a.y <= b.y };
    let (grow_lo, grow_hi) = if forward {
        (grow_a, grow_b)
    } else {
        (grow_b, grow_a)
    };
    let (lo, hi) = (
        lo - if grow_lo { LINE / 2.0 } else { 0.0 },
        hi + if grow_hi { LINE / 2.0 } else { 0.0 },
    );
    let rect = if horizontal {
        GRect {
            x: lo,
            y: at - LINE / 2.0,
            w: hi - lo,
            h: LINE,
        }
    } else {
        GRect {
            x: at - LINE / 2.0,
            y: lo,
            w: LINE,
            h: hi - lo,
        }
    };
    (rect, horizontal)
}

/// A straight piece of an edge, solid or dashed (one node either way).
fn segment(
    p: &mut ChildSpawnerCommands,
    tiles: Option<&Dots>,
    (a, b): (Point, Point),
    grow: (bool, bool),
    ink: Swatch,
    dashed: bool,
) {
    let (r, horizontal) = strip_rect(a, b, grow.0, grow.1);
    let len = if horizontal { r.w } else { r.h };
    match tiles {
        Some(tiles) if dashed => {
            let tile = if horizontal {
                &tiles.dash_x
            } else {
                &tiles.dash_y
            };
            dashed_strip(p, tile, horizontal, (r.x, r.y), len, LINE, ink);
        }
        _ => strip(p, r.x, r.y, r.w, r.h, ink),
    }
}

fn head(p: &mut ChildSpawnerCommands, fonts: &UiFonts, tip: Point, dir: Dir, ink: Swatch) {
    let (rect, glyph) = arrow_head(tip, dir);
    p.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(rect.x),
            top: px(rect.y),
            width: px(rect.w),
            height: px(rect.h),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            ..default()
        },
        ignore(),
        LinePiece,
    ))
    .with_children(|h| {
        h.spawn((
            text(fonts, glyph, Type::BODY.size(HEAD).ink(ink)),
            TextLayout::no_wrap(),
            ignore(),
        ));
    });
}

/// Where the pieces of a dashed ring go: straight dashed sides (along x or not, top left, length)
/// and four quarter circles, `radius` across, at the top left, top right, bottom left and
/// bottom right.
#[derive(Debug, Clone, PartialEq)]
pub struct RingParts {
    pub radius: f32,
    pub sides: Vec<(bool, (f32, f32), f32)>,
    pub corners: [GRect; 4],
}

/// The ring whose outer edge is `gap` px outside `rect`, with the box's corner `radius` grown by
/// the gap so it stays parallel to the border. Sides of no length (the ends of a pill) are left
/// out.
pub fn ring_parts(rect: GRect, radius: f32, gap: f32) -> RingParts {
    let (x, y) = (rect.x - gap, rect.y - gap);
    let (w, h) = (rect.w + 2.0 * gap, rect.h + 2.0 * gap);
    let r = (radius + gap).min(w / 2.0).min(h / 2.0);
    let t = RING_LINE;
    let sides = [
        (true, (x + r, y), w - 2.0 * r),
        (true, (x + r, y + h - t), w - 2.0 * r),
        (false, (x, y + r), h - 2.0 * r),
        (false, (x + w - t, y + r), h - 2.0 * r),
    ]
    .into_iter()
    .filter(|s| s.2 > 0.5)
    .collect();
    let corner = |cx, cy| GRect {
        x: cx,
        y: cy,
        w: r,
        h: r,
    };
    RingParts {
        radius: r,
        sides,
        corners: [
            corner(x, y),
            corner(x + w - r, y),
            corner(x, y + h - r),
            corner(x + w - r, y + h - r),
        ],
    }
}

/// A dashed ring `gap` px outside `rect` (see `ring_parts`): a tiled node per side, solid when
/// there are no tiles, and a bordered node per corner whose one round corner draws the arc.
fn dashed_ring(
    p: &mut ChildSpawnerCommands,
    tiles: Option<&Dots>,
    rect: GRect,
    radius: f32,
    gap: f32,
    ink: Swatch,
) {
    let ring = ring_parts(rect, radius, gap);
    let t = RING_LINE;
    for (horizontal, at, len) in ring.sides {
        match tiles {
            Some(tiles) => {
                let tile = if horizontal {
                    &tiles.ring_x
                } else {
                    &tiles.ring_y
                };
                dashed_strip(p, tile, horizontal, at, len, t, ink);
            }
            None if horizontal => strip(p, at.0, at.1, len, t, ink),
            None => strip(p, at.0, at.1, t, len, ink),
        }
    }
    // Bevy caps a corner's radius at half the node's shorter side, so each arc is the round
    // corner of a node twice the radius across, clipped to the quarter that holds the arc.
    let (r, d) = (ring.radius, 2.0 * ring.radius);
    let arcs = [
        (
            (0.0, 0.0),
            UiRect::new(px(t), px(0), px(t), px(0)),
            BorderRadius::top_left(px(r)),
        ),
        (
            (-r, 0.0),
            UiRect::new(px(0), px(t), px(t), px(0)),
            BorderRadius::top_right(px(r)),
        ),
        (
            (0.0, -r),
            UiRect::new(px(t), px(0), px(0), px(t)),
            BorderRadius::bottom_left(px(r)),
        ),
        (
            (-r, -r),
            UiRect::new(px(0), px(t), px(0), px(t)),
            BorderRadius::bottom_right(px(r)),
        ),
    ];
    for (c, ((dx, dy), border, border_radius)) in ring.corners.iter().zip(arcs) {
        p.spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(c.x),
                top: px(c.y),
                width: px(c.w),
                height: px(c.h),
                overflow: Overflow::clip(),
                ..default()
            },
            ignore(),
            LinePiece,
        ))
        .with_children(|q| {
            q.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: px(dx),
                    top: px(dy),
                    width: px(d),
                    height: px(d),
                    border,
                    border_radius,
                    ..default()
                },
                BorderColor::default(),
                Stroke(ink),
                ignore(),
            ));
        });
    }
}

fn spawn_canvas(
    c: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    theme: &Theme,
    dots: Option<&Dots>,
    ctx: &Ctx,
    spec: &DiagramSpec,
    g: &Geometry,
) {
    let canvas = ctx.canvas;
    if let Some(dots) = dots {
        c.spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                width: percent(100),
                height: percent(100),
                ..default()
            },
            ImageNode {
                image: dots.dot.clone(),
                color: theme.tokens.canvas_dot,
                image_mode: NodeImageMode::Tiled {
                    tile_x: true,
                    tile_y: true,
                    stretch_value: 1.0,
                },
                ..default()
            },
            Tint(Swatch::CanvasDot),
            ignore(),
            CanvasDots,
        ));
    }
    c.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(0),
            top: px(0),
            width: percent(100),
            height: percent(100),
            overflow: Overflow::clip(),
            ..default()
        },
        Viewport(canvas),
        observe(
            move |scrolled: On<Pointer<Scroll>>,
                  keys: Res<ButtonInput<KeyCode>>,
                  viewports: Query<(&Viewport, &ComputedNode, &UiGlobalTransform)>,
                  mut states: Query<&mut CanvasState>| {
                let Ok(mut state) = states.get_mut(canvas) else {
                    return;
                };
                let line = scrolled.event.unit == MouseScrollUnit::Line;
                let cursor = viewport_origin(canvas, &viewports)
                    .map_or(Vec2::ZERO, |o| scrolled.pointer_location.position - o);
                scroll(
                    &mut state,
                    cursor,
                    command_held(&keys),
                    wheel_pixels(line, scrolled.event.x),
                    wheel_pixels(line, scrolled.event.y),
                );
            },
        ),
        observe(
            move |dragged: On<Pointer<Drag>>,
                  time: Res<Time>,
                  mut states: Query<&mut CanvasState>| {
                if dragged.event.button != PointerButton::Primary {
                    return;
                }
                if let Ok(mut state) = states.get_mut(canvas) {
                    drag(
                        &mut state,
                        dragged.event.delta,
                        dragged.event.distance,
                        time.elapsed_secs_f64(),
                    );
                }
            },
        ),
        observe(
            move |ended: On<Pointer<DragEnd>>,
                  frame: Res<FrameCount>,
                  mut states: Query<&mut CanvasState>| {
                if ended.event.button == PointerButton::Primary
                    && let Ok(mut state) = states.get_mut(canvas)
                {
                    end_drag(&mut state, frame.0);
                }
            },
        ),
        observe(
            move |pressed: On<Pointer<Press>>, mut states: Query<&mut CanvasState>| {
                if pressed.event.button != PointerButton::Primary {
                    return;
                }
                if let Ok(mut state) = states.get_mut(canvas) {
                    press(&mut state);
                }
            },
        ),
    ))
    .with_children(|v| {
        v.spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                width: px(g.size.0),
                height: px(g.size.1),
                ..default()
            },
            UiTransform::IDENTITY,
            // Shown once `apply_view` has fitted it, so it never flashes at 100 %.
            Visibility::Hidden,
            ignore(),
            ContentNode(canvas),
        ))
        .with_children(|content| spawn_content(content, fonts, ctx, spec, g));
    });
    header(c, fonts, spec, g.dropped_texts);
    zoom_control(c, fonts, canvas);
    legend(c, fonts, ctx.tiles.as_ref(), ctx.compare);
}

fn spawn_content(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    ctx: &Ctx,
    spec: &DiagramSpec,
    g: &Geometry,
) {
    let compare = ctx.compare;
    for group in &g.groups {
        let (border, fill) = (Swatch::Line, Swatch::Clear);
        p.spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(group.rect.x),
                top: px(group.rect.y),
                width: px(group.rect.w),
                height: px(group.rect.h),
                border: px(1.5).all(),
                border_radius: BorderRadius::all(px(12)),
                ..default()
            },
            BackgroundColor::default(),
            Fill(fill),
            BorderColor::default(),
            Stroke(border),
            ignore(),
        ))
        .with_children(|f| {
            text_block(
                f,
                fonts,
                &group.label,
                Swatch::Muted,
                false,
                Vec2::new(group.rect.x + 1.5, group.rect.y + 1.5),
            );
        });
    }
    for lifeline in &g.lifelines {
        strip(
            p,
            lifeline.x - 1.0,
            lifeline.top,
            LINE,
            lifeline.bottom - lifeline.top,
            Swatch::Line,
        );
    }
    for activation in &g.activations {
        let r = activation.rect;
        p.spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(r.x),
                top: px(r.y),
                width: px(r.w),
                height: px(r.h),
                border: px(RING).all(),
                border_radius: BorderRadius::all(px(2)),
                ..default()
            },
            BackgroundColor::default(),
            Fill(Swatch::ChangedBg),
            BorderColor::default(),
            Stroke(Swatch::Green),
            ignore(),
        ));
    }
    for edge in &g.edges {
        draw_edge(p, fonts, ctx.tiles.as_ref(), edge, compare);
    }
    for message in &g.messages {
        let look = line_look(
            message.change,
            message.history,
            compare,
            message.reply,
            message.reply,
        );
        let (from, to) = (
            Point {
                x: message.from_x,
                y: message.y,
            },
            Point {
                x: message.to_x,
                y: message.y,
            },
        );
        segment(
            p,
            ctx.tiles.as_ref(),
            (from, to),
            (false, false),
            look.ink,
            look.dashed,
        );
        if (message.to_x - message.from_x).abs() > 0.5 {
            head(p, fonts, to, direction(from, to), look.ink);
        }
        text_block(
            p,
            fonts,
            &message.label,
            look.label,
            look.strike,
            Vec2::ZERO,
        );
    }
    // The dashed rings go with the lines, under every box.
    for b in &g.boxes {
        let look = look(b.shape, b.change, b.history, compare);
        if look.dashed {
            let radius = if matches!(b.shape, Shape::Pill | Shape::Circle | Shape::Start) {
                b.rect.w.min(b.rect.h) / 2.0
            } else {
                BOX_RADIUS
            };
            dashed_ring(p, ctx.tiles.as_ref(), b.rect, radius, RING_GAP, look.ring);
        }
    }
    for i in reading_order(&g.boxes) {
        draw_box(p, fonts, ctx, spec, &g.boxes[i]);
    }
}

fn draw_edge(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    tiles: Option<&Dots>,
    e: &EdgeGeo,
    compare: bool,
) {
    let look = line_look(
        e.change,
        e.history,
        compare,
        e.style == LineStyle::Dashed,
        false,
    );
    let pieces = e.points.len().saturating_sub(1);
    for (i, pair) in e.points.windows(2).enumerate() {
        let grow = (i > 0, i + 1 < pieces);
        segment(p, tiles, (pair[0], pair[1]), grow, look.ink, look.dashed);
    }
    if let [.., before, last] = e.points[..]
        && matches!(e.arrow, Arrow::End | Arrow::Both)
    {
        head(p, fonts, last, direction(before, last), look.ink);
    }
    if let [first, second, ..] = e.points[..]
        && e.arrow == Arrow::Both
    {
        head(p, fonts, first, direction(second, first), look.ink);
    }
    if let Some(label) = &e.label {
        text_block(p, fonts, label, look.label, look.strike, Vec2::ZERO);
    }
    for end in e.ends.iter().flatten() {
        text_block(p, fonts, end, Swatch::Muted, false, Vec2::ZERO);
    }
}

fn draw_box(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    ctx: &Ctx,
    spec: &DiagramSpec,
    b: &BoxGeo,
) {
    let canvas = ctx.canvas;
    let look = look(b.shape, b.change, b.history, ctx.compare);
    let node = spec
        .nodes
        .get(b.index)
        .map_or(b.node.clone(), |n| n.id.clone());
    let round = matches!(b.shape, Shape::Pill | Shape::Circle | Shape::Start);
    p.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(b.rect.x),
            top: px(b.rect.y),
            width: px(b.rect.w),
            height: px(b.rect.h),
            border: px(look.ring_w).all(),
            border_radius: if round {
                BorderRadius::MAX
            } else {
                BorderRadius::all(px(BOX_RADIUS))
            },
            ..default()
        },
        BackgroundColor::default(),
        Fill(look.fill),
        BorderColor::default(),
        Stroke(look.ring),
        Outline::new(px(2), px(3), Color::NONE),
        WidgetButton,
        Clickable,
        Hovered::default(),
        TabIndex(0),
        DiagramBox {
            canvas: ctx.canvas,
            pr: ctx.pr.clone(),
            number: ctx.number,
            key: ctx.key.clone(),
            node,
            rect: b.rect,
            anchored: b.anchored,
        },
        observe(on_box),
        // A press on a box does not bubble to the viewport, so the box clears the last drag itself.
        observe(
            move |pressed: On<Pointer<Press>>, mut states: Query<&mut CanvasState>| {
                if pressed.event.button == PointerButton::Primary
                    && let Ok(mut state) = states.get_mut(canvas)
                {
                    press(&mut state);
                }
            },
        ),
    ))
    .with_children(|bx| {
        let origin = Vec2::new(b.rect.x + look.ring_w, b.rect.y + look.ring_w);
        text_block(bx, fonts, &b.title, look.title, look.strike, origin);
        if let Some(sub) = &b.sub {
            text_block(bx, fonts, sub, look.sub, look.strike, origin);
        }
        for row in &b.rows {
            if row.divider_above {
                bx.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(row.rect.x - origin.x),
                        top: px(row.rect.y - origin.y),
                        width: px(row.rect.w),
                        height: px(1),
                        ..default()
                    },
                    BackgroundColor::default(),
                    Fill(Swatch::Line),
                    ignore(),
                ));
            }
            let ink = match row.change {
                Some(Change::New | Change::Changed) => Swatch::Green,
                Some(Change::Risk) => Swatch::Orange,
                _ => Swatch::Muted,
            };
            text_block(bx, fonts, &row.text, ink, false, origin);
            if let Some(key) = row.key {
                let tag = match key {
                    RowKey::Pk => "PK",
                    RowKey::Fk => "FK",
                };
                bx.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(key_left(&row.text) - origin.x),
                        top: px(row.text.at.y - origin.y),
                        ..default()
                    },
                    text(fonts, tag, type_of(row.text.style, ink)),
                    TextLayout::new(Justify::Left, LineBreak::NoWrap),
                    ignore(),
                ));
            }
        }
    });
}

/// `3 labels not shown`: the edge texts the layout found no free place for.
fn hidden_note(dropped: u32) -> Option<String> {
    match dropped {
        0 => None,
        1 => Some("1 label not shown".into()),
        n => Some(format!("{n} labels not shown")),
    }
}

fn header(c: &mut ChildSpawnerCommands, fonts: &UiFonts, spec: &DiagramSpec, dropped: u32) {
    c.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(20),
            top: px(16),
            column_gap: px(10),
            align_items: AlignItems::Center,
            ..default()
        },
        ignore(),
    ))
    .with_children(|h| {
        h.spawn((text(fonts, spec.title.clone(), Type::STRONG), ignore()));
        h.spawn((
            Node {
                height: px(20),
                padding: UiRect::horizontal(px(8)),
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(10)),
                ..default()
            },
            BackgroundColor::default(),
            Fill(Swatch::Chrome),
            ignore(),
        ))
        .with_children(|chip| {
            chip.spawn((
                text(fonts, spec.kind.label(), Type::MUTED.size(11.0)),
                ignore(),
            ));
        });
        h.spawn((
            text(fonts, spec.caption.clone(), Type::MUTED.ink(Swatch::Faint)),
            ignore(),
        ));
        if let Some(note) = hidden_note(dropped) {
            h.spawn((
                text(fonts, format!("· {note}"), Type::MUTED.ink(Swatch::Faint)),
                ignore(),
            ));
        }
    });
}

fn zoom_control(c: &mut ChildSpawnerCommands, fonts: &UiFonts, canvas: Entity) {
    let button = |label: &'static str, factor: f32| {
        (
            Node {
                width: px(30),
                height: px(28),
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
            ZoomButton { canvas, factor },
            observe(on_zoom),
            children![text(fonts, label, Type::MUTED)],
        )
    };
    c.spawn((
        Node {
            position_type: PositionType::Absolute,
            right: px(16),
            top: px(12),
            column_gap: px(4),
            align_items: AlignItems::Center,
            border: px(1).all(),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Surface),
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|z| {
        z.spawn(button("−", 1.0 / BUTTON_STEP));
        z.spawn((
            text(fonts, "100%", Type::MUTED.size(12.0)),
            ZoomLabel(canvas),
            ignore(),
        ));
        z.spawn(button("+", BUTTON_STEP));
    });
}

/// One swatch of the legend: filled and ringed, or dashed.
fn legend_item(
    row: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    tiles: Option<&Dots>,
    label: &str,
    fill: Swatch,
    ring: Swatch,
    dashed: bool,
) {
    row.spawn((
        Node {
            column_gap: px(6),
            align_items: AlignItems::Center,
            ..default()
        },
        ignore(),
    ))
    .with_children(|item| {
        item.spawn((
            Node {
                width: px(12),
                height: px(12),
                border: if dashed { UiRect::ZERO } else { px(1.5).all() },
                border_radius: BorderRadius::all(px(3)),
                ..default()
            },
            BackgroundColor::default(),
            Fill(fill),
            BorderColor::default(),
            Stroke(ring),
            ignore(),
        ))
        .with_children(|icon| {
            if dashed {
                dashed_ring(
                    icon,
                    tiles,
                    GRect {
                        x: 0.0,
                        y: 0.0,
                        w: 12.0,
                        h: 12.0,
                    },
                    3.0,
                    0.0,
                    ring,
                );
            }
        });
        item.spawn((text(fonts, label, Type::MUTED.size(12.0)), ignore()));
    });
}

fn legend_row() -> Node {
    Node {
        column_gap: px(16),
        ..default()
    }
}

fn legend(c: &mut ChildSpawnerCommands, fonts: &UiFonts, tiles: Option<&Dots>, compare: bool) {
    c.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(20),
            bottom: px(16),
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            padding: UiRect::axes(px(12), px(8)),
            border: px(1).all(),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Surface),
        BorderColor::default(),
        Stroke(Swatch::Line),
        ignore(),
        Legend,
    ))
    .with_children(|l| {
        l.spawn((legend_row(), ignore())).with_children(|r| {
            legend_item(
                r,
                fonts,
                tiles,
                "Changed",
                Swatch::ChangedBg,
                Swatch::Green,
                false,
            );
            legend_item(r, fonts, tiles, "New", Swatch::Clear, Swatch::Green, true);
            legend_item(
                r,
                fonts,
                tiles,
                "Risk",
                Swatch::RiskBg,
                Swatch::Orange,
                false,
            );
            legend_item(
                r,
                fonts,
                tiles,
                "Unchanged",
                Swatch::Clear,
                Swatch::Line,
                false,
            );
        });
        if compare {
            l.spawn((legend_row(), ignore())).with_children(|r| {
                legend_item(r, fonts, tiles, "Added", Swatch::Clear, Swatch::Green, true);
                legend_item(
                    r,
                    fonts,
                    tiles,
                    "Changed",
                    Swatch::AmberSoft,
                    Swatch::Amber,
                    false,
                );
                legend_item(
                    r,
                    fonts,
                    tiles,
                    "Removed",
                    Swatch::GhostFill,
                    Swatch::Line,
                    false,
                );
            });
        }
    });
}

// ---- observers and systems

/// A click on a box asks for its code: the tab remembers which box (a drag that ended on it
/// does not count).
fn on_box(
    activate: On<Activate>,
    boxes: Query<&DiagramBox>,
    mut states: Query<&mut CanvasState>,
    mut tabs: ResMut<ReviewTabs>,
    time: Res<Time>,
    frame: Res<FrameCount>,
) {
    let Ok(b) = boxes.get(activate.entity) else {
        return;
    };
    if let Ok(mut state) = states.get_mut(b.canvas)
        && swallows(&state, time.elapsed_secs_f64(), frame.0)
    {
        state.dragged = 0.0;
        return;
    }
    if let Some(tab) = tabs.0.get_mut(&b.pr) {
        tab.ui.diagram_node = Some((b.number, b.key.clone(), b.node.clone()));
    }
}

fn on_zoom(
    activate: On<Activate>,
    buttons: Query<&ZoomButton>,
    mut states: Query<&mut CanvasState>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Ok(mut state) = states.get_mut(b.canvas) {
        let centre = state.view.viewport / 2.0;
        state.view.zoom_at(centre, b.factor);
    }
}

/// Fits each new view once its viewport is known and writes every view into its transform.
fn apply_view(
    mut states: Query<&mut CanvasState>,
    mut contents: Query<(&ContentNode, &mut UiTransform, &mut Visibility)>,
    mut labels: Query<(&ZoomLabel, &mut Text)>,
) {
    for (content, mut transform, mut visibility) in &mut contents {
        let Ok(mut state) = states.get_mut(content.0) else {
            continue;
        };
        if !state.fitted && state.view.viewport.x > 0.0 {
            state.view.fit();
            state.fitted = true;
        }
        let t = state.view.translation();
        let want = UiTransform {
            translation: Val2::px(t.x, t.y),
            scale: Vec2::splat(state.view.zoom),
            rotation: Rot2::IDENTITY,
        };
        if *transform != want {
            *transform = want;
        }
        if state.fitted && *visibility != Visibility::Inherited {
            *visibility = Visibility::Inherited;
        }
    }
    for (label, mut text) in &mut labels {
        if let Ok(state) = states.get(label.0) {
            let percent = state.view.percent();
            let shown = text.0.strip_suffix('%').and_then(|n| n.parse::<u32>().ok());
            if shown != Some(percent) {
                text.0 = format!("{percent}%");
            }
        }
    }
}

/// Tells each view how big its viewport is, as laid out.
fn sync_viewport(
    viewports: Query<(&Viewport, &ComputedNode)>,
    mut states: Query<&mut CanvasState>,
) {
    for (viewport, node) in &viewports {
        let size = node.size * node.inverse_scale_factor;
        if size.x <= 0.0 || size.y <= 0.0 {
            continue;
        }
        if let Ok(mut state) = states.get_mut(viewport.0)
            && state.view.viewport != size
        {
            state.view.set_viewport(size);
        }
    }
}

/// The ring of a box that is hovered or has the keyboard focus.
fn outline_boxes(
    focus: Res<InputFocus>,
    theme: Res<Theme>,
    mut boxes: Query<(Entity, &Hovered, &mut Outline), With<DiagramBox>>,
) {
    for (entity, hovered, mut outline) in &mut boxes {
        let on = hovered.get() || focus.get() == Some(entity);
        let want = if on { theme.tokens.green } else { Color::NONE };
        if outline.color != want {
            outline.color = want;
        }
    }
}

/// Tab to a box that is off screen pans it into view.
fn reveal_focus(
    focus: Res<InputFocus>,
    boxes: Query<&DiagramBox>,
    mut states: Query<&mut CanvasState>,
) {
    if !focus.is_changed() {
        return;
    }
    let Some(b) = focus.get().and_then(|e| boxes.get(e).ok()) else {
        return;
    };
    if let Ok(mut state) = states.get_mut(b.canvas) {
        state.view.reveal(Rect::new(
            b.rect.x,
            b.rect.y,
            b.rect.right(),
            b.rect.bottom(),
        ));
    }
}

/// ⌘0 fits every canvas on screen.
fn fit_shortcut(keys: Res<ButtonInput<KeyCode>>, mut states: Query<&mut CanvasState>) {
    if command_held(&keys) && keys.just_pressed(KeyCode::Digit0) {
        for mut state in &mut states {
            state.view.fit();
        }
    }
}

/// A trackpad pinch zooms the canvas under the cursor.
fn pinch_zoom(
    mut pinches: MessageReader<PinchGesture>,
    windows: Query<&Window, With<PrimaryWindow>>,
    viewports: Query<(&Viewport, &ComputedNode, &UiGlobalTransform)>,
    mut states: Query<&mut CanvasState>,
) {
    let cursor = windows.iter().next().and_then(Window::cursor_position);
    for pinch in pinches.read() {
        let Some(cursor) = cursor else { continue };
        for (viewport, node, at) in &viewports {
            let s = node.inverse_scale_factor;
            let relative = cursor - (at.translation * s - node.size * s / 2.0);
            if relative.cmpge(Vec2::ZERO).all()
                && relative.cmple(node.size * s).all()
                && let Ok(mut state) = states.get_mut(viewport.0)
            {
                state.view.zoom_at(relative, pinch_factor(pinch.0));
            }
        }
    }
}

/// Tinted image nodes (the dots, the dashes) take their swatch when they appear and again
/// whenever the theme changes.
fn tint_images(theme: Res<Theme>, mut images: Query<(Ref<Tint>, &mut ImageNode)>) {
    for (tint, mut image) in &mut images {
        if theme.is_changed() || tint.is_added() {
            let want = theme.tokens.get(tint.0);
            if image.color != want {
                image.color = want;
            }
        }
    }
}

fn make_dots(mut commands: Commands, images: Option<ResMut<Assets<Image>>>) {
    if let Some(mut images) = images {
        commands.insert_resource(Dots {
            dot: images.add(dot_tile()),
            dash_x: images.add(dash_tile(true)),
            dash_y: images.add(dash_tile(false)),
            ring_x: images.add(ring_dash_tile(true)),
            ring_y: images.add(ring_dash_tile(false)),
        });
    }
}

pub(super) fn add_systems(app: &mut App) {
    app.add_message::<PinchGesture>()
        .add_systems(Startup, make_dots)
        .add_systems(
            Update,
            (
                outline_boxes,
                reveal_focus,
                fit_shortcut,
                pinch_zoom,
                tint_images,
            ),
        )
        .add_systems(
            PostUpdate,
            (
                apply_view.before(UiSystems::Layout),
                sync_viewport.after(UiSystems::PostLayout),
            ),
        );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{DiagramsTell, Tell};
    use crate::fixture;
    use crate::review_state::ReviewSection;
    use crate::testing::{self, NOW};
    use crate::theme::{DARK, LIGHT};
    use bevy::input_focus::FocusCause;
    use clusia_core::diagram::{DiagramKind, Edge, Node};
    use clusia_diagram::{Align, TextStyle};

    fn spec(key: &str) -> DiagramSpec {
        let version = fixture::demo_diagrams(NOW, &fixture::demo_pr()).remove(1);
        match version.entry(key).expect("an entry").state.clone() {
            EntryState::Ready { spec, .. } => spec,
            _ => panic!("ready"),
        }
    }

    fn node(id: &str, history: Option<History>) -> Node {
        Node {
            id: id.into(),
            label: id.into(),
            sub: None,
            shape: Shape::Box,
            rows: Vec::new(),
            divider_after: None,
            anchor: None,
            change: Change::Unchanged,
            note: None,
            history,
        }
    }

    fn edge(from: &str, to: &str, history: Option<History>) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            label: None,
            style: LineStyle::Solid,
            ends: [None, None],
            change: Change::Unchanged,
            history,
        }
    }

    fn block(x: f32, y: f32) -> TextLines {
        TextLines {
            lines: vec!["t".into()],
            style: TextStyle::Title,
            at: Point { x, y },
            width: 10.0,
            height: 16.0,
            align: Align::Left,
        }
    }

    fn at(id: &str, x: f32, y: f32) -> BoxGeo {
        BoxGeo {
            node: id.into(),
            index: 0,
            rect: GRect {
                x,
                y,
                w: 100.0,
                h: 40.0,
            },
            shape: Shape::Box,
            title: block(x, y),
            sub: None,
            rows: Vec::new(),
            change: Change::Unchanged,
            history: None,
            anchored: false,
        }
    }

    #[test]
    fn what_is_gone_is_not_drawn_unless_comparing() {
        let mut s = spec("flow");
        s.nodes.push(node("old", Some(History::Removed)));
        s.edges.push(edge("http", "old", Some(History::Same)));
        let plain = shown_spec(&s, false);
        assert!(plain.nodes.iter().all(|n| n.id != "old"));
        assert!(
            plain.edges.iter().all(|e| e.to != "old"),
            "a line to a removed box goes with it"
        );
        assert!(
            plain
                .edges
                .iter()
                .all(|e| e.history != Some(History::Removed)),
            "so does a removed line"
        );
        assert_eq!(plain.nodes.len(), s.nodes.len() - 1);
        let all = shown_spec(&s, true);
        assert_eq!(all, s, "a comparison draws everything");
    }

    #[test]
    fn a_removed_participant_takes_its_messages_and_group_seat() {
        let mut s = spec("sequence");
        s.nodes[1].history = Some(History::Removed);
        let gone = s.nodes[1].id.clone();
        let shown = shown_spec(&s, false);
        assert!(shown.nodes.iter().all(|n| n.id != gone));
        assert!(
            shown
                .messages
                .iter()
                .all(|m| m.from != gone && m.to != gone)
        );
        let mut u = spec("usecase");
        u.nodes[3].history = Some(History::Removed);
        let id = u.nodes[3].id.clone();
        let shown = shown_spec(&u, false);
        assert!(shown.groups[0].members.iter().all(|m| *m != id));
    }

    #[test]
    fn boxes_are_read_row_by_row_and_left_to_right() {
        let boxes = vec![
            at("c", 400.0, 100.0),
            at("a", 24.0, 24.0),
            at("d", 24.0, 220.0),
            at("b", 260.0, 30.0),
        ];
        let order: Vec<&str> = reading_order(&boxes)
            .into_iter()
            .map(|i| boxes[i].node.as_str())
            .collect();
        assert_eq!(order, ["a", "b", "c", "d"]);
        assert!(reading_order(&[]).is_empty());
    }

    #[test]
    fn the_dash_tile_is_five_on_and_three_off() {
        for horizontal in [true, false] {
            let tile = dash_tile(horizontal);
            let (w, h) = (tile.width(), tile.height());
            assert_eq!(if horizontal { (w, h) } else { (h, w) }, (8, 1));
            let data = tile.data.as_ref().expect("pixels");
            let alpha: Vec<u8> = data.chunks(4).map(|px| px[3]).collect();
            assert_eq!(alpha, [255, 255, 255, 255, 255, 0, 0, 0]);
        }
    }

    #[test]
    fn the_ring_dash_tile_is_three_on_and_two_off() {
        for horizontal in [true, false] {
            let tile = ring_dash_tile(horizontal);
            let (w, h) = (tile.width(), tile.height());
            assert_eq!(if horizontal { (w, h) } else { (h, w) }, (5, 1));
            let data = tile.data.as_ref().expect("pixels");
            let alpha: Vec<u8> = data.chunks(4).map(|px| px[3]).collect();
            assert_eq!(alpha, [255, 255, 255, 0, 0]);
        }
    }

    #[test]
    fn a_ring_hugs_its_box_with_dashed_sides_and_round_corners() {
        let rect = GRect {
            x: 10.0,
            y: 20.0,
            w: 100.0,
            h: 40.0,
        };
        let ring = ring_parts(rect, 8.0, 3.0);
        assert_eq!(ring.radius, 11.0, "the box's radius plus the gap");
        assert_eq!(
            ring.sides,
            vec![
                (true, (18.0, 17.0), 84.0),
                (true, (18.0, 62.0), 84.0),
                (false, (7.0, 28.0), 24.0),
                (false, (112.0, 28.0), 24.0),
            ],
            "each side stops where its corners start"
        );
        let corner = |x, y| GRect {
            x,
            y,
            w: 11.0,
            h: 11.0,
        };
        assert_eq!(
            ring.corners,
            [
                corner(7.0, 17.0),
                corner(102.0, 17.0),
                corner(7.0, 52.0),
                corner(102.0, 52.0)
            ]
        );
        let pill = ring_parts(rect, 20.0, 3.0);
        assert_eq!(pill.radius, 23.0);
        assert!(
            pill.sides.iter().all(|(horizontal, _, _)| *horizontal),
            "a pill's ends are all corner"
        );
    }

    #[test]
    fn a_centred_line_starts_half_its_spare_width_in() {
        let block = TextLines {
            lines: vec!["Fresh".into(), "a longer line".into()],
            style: TextStyle::Title,
            at: Point { x: 10.0, y: 0.0 },
            width: 200.0,
            height: 32.0,
            align: Align::Center,
        };
        for line in &block.lines {
            let spare = 200.0 - Fonts.width(line, TextStyle::Title);
            assert!((line_left(&block, line) - (10.0 + spare / 2.0)).abs() < 1e-3);
        }
        let left = TextLines {
            align: Align::Left,
            ..block.clone()
        };
        assert_eq!(line_left(&left, "Fresh"), 10.0);
    }

    #[test]
    fn a_key_tag_follows_its_row_name() {
        let name = TextLines {
            lines: vec!["token_id".into()],
            style: TextStyle::RowMono,
            at: Point { x: 10.0, y: 0.0 },
            width: 206.0,
            height: 14.0,
            align: Align::Left,
        };
        let after = 10.0 + Fonts.width("token_id", TextStyle::RowMono) + KEY_GAP;
        assert!((key_left(&name) - after).abs() < 1e-3);
    }

    #[test]
    fn an_elbow_fills_its_outer_corner() {
        let p = |x, y| Point { x, y };
        let (corner, down) = (p(100.0, 50.0), p(100.0, 120.0));
        let (r, h) = strip_rect(p(0.0, 50.0), corner, false, true);
        assert!(h);
        assert_eq!((r.x, r.right(), r.y, r.bottom()), (0.0, 101.0, 49.0, 51.0));
        let (r, h) = strip_rect(corner, down, true, false);
        assert!(!h);
        assert_eq!(
            (r.y, r.bottom(), r.x, r.right()),
            (49.0, 120.0, 99.0, 101.0)
        );
        // The same piece drawn the other way round is the same rectangle.
        assert_eq!(
            strip_rect(down, corner, false, true).0,
            strip_rect(corner, down, true, false).0
        );
        let (r, _) = strip_rect(corner, p(0.0, 50.0), true, false);
        assert_eq!((r.x, r.right()), (0.0, 101.0), "growing at the bend only");
    }

    #[test]
    fn an_arrowhead_sits_with_its_tip_on_the_end_of_the_line() {
        let tip = Point { x: 100.0, y: 50.0 };
        let (r, g) = arrow_head(tip, Dir::Right);
        assert_eq!((g, r.right(), r.y + r.h / 2.0), ("▶", 100.0, 50.0));
        let (r, g) = arrow_head(tip, Dir::Left);
        assert_eq!((g, r.x, r.y + r.h / 2.0), ("◀", 100.0, 50.0));
        let (r, g) = arrow_head(tip, Dir::Down);
        assert_eq!((g, r.bottom(), r.x + r.w / 2.0), ("▼", 50.0, 100.0));
        let (r, g) = arrow_head(tip, Dir::Up);
        assert_eq!((g, r.y, r.x + r.w / 2.0), ("▲", 50.0, 100.0));
        let (a, b) = (Point { x: 0.0, y: 0.0 }, Point { x: 10.0, y: 3.0 });
        assert_eq!(direction(a, b), Dir::Right);
        assert_eq!(direction(b, a), Dir::Left);
        assert_eq!(direction(a, Point { x: 1.0, y: 9.0 }), Dir::Down);
        assert_eq!(direction(Point { x: 1.0, y: 9.0 }, a), Dir::Up);
    }

    #[test]
    fn a_box_looks_by_what_changed_or_by_history_when_comparing() {
        let u = Change::Unchanged;
        let l = look(Shape::Box, Change::Changed, None, false);
        assert_eq!(
            (l.fill, l.ring, l.ring_w, l.dashed),
            (Swatch::ChangedBg, Swatch::Green, 1.5, false)
        );
        assert_eq!(l.sub, Swatch::Green);
        let l = look(Shape::Box, Change::New, None, false);
        assert_eq!(
            (l.fill, l.ring, l.dashed),
            (Swatch::Bg, Swatch::Green, true)
        );
        let l = look(Shape::Box, Change::Risk, None, false);
        assert_eq!(
            (l.fill, l.ring, l.sub),
            (Swatch::RiskBg, Swatch::Orange, Swatch::Orange)
        );
        let l = look(Shape::Box, u, None, false);
        assert_eq!((l.fill, l.ring, l.ring_w), (Swatch::Bg, Swatch::Line, 1.0));
        assert_eq!(look(Shape::Store, u, None, false).fill, Swatch::Chrome);
        assert_eq!(look(Shape::Actor, u, None, false).ring, Swatch::Edge);
        assert_eq!(look(Shape::External, u, None, false).fill, Swatch::Surface);
        let dot = look(Shape::Start, Change::Risk, None, false);
        assert_eq!((dot.fill, dot.ring_w), (Swatch::Fg, 0.0));
        // Comparing: the change is ignored, the history decides.
        let l = look(Shape::Box, Change::Risk, Some(History::Same), true);
        assert_eq!((l.fill, l.ring), (Swatch::Bg, Swatch::Line));
        let l = look(Shape::Box, Change::Unchanged, Some(History::Added), true);
        assert_eq!((l.ring, l.dashed), (Swatch::Green, true));
        let l = look(Shape::Box, Change::Unchanged, Some(History::Changed), true);
        assert_eq!(
            (l.fill, l.ring, l.sub),
            (Swatch::AmberSoft, Swatch::Amber, Swatch::Amber)
        );
        let l = look(Shape::Box, Change::Changed, Some(History::Removed), true);
        assert_eq!(
            (l.fill, l.title, l.strike),
            (Swatch::GhostFill, Swatch::GhostInk, true)
        );
        assert_eq!(
            look(Shape::Box, Change::Changed, None, true).fill,
            Swatch::Bg
        );
    }

    #[test]
    fn a_line_looks_by_what_changed_or_by_history_when_comparing() {
        let plain = line_look(Change::Unchanged, None, false, false, false);
        assert_eq!(
            (plain.ink, plain.dashed, plain.label),
            (Swatch::Edge, false, Swatch::Faint)
        );
        assert_eq!(
            line_look(Change::New, None, false, false, false).ink,
            Swatch::Green
        );
        assert_eq!(
            line_look(Change::Risk, None, false, false, false).label,
            Swatch::Orange
        );
        assert!(line_look(Change::Unchanged, None, false, true, false).dashed);
        let reply = line_look(Change::Unchanged, None, false, true, true);
        assert_eq!((reply.ink, reply.dashed), (Swatch::EdgeSoft, true));
        let added = line_look(Change::Unchanged, Some(History::Added), true, false, false);
        assert_eq!((added.ink, added.dashed), (Swatch::Green, true));
        let changed = line_look(
            Change::Unchanged,
            Some(History::Changed),
            true,
            false,
            false,
        );
        assert_eq!(changed.ink, Swatch::Amber);
        let removed = line_look(
            Change::Unchanged,
            Some(History::Removed),
            true,
            false,
            false,
        );
        assert_eq!(
            (removed.ink, removed.dashed, removed.strike),
            (Swatch::EdgeSoft, true, true)
        );
        assert_eq!(
            line_look(Change::Risk, Some(History::Same), true, false, false).ink,
            Swatch::Edge,
            "a comparison shows only what changed since"
        );
    }

    #[test]
    fn the_dot_tile_is_one_centred_two_pixel_dot() {
        let tile = dot_tile();
        assert_eq!((tile.width(), tile.height()), (20, 20));
        let data = tile.data.as_ref().expect("pixels");
        let lit: Vec<usize> = data
            .chunks(4)
            .enumerate()
            .filter(|(_, px)| px[3] != 0)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(lit, [9 * 20 + 9, 9 * 20 + 10, 10 * 20 + 9, 10 * 20 + 10]);
    }

    #[test]
    fn dragging_and_scrolling_move_the_view() {
        let mut view = CanvasView::new(Vec2::new(800.0, 500.0));
        view.set_viewport(Vec2::new(1000.0, 600.0));
        let mut state = CanvasState {
            view,
            fitted: true,
            dragged: 0.0,
            drag_at: 0.0,
            dragging: false,
            drag_end_frame: None,
            built: None,
        };
        drag(&mut state, Vec2::new(10.0, 5.0), Vec2::new(10.0, 5.0), 3.0);
        assert_eq!(state.view.pan, Vec2::new(10.0, 5.0));
        assert!(state.dragged > DRAG_SLOP);
        assert!(state.dragging);
        assert!(swallows(&state, 3.0 + DRAG_GRACE / 2.0, 9));
        assert!(
            swallows(&state, 3.0 + DRAG_GRACE * 2.0, 9),
            "a drag that is still going on swallows, however long it paused"
        );
        end_drag(&mut state, 9);
        assert!(swallows(&state, 30.0, 9), "released in this frame");
        assert!(
            !swallows(&state, 30.0, 10),
            "a pan that ended in an earlier frame swallows nothing"
        );
        press(&mut state);
        assert!(!state.dragging && state.drag_end_frame.is_none());
        drag(&mut state, Vec2::ZERO, Vec2::new(10.0, 5.0), 3.0);
        end_drag(&mut state, 9);
        assert!(!swallows(&state, 3.0 + DRAG_GRACE * 2.0, 10));
        drag(&mut state, Vec2::new(1.0, 0.0), Vec2::new(2.0, 1.0), 3.1);
        assert!(state.dragged > 11.0, "the longest distance is kept");
        assert_eq!(state.drag_at, 3.1);
        press(&mut state);
        assert_eq!(state.dragged, 0.0);
        scroll(&mut state, Vec2::ZERO, false, 0.0, 30.0);
        assert_eq!(state.view.pan, Vec2::new(11.0, 35.0));
        let before = state.view.zoom;
        scroll(&mut state, Vec2::new(500.0, 300.0), true, 0.0, 100.0);
        assert!(state.view.zoom > before, "⌘ + scroll up zooms in");
        scroll(&mut state, Vec2::new(500.0, 300.0), true, 0.0, -10_000.0);
        assert_eq!(
            state.view.zoom,
            crate::screens::review::diagrams::view::ZOOM_MIN
        );
    }

    // ---- with the demo drawings on screen

    fn open(app: &mut App) -> (PrRef, Entity) {
        let pr = testing::open_ready(app, false);
        testing::open_diagrams(app, &pr);
        testing::set_section(app, &pr, ReviewSection::Diagrams);
        let canvas = testing::find::<DiagramCanvas>(app, |_| true);
        (pr, canvas)
    }

    fn state_of(app: &App, canvas: Entity) -> &CanvasState {
        app.world()
            .get::<CanvasState>(canvas)
            .expect("a canvas state")
    }

    fn size_the_viewport(app: &mut App, canvas: Entity, size: Vec2) {
        app.world_mut()
            .get_mut::<CanvasState>(canvas)
            .expect("a canvas state")
            .view
            .set_viewport(size);
        testing::settle(app);
    }

    fn texts(app: &mut App) -> Vec<String> {
        testing::shown_text(app)
    }

    fn box_entity(app: &mut App, node: &str) -> Entity {
        testing::find::<DiagramBox>(app, |b| b.node == node)
    }

    #[test]
    fn the_canvas_draws_the_boxes_the_header_the_zoom_and_the_legend() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, canvas) = open(&mut app);
        assert_eq!(
            testing::count::<DiagramBox>(&mut app),
            5,
            "the flow has five boxes"
        );
        let shown = texts(&mut app);
        for needle in [
            "Token refresh flow",
            "Flow",
            "Click a box to comment on its code",
            "TokenStore::refresh",
            "100%",
            "Changed",
            "New",
            "Risk",
            "Unchanged",
        ] {
            assert!(
                shown.iter().any(|t| t == needle),
                "shows {needle}: {shown:?}"
            );
        }
        assert!(
            !shown.iter().any(|t| t == "Removed"),
            "no comparison, no second legend row"
        );
        assert_eq!(testing::count::<Legend>(&mut app), 1);
        assert_eq!(testing::count::<ZoomButton>(&mut app), 2);
        assert_eq!(testing::count::<CanvasDots>(&mut app), 1);
        let dots = testing::find::<CanvasDots>(&mut app, |_| true);
        let node = app.world().get::<ImageNode>(dots).unwrap();
        assert!(matches!(node.image_mode, NodeImageMode::Tiled { .. }));
        assert!(state_of(&app, canvas).built.is_some());
    }

    #[test]
    fn the_boxes_are_spawned_in_reading_order_and_are_the_only_buttons_of_the_diagram() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, canvas) = open(&mut app);
        let content = testing::find::<ContentNode>(&mut app, |c| c.0 == canvas);
        let children: Vec<Entity> = app
            .world()
            .get::<Children>(content)
            .expect("content children")
            .iter()
            .collect();
        let rects: Vec<GRect> = children
            .iter()
            .filter_map(|e| app.world().get::<DiagramBox>(*e))
            .map(|b| b.rect)
            .collect();
        assert_eq!(rects.len(), 5);
        let key = |r: &GRect| ((r.y / ROW_BAND).round() as i64, r.x);
        assert!(
            rects.windows(2).all(|w| {
                let (a, b) = (key(&w[0]), key(&w[1]));
                a.0 < b.0 || (a.0 == b.0 && a.1 <= b.1)
            }),
            "{rects:?}"
        );
        let last_piece = children
            .iter()
            .rposition(|e| app.world().get::<LinePiece>(*e).is_some())
            .expect("the diagram has lines");
        let first_box = children
            .iter()
            .position(|e| app.world().get::<DiagramBox>(*e).is_some())
            .expect("boxes");
        assert!(last_piece < first_box, "boxes are on top of the lines");
        for e in &children {
            let is_box = app.world().get::<DiagramBox>(*e).is_some();
            let pickable = app.world().get::<Pickable>(*e).copied();
            if is_box {
                assert_ne!(pickable, Some(Pickable::IGNORE));
                assert!(app.world().get::<TabIndex>(*e).is_some());
            } else {
                assert_eq!(
                    pickable,
                    Some(Pickable::IGNORE),
                    "only boxes take the pointer"
                );
            }
        }
        let content_pick = app.world().get::<Pickable>(content).copied();
        assert_eq!(content_pick, Some(Pickable::IGNORE));
    }

    #[test]
    fn the_content_waits_for_the_viewport_then_opens_fitted() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, canvas) = open(&mut app);
        let content = testing::find::<ContentNode>(&mut app, |c| c.0 == canvas);
        assert_eq!(
            app.world().get::<Visibility>(content),
            Some(&Visibility::Hidden),
            "not shown at 100 % before it is fitted"
        );
        size_the_viewport(&mut app, canvas, Vec2::new(1000.0, 600.0));
        let state = state_of(&app, canvas);
        assert!(state.fitted);
        assert!(state.view.zoom <= 1.0);
        let t = state.view.translation();
        let zoom = state.view.zoom;
        assert_eq!(
            app.world().get::<Visibility>(content),
            Some(&Visibility::Inherited)
        );
        let transform = app.world().get::<UiTransform>(content).unwrap();
        assert_eq!(transform.scale, Vec2::splat(zoom));
        assert_eq!(transform.translation, Val2::px(t.x, t.y));
    }

    #[test]
    fn the_zoom_buttons_zoom_about_the_centre_between_25_and_200_percent() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, canvas) = open(&mut app);
        size_the_viewport(&mut app, canvas, Vec2::new(3000.0, 2000.0));
        assert_eq!(state_of(&app, canvas).view.percent(), 100);
        let plus = testing::find::<ZoomButton>(&mut app, |b| b.factor > 1.0);
        let minus = testing::find::<ZoomButton>(&mut app, |b| b.factor < 1.0);
        testing::activate(&mut app, plus);
        assert_eq!(state_of(&app, canvas).view.percent(), 125);
        testing::settle(&mut app);
        assert!(texts(&mut app).iter().any(|t| t == "125%"));
        for _ in 0..10 {
            testing::activate(&mut app, plus);
        }
        assert_eq!(state_of(&app, canvas).view.percent(), 200);
        for _ in 0..20 {
            testing::activate(&mut app, minus);
        }
        assert_eq!(state_of(&app, canvas).view.percent(), 25);
        testing::settle(&mut app);
        assert!(texts(&mut app).iter().any(|t| t == "25%"));
    }

    #[test]
    fn command_zero_fits_again() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, canvas) = open(&mut app);
        size_the_viewport(&mut app, canvas, Vec2::new(1000.0, 600.0));
        let fitted = state_of(&app, canvas).view;
        let plus = testing::find::<ZoomButton>(&mut app, |b| b.factor > 1.0);
        testing::activate(&mut app, plus);
        assert_ne!(state_of(&app, canvas).view.zoom, fitted.zoom);
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.press(KeyCode::SuperLeft);
            keys.press(KeyCode::Digit0);
        }
        testing::settle(&mut app);
        assert_eq!(state_of(&app, canvas).view.zoom, fitted.zoom);
        assert_eq!(state_of(&app, canvas).view.pan, fitted.pan);
    }

    /// A pan of 12 px that ended `ago` seconds before now.
    fn panned(app: &mut App, canvas: Entity, ago: f64) {
        let now = app.world().resource::<Time>().elapsed_secs_f64();
        let mut state = app.world_mut().get_mut::<CanvasState>(canvas).unwrap();
        state.dragged = 12.0;
        state.drag_at = now - ago;
    }

    #[test]
    fn a_pan_that_ended_a_while_ago_swallows_neither_a_click_nor_enter() {
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, canvas) = open(&mut app);
        let http = box_entity(&mut app, "http");
        panned(&mut app, canvas, 5.0);
        // Enter and Space on a focused box arrive as the same `Activate` as a click.
        testing::activate(&mut app, http);
        assert_eq!(
            testing::tab(&app, &pr).ui.diagram_node,
            Some((2, "flow".into(), "http".into()))
        );
    }

    #[test]
    fn a_drag_that_paused_before_it_was_released_does_not_open_the_box() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::picking::pointer::{Location, PointerId};
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, canvas) = open(&mut app);
        let refresh = box_entity(&mut app, "refresh");
        // The pointer moved 12 px, rested for seconds and was released on the box: the click
        // that ends the drag arrives while the drag is still on.
        panned(&mut app, canvas, 5.0);
        app.world_mut()
            .get_mut::<CanvasState>(canvas)
            .unwrap()
            .dragging = true;
        testing::activate(&mut app, refresh);
        assert_eq!(testing::tab(&app, &pr).ui.diagram_node, None);
        // The release is seen after the click, in the same frame: the click that follows it is
        // swallowed too, and the next frame's is not.
        panned(&mut app, canvas, 5.0);
        let location = Location {
            target: NormalizedRenderTarget::None {
                width: 1,
                height: 1,
            },
            position: Vec2::ZERO,
        };
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location,
            DragEnd {
                button: PointerButton::Primary,
                distance: Vec2::new(12.0, 0.0),
            },
            refresh,
        ));
        assert!(!state_of(&app, canvas).dragging);
        let frame = app.world().resource::<FrameCount>().0;
        assert_eq!(state_of(&app, canvas).drag_end_frame, Some(frame));
        app.world_mut().trigger(Activate { entity: refresh });
        assert_eq!(testing::tab(&app, &pr).ui.diagram_node, None);
        panned(&mut app, canvas, 5.0);
        testing::settle(&mut app);
        testing::activate(&mut app, refresh);
        assert_eq!(
            testing::tab(&app, &pr).ui.diagram_node,
            Some((2, "flow".into(), "refresh".into())),
            "a later click opens it"
        );
    }

    #[test]
    fn a_press_on_a_box_clears_the_last_drag() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::picking::backend::HitData;
        use bevy::picking::pointer::{Location, PointerId};
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, canvas) = open(&mut app);
        let lock = box_entity(&mut app, "lock");
        panned(&mut app, canvas, 0.0);
        let location = Location {
            target: NormalizedRenderTarget::None {
                width: 1,
                height: 1,
            },
            position: Vec2::ZERO,
        };
        let press = |button| {
            Pointer::new(
                PointerId::Mouse,
                location.clone(),
                Press {
                    button,
                    hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
                    count: 1,
                },
                lock,
            )
        };
        app.world_mut().trigger(press(PointerButton::Secondary));
        assert!(
            state_of(&app, canvas).dragged > DRAG_SLOP,
            "only the primary button counts"
        );
        app.world_mut().trigger(press(PointerButton::Primary));
        assert_eq!(state_of(&app, canvas).dragged, 0.0);
        testing::activate(&mut app, lock);
        assert_eq!(
            testing::tab(&app, &pr).ui.diagram_node,
            Some((2, "flow".into(), "lock".into()))
        );
    }

    #[test]
    fn a_click_asks_for_the_box_and_a_drag_does_not() {
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, canvas) = open(&mut app);
        let refresh = box_entity(&mut app, "refresh");
        panned(&mut app, canvas, 0.0);
        testing::activate(&mut app, refresh);
        assert_eq!(
            testing::tab(&app, &pr).ui.diagram_node,
            None,
            "the drag swallowed it"
        );
        assert_eq!(state_of(&app, canvas).dragged, 0.0);
        testing::activate(&mut app, refresh);
        assert_eq!(
            testing::tab(&app, &pr).ui.diagram_node,
            Some((2, "flow".into(), "refresh".into()))
        );
    }

    #[test]
    fn a_dashed_edge_is_one_node_per_segment_however_long() {
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, _) = open(&mut app);
        let mut flow = spec("flow");
        for e in &mut flow.edges {
            e.style = LineStyle::Dashed;
        }
        let g = layout(&shown_spec(&flow, false), &Fonts);
        let segments: usize = g.edges.iter().map(|e| e.points.len() - 1).sum();
        let rings = g
            .boxes
            .iter()
            .filter(|b| look(b.shape, b.change, b.history, false).dashed)
            .count();
        testing::tell(
            &mut app,
            Tell::Diagrams(DiagramsTell::Entry {
                pr,
                number: 2,
                key: "flow".into(),
                state: EntryState::Ready {
                    spec: flow,
                    notes: Vec::new(),
                },
            }),
        );
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<DashedPiece>(&mut app),
            segments + 4 * rings + 4,
            "the lines, a ring for each new box and the legend's New swatch: a dash image is tiled by the node, not drawn one node a dash"
        );
        let mut q = app
            .world_mut()
            .query_filtered::<(&ImageNode, &Tint), With<DashedPiece>>();
        for (image, tint) in q.iter(app.world()) {
            assert!(matches!(image.image_mode, NodeImageMode::Tiled { .. }));
            assert_eq!(image.color, LIGHT.get(tint.0), "tinted by its swatch");
        }
    }

    #[test]
    fn a_small_wobble_still_clicks() {
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, canvas) = open(&mut app);
        app.world_mut()
            .get_mut::<CanvasState>(canvas)
            .unwrap()
            .dragged = DRAG_SLOP;
        let http = box_entity(&mut app, "http");
        testing::activate(&mut app, http);
        assert_eq!(
            testing::tab(&app, &pr).ui.diagram_node,
            Some((2, "flow".into(), "http".into()))
        );
    }

    #[test]
    fn a_box_with_a_code_link_says_so() {
        let mut app = testing::app(fixture::demo(NOW));
        let _ = open(&mut app);
        let anchored = |app: &mut App, node: &str| {
            let e = box_entity(app, node);
            app.world().get::<DiagramBox>(e).unwrap().anchored
        };
        assert!(anchored(&mut app, "refresh"));
        assert!(!anchored(&mut app, "oauth"));
    }

    #[test]
    fn hover_and_focus_ring_a_box() {
        let mut app = testing::app(fixture::demo(NOW));
        let _ = open(&mut app);
        let http = box_entity(&mut app, "http");
        let lock = box_entity(&mut app, "lock");
        let ring = |app: &App, e: Entity| app.world().get::<Outline>(e).unwrap().color;
        assert_eq!(ring(&app, http), Color::NONE);
        app.world_mut().entity_mut(http).insert(Hovered(true));
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(lock, FocusCause::Navigated);
        testing::settle(&mut app);
        assert_eq!(ring(&app, http), LIGHT.green);
        assert_eq!(ring(&app, lock), LIGHT.green);
        app.world_mut().entity_mut(http).insert(Hovered(false));
        app.world_mut().resource_mut::<InputFocus>().clear();
        testing::settle(&mut app);
        assert_eq!(ring(&app, http), Color::NONE);
        assert_eq!(ring(&app, lock), Color::NONE);
    }

    #[test]
    fn tab_to_a_box_that_is_off_screen_pans_it_into_view() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, canvas) = open(&mut app);
        size_the_viewport(&mut app, canvas, Vec2::new(400.0, 300.0));
        let target = box_entity(&mut app, "save");
        let rect = app.world().get::<DiagramBox>(target).unwrap().rect;
        {
            let mut s = app.world_mut().get_mut::<CanvasState>(canvas).unwrap();
            s.view.zoom = 1.0;
            s.view.pan = Vec2::ZERO;
        }
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(target, FocusCause::Navigated);
        testing::settle(&mut app);
        let view = state_of(&app, canvas).view;
        let (lo, hi) = (
            view.pan + Vec2::new(rect.x, rect.y) * view.zoom,
            view.pan + Vec2::new(rect.right(), rect.bottom()) * view.zoom,
        );
        assert!(lo.x >= 0.0 && lo.y >= 0.0, "{lo:?}");
        assert!(
            hi.x <= 400.0 && hi.y <= 300.0 || (rect.w > 400.0 || rect.h > 300.0),
            "{hi:?}"
        );
    }

    #[test]
    fn comparing_shows_what_changed_and_what_is_gone() {
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, canvas) = open(&mut app);
        let pieces = |app: &mut App| testing::count::<LinePiece>(app);
        let plain_pieces = pieces(&mut app);
        let refresh = box_entity(&mut app, "refresh");
        assert_eq!(
            app.world().get::<Fill>(refresh),
            Some(&Fill(Swatch::ChangedBg))
        );
        let dashed_before = app.world().resource::<ReviewTabs>().0[&pr]
            .ui
            .diagram_compare;
        assert!(!dashed_before);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .diagram_compare = true;
        testing::settle(&mut app);
        let canvas_now = testing::find::<DiagramCanvas>(&mut app, |_| true);
        assert!(
            state_of(&app, canvas_now).built.as_ref().unwrap().2,
            "built for the comparison"
        );
        let _ = canvas;
        let refresh = box_entity(&mut app, "refresh");
        assert_eq!(
            app.world().get::<Fill>(refresh),
            Some(&Fill(Swatch::AmberSoft))
        );
        assert_eq!(
            app.world().get::<Stroke>(refresh),
            Some(&Stroke(Swatch::Amber))
        );
        let lock = box_entity(&mut app, "lock");
        assert_eq!(
            app.world().get::<Stroke>(lock),
            Some(&Stroke(Swatch::Green))
        );
        let http = box_entity(&mut app, "http");
        assert_eq!(
            app.world().get::<Fill>(http),
            Some(&Fill(Swatch::Bg)),
            "unchanged is plain"
        );
        assert!(
            pieces(&mut app) > plain_pieces,
            "the removed arrow is drawn"
        );
        let shown = texts(&mut app);
        assert!(
            shown.iter().any(|t| t == "Removed"),
            "the legend gains its second row"
        );
        assert!(shown.iter().any(|t| t == "Added"));
        assert!(
            shown.iter().any(|t| t == "save()"),
            "the label of the removed arrow"
        );
    }

    #[test]
    fn the_theme_switch_while_comparing_recolours_the_boxes() {
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, _) = open(&mut app);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .diagram_compare = true;
        testing::settle(&mut app);
        let refresh = box_entity(&mut app, "refresh");
        assert_eq!(
            app.world().get::<BackgroundColor>(refresh).unwrap().0,
            LIGHT.amber_soft
        );
        *app.world_mut().resource_mut::<Theme>() =
            Theme::new(true, 13, clusia_core::Density::Comfortable);
        testing::settle(&mut app);
        let refresh = box_entity(&mut app, "refresh");
        assert_eq!(
            app.world().get::<BackgroundColor>(refresh).unwrap().0,
            DARK.amber_soft
        );
        assert_eq!(
            app.world().get::<BorderColor>(refresh).unwrap().top,
            DARK.amber
        );
        let dots = testing::find::<CanvasDots>(&mut app, |_| true);
        assert_eq!(
            app.world().get::<ImageNode>(dots).unwrap().color,
            DARK.canvas_dot
        );
        let mut q = app
            .world_mut()
            .query_filtered::<(&ImageNode, &Tint), With<DashedPiece>>();
        let dashes: Vec<_> = q.iter(app.world()).collect();
        assert!(!dashes.is_empty(), "a comparison draws dashes");
        for (image, tint) in dashes {
            assert_eq!(image.color, DARK.get(tint.0), "the dashes follow the theme");
        }
    }

    #[test]
    fn a_redraw_rebuilds_the_canvas_node_in_place_and_nothing_else_does() {
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, canvas) = open(&mut app);
        size_the_viewport(&mut app, canvas, Vec2::new(3000.0, 2000.0));
        let plus = testing::find::<ZoomButton>(&mut app, |b| b.factor > 1.0);
        testing::activate(&mut app, plus);
        testing::settle(&mut app);
        assert_eq!(
            state_of(&app, canvas).view.percent(),
            125,
            "nothing changed: nothing rebuilt"
        );
        let mut redrawn = spec("flow");
        redrawn.nodes.retain(|n| n.id != "lock");
        redrawn.edges.retain(|e| e.to != "lock");
        testing::tell(
            &mut app,
            Tell::Diagrams(DiagramsTell::Entry {
                pr,
                number: 2,
                key: "flow".into(),
                state: EntryState::Ready {
                    spec: redrawn,
                    notes: Vec::new(),
                },
            }),
        );
        testing::settle(&mut app);
        assert_eq!(testing::count::<DiagramBox>(&mut app), 4);
        let again = testing::find::<DiagramCanvas>(&mut app, |_| true);
        assert_eq!(again, canvas, "the canvas node stays");
        assert!(!state_of(&app, canvas).fitted || state_of(&app, canvas).view.percent() <= 100);
    }

    #[test]
    fn a_frame_where_nothing_changed_does_not_look_at_the_diagram() {
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, _) = open(&mut app);
        assert_eq!(testing::count::<DiagramBox>(&mut app), 5);
        {
            let mut diagrams = app.world_mut().resource_mut::<Diagrams>();
            let model = diagrams.bypass_change_detection().0.get_mut(&pr).unwrap();
            let entry = model
                .loaded
                .get_mut(&2)
                .and_then(|v| v.entries.iter_mut().find(|e| e.plan.key == "flow"))
                .unwrap();
            if let EntryState::Ready { spec, .. } = &mut entry.state {
                spec.nodes.retain(|n| n.id != "lock");
                spec.edges.retain(|e| e.to != "lock");
            }
        }
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<DiagramBox>(&mut app),
            5,
            "an unchanged frame compares nothing"
        );
        app.world_mut().resource_mut::<Diagrams>().set_changed();
        testing::settle(&mut app);
        assert_eq!(testing::count::<DiagramBox>(&mut app), 4);
    }

    #[test]
    fn no_demo_label_is_cut_by_a_route() {
        // Whether the axis-aligned run `a`–`b` passes through the inside of `r`.
        let cuts = |a: Point, b: Point, r: GRect| {
            a.x.max(b.x) > r.x
                && a.x.min(b.x) < r.right()
                && a.y.max(b.y) > r.y
                && a.y.min(b.y) < r.bottom()
        };
        for version in fixture::demo_diagrams(NOW, &fixture::demo_pr()) {
            for entry in &version.entries {
                let EntryState::Ready { spec, .. } = &entry.state else {
                    continue;
                };
                let g = clusia_diagram::layout(spec, &Fonts);
                for e in &g.edges {
                    for label in e.label.iter().chain(e.ends.iter().flatten()) {
                        let r = label.rect().inflate(-0.5);
                        for route in &g.edges {
                            assert!(
                                !route.points.windows(2).any(|w| cuts(w[0], w[1], r)),
                                "{}: {:?} is cut by the route of edge {}",
                                spec.key,
                                label.lines,
                                route.index
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_header_says_how_many_labels_had_no_room() {
        assert_eq!(hidden_note(0), None);
        assert_eq!(hidden_note(1).as_deref(), Some("1 label not shown"));
        assert_eq!(hidden_note(3).as_deref(), Some("3 labels not shown"));
        for version in fixture::demo_diagrams(NOW, &fixture::demo_pr()) {
            for entry in &version.entries {
                if let EntryState::Ready { spec, .. } = &entry.state {
                    let g = clusia_diagram::layout(spec, &Fonts);
                    assert_eq!(
                        g.dropped_texts, 0,
                        "the demo keeps every label: {}",
                        spec.key
                    );
                }
            }
        }
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, _) = open(&mut app);
        assert!(!texts(&mut app).iter().any(|t| t.contains("not shown")));
        let mut crowded = spec("flow");
        crowded.nodes = vec![node("hub", None)];
        crowded.edges.clear();
        for i in 0..20 {
            let id = format!("s{i}");
            crowded.nodes.push(node(&id, None));
            let mut e = edge("hub", &id, None);
            e.label = Some("calls".into());
            crowded.edges.push(e);
        }
        let hidden = clusia_diagram::layout(&crowded, &Fonts).dropped_texts;
        assert!(hidden > 0);
        testing::tell(
            &mut app,
            Tell::Diagrams(DiagramsTell::Entry {
                pr,
                number: 2,
                key: "flow".into(),
                state: EntryState::Ready {
                    spec: crowded,
                    notes: Vec::new(),
                },
            }),
        );
        testing::settle(&mut app);
        let want = hidden_note(hidden).expect("a note");
        assert!(texts(&mut app).iter().any(|t| t.contains(&want)), "{want}");
    }

    #[test]
    fn every_kind_draws_its_boxes() {
        let expected = [
            ("flow", 5),
            ("deps", 6),
            ("sequence", 5),
            ("er", 4),
            ("usecase", 10),
            ("dataflow", 5),
            ("states", 5),
            ("classes", 3),
        ];
        for (key, boxes) in expected {
            let mut app = testing::app(fixture::demo(NOW));
            let pr = testing::open_ready(&mut app, false);
            testing::open_diagrams(&mut app, &pr);
            app.world_mut()
                .resource_mut::<ReviewTabs>()
                .0
                .get_mut(&pr)
                .unwrap()
                .ui
                .diagram = Some(key.into());
            testing::set_section(&mut app, &pr, ReviewSection::Diagrams);
            assert_eq!(testing::count::<DiagramBox>(&mut app), boxes, "{key}");
        }
        assert_eq!(spec("flow").kind, DiagramKind::Flow);
    }

    #[test]
    fn the_sequence_has_a_lifeline_per_participant_and_a_line_per_message() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::open_diagrams(&mut app, &pr);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .diagram = Some("sequence".into());
        testing::set_section(&mut app, &pr, ReviewSection::Diagrams);
        let shown = texts(&mut app);
        assert!(shown.iter().any(|t| t == "exchange(refresh)"));
        assert!(
            shown
                .join(" ")
                .contains("Ok(same token) · no second exchange"),
            "a long label is wrapped into lines, each its own text: {shown:?}"
        );
        assert!(
            testing::count::<LinePiece>(&mut app) >= 5 + 8,
            "lifelines and messages"
        );
    }

    #[test]
    fn an_entity_table_tags_its_keys() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::open_diagrams(&mut app, &pr);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .diagram = Some("er".into());
        testing::set_section(&mut app, &pr, ReviewSection::Diagrams);
        let shown = texts(&mut app);
        assert_eq!(shown.iter().filter(|t| *t == "PK").count(), 3);
        assert_eq!(shown.iter().filter(|t| *t == "FK").count(), 2);
        assert!(shown.iter().any(|t| t == "0..N"));
    }
}
