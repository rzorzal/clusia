//! The Calls canvas: the call graph drawn top-down with the canvas pieces Diagrams draws with
//! (dotted ground, clipped viewport, zoom, pan, pinch), the level labels left of it, the header
//! with the breadcrumb and the Depth menu, the legend, and the expansion card of the selected
//! node. With no graph it shows the card of a file the call graph cannot read, or one line that
//! says why.
//!
//! What the canvas draws is rebuilt only when the graph, the files, or the text measure change:
//! the selection is in `TabUi`, so its ring (`outline_selected`) and its card (`fill_canvas`,
//! `place_card`) change without a rebuild, and the zoom and pan are kept.

use bevy::diagnostic::FrameCount;
use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::input_focus::InputFocus;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::events::{Click, Pointer, Press};
use bevy::picking::hover::Hovered;
use bevy::picking::pointer::PointerButton;
use bevy::prelude::*;
use bevy::text::{Justify, LineBreak};
use bevy::ui::ComputedNode;
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};
use clusia_calls::{CallGraph, NodeKind};
use clusia_core::PrRef;
use clusia_diagram::{BoxGeo, FaceMeasure, Geometry, Rect as GRect, layout_ranked};

use super::effective_depth;
use super::graph::{
    How, LABEL_W, PACKING, bands, breadcrumb, build, call_look, card_origin, pick, set_depth,
};
use super::list::{Tone, selected_file};
use super::model::{Calls, CallsModel};
use super::panel::{
    CardView, UnsupportedAction, UnsupportedWhat, card_view, on_unsupported, unsupported_title,
};
use crate::bridge::{Asks, Connection, Model, set_config};
use crate::fonts::UiFonts;
use crate::review_state::{Modal, ReviewTabs};
use crate::screens::review::diagrams::canvas::{
    BOX_RADIUS, CanvasState, DiagramBox, Dots, Pen, dashed_ring, draw_edge, ignore, legend_item,
    press, reading_order, spawn_ground, spawn_viewport, swallows, text_block, zoom_control,
};
use crate::screens::review::diagrams::modal::body_block;
use crate::screens::review::diagrams::view::CanvasView;
use crate::theme::{Swatch, Theme};
use crate::ui::kit::{
    self, Clickable, Fill, HoverFill, Stroke, Type, Variant, badge, button, card, text,
};

/// The zoom a graph that does not fit opens at.
const FIT_FLOOR: f32 = 0.5;
/// The expansion card's width.
const CARD_WIDTH: f32 = 340.0;
/// The size the card is placed with until it has been laid out.
const CARD_GUESS: Vec2 = Vec2::new(CARD_WIDTH, 220.0);
/// The breadcrumb's length before it drops its top.
const CRUMB_CHARS: usize = 44;
/// The height of the header strip.
const HEADER_H: f32 = 48.0;
/// The height of a level label's line, which centres it on its band.
const LEVEL_LABEL_H: f32 = 14.0;

/// The area of `pr`'s Calls view the graph is drawn in.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CallsCanvas(pub PrRef);

/// A box of the call graph (with its `DiagramBox`), so Diagrams' systems tell the two apart.
#[derive(Component, Debug, Clone, Copy)]
pub struct CallsMarker;

/// A level label (**UP 2**, **DOWN 1**): a sibling of the viewport, so the zoom does not scale
/// its text, kept at the left of the canvas level with the middle of its band (`mid`, in
/// content pixels).
#[derive(Component, Debug, Clone, Copy)]
pub struct LevelLabel {
    canvas: Entity,
    mid: f32,
}

/// The expansion card of the selected node: a sibling of the viewport, so the zoom does not
/// scale its text.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ExpansionCard(pub PrRef);

/// **Full view** on the card.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CardFull(pub PrRef);

/// **×** on the card.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CardClose(pub PrRef);

/// **Depth n ▼**.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DepthButton(pub PrRef);

/// A depth of the open Depth menu.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DepthItem {
    pub pr: PrRef,
    pub depth: u8,
}

/// The Depth button and its menu; rebuilt when the depth or the menu's state changes.
#[derive(Component)]
pub struct DepthPart {
    pub pr: PrRef,
    built: Option<(u8, bool)>,
}

/// What a canvas shows when there is no graph to draw.
#[derive(Debug, Clone, PartialEq)]
enum Blank {
    /// A file the call graph cannot read.
    File(String),
    /// One centred line; orange when it says why the index failed.
    Line(String, bool),
}

/// What a canvas was built from.
#[derive(Debug, Clone, PartialEq)]
struct Drawn {
    graph: Option<CallGraph>,
    blank: Option<Blank>,
    measure: FaceMeasure,
}

/// On the `CallsCanvas` entity: what it was built from and the card it shows.
#[derive(Component, Debug, Default)]
pub struct CallsState {
    drawn: Option<Drawn>,
    card: Option<CardView>,
}

/// The line the canvas says while it has nothing to draw.
fn blank_of(m: &CallsModel) -> Blank {
    use clusia_protocol::CallsIndexState;
    if let Some(why) = &m.failed {
        return Blank::Line(why.clone(), true);
    }
    let waiting = m.asking.is_some() || !m.asked || m.index == CallsIndexState::Building;
    Blank::Line(
        if waiting {
            "Reading the repository…".into()
        } else {
            "No functions changed".into()
        },
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn fill_canvas(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    calls: Res<Calls>,
    model: Res<Model>,
    fonts: Res<UiFonts>,
    theme: Res<Theme>,
    dots: Option<Res<Dots>>,
    mut canvases: Query<(
        Entity,
        &CallsCanvas,
        Option<&mut CallsState>,
        Option<&CanvasState>,
    )>,
    mut depths: Query<(Entity, &mut DepthPart)>,
    cards: Query<(Entity, &ExpansionCard)>,
) {
    let live = model.connection == Connection::Live;
    let empty = CallsModel::default();
    let config_depth = model.snapshot.config.diff.calls_depth;
    for (entity, canvas, state, old_view) in &mut canvases {
        let Some(tab) = tabs.0.get(&canvas.0) else {
            continue;
        };
        let m = calls.0.get(&canvas.0).unwrap_or(&empty);
        let measure = FaceMeasure::new(fonts.advances.clone(), theme.text_scale);
        let drawn = Drawn {
            graph: m.graph.clone(),
            blank: match (&m.graph, selected_file(&tab.ui, &m.unsupported)) {
                (Some(_), _) => None,
                (None, Some(file)) => Some(Blank::File(file)),
                (None, None) => Some(blank_of(m)),
            },
            measure,
        };
        let card = card_view(m, &tab.ui, live);
        let mut state = match state {
            Some(s) => s,
            None => {
                commands.entity(entity).insert(CallsState::default());
                continue;
            }
        };
        let rebuilt = state.drawn.as_ref() != Some(&drawn);
        if rebuilt {
            let pen = Pen::new(&fonts, drawn.measure.clone());
            commands.entity(entity).despawn_related::<Children>();
            match (&drawn.graph, &drawn.blank) {
                (Some(g), _) => {
                    let view = draw_graph(
                        &mut commands,
                        entity,
                        &canvas.0,
                        &pen,
                        &theme,
                        dots.as_deref(),
                        g,
                    );
                    let mut view = view;
                    if let Some(old) = old_view {
                        view.viewport = old.view.viewport;
                    }
                    commands.entity(entity).insert(CanvasState::new(view));
                }
                (None, Some(blank)) => {
                    commands.entity(entity).remove::<CanvasState>();
                    commands.entity(entity).with_children(|c| {
                        spawn_ground(c, &theme, dots.as_deref());
                        draw_blank(c, &fonts, &canvas.0, blank);
                    });
                }
                (None, None) => {}
            }
            state.drawn = Some(drawn);
            state.card = None;
        }
        if rebuilt || state.card != card {
            if !rebuilt {
                for (e, c) in &cards {
                    if c.0 == canvas.0 {
                        commands.entity(e).despawn();
                    }
                }
            }
            if let Some(v) = &card {
                let pr = canvas.0.clone();
                commands
                    .entity(entity)
                    .with_children(|c| expansion_card(c, &fonts, &pr, v));
            }
            state.card = card;
        }
    }
    for (entity, mut part) in &mut depths {
        let Some(tab) = tabs.0.get(&part.pr) else {
            continue;
        };
        let want = (
            effective_depth(&tab.ui, config_depth),
            tab.ui.calls_depth_menu,
        );
        if part.built == Some(want) {
            continue;
        }
        let pr = part.pr.clone();
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| depth_control(p, &fonts, &pr, want.0, want.1));
        part.built = Some(want);
    }
}

/// Draws `g` into the canvas `entity` and returns the view it opens with.
fn draw_graph(
    commands: &mut Commands,
    entity: Entity,
    pr: &PrRef,
    pen: &Pen,
    theme: &Theme,
    dots: Option<&Dots>,
    g: &CallGraph,
) -> CanvasView {
    let built = build(g);
    let geometry = layout_ranked(&built.spec, &built.ranks, pen.measure(), &PACKING);
    let size = (geometry.size.0 + LABEL_W, geometry.size.1);
    let mut view = CanvasView::new(Vec2::new(size.0, size.1));
    view.fit_floor = FIT_FLOOR;
    view.focus = geometry.boxes.iter().find(|b| b.node == g.center).map(|b| {
        Vec2::new(
            b.rect.x + b.rect.w / 2.0 + LABEL_W,
            b.rect.y + b.rect.h / 2.0,
        )
    });
    let level_of = |b: &BoxGeo| g.nodes.get(b.index).map_or(0, |n| n.level);
    let boxes: Vec<(i8, GRect)> = geometry
        .boxes
        .iter()
        .map(|b| (level_of(b), b.rect))
        .collect();
    commands.entity(entity).with_children(|c| {
        spawn_ground(c, theme, dots);
        spawn_viewport(c, entity, size, |content| {
            spawn_content(content, pen, entity, pr, dots, g, &geometry)
        });
        // Under the header and the legend, which are drawn after them.
        for band in bands(&boxes) {
            c.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: px(10),
                    ..default()
                },
                text(pen, band.label.to_uppercase(), Type::META.size(11.0)),
                TextLayout::new(Justify::Left, LineBreak::NoWrap),
                Visibility::Hidden,
                LevelLabel {
                    canvas: entity,
                    mid: band.y + band.h / 2.0,
                },
                ignore(),
            ));
        }
        header(c, pen, pr, g);
        // Over the header's right end, which the header leaves free for it.
        c.spawn((
            Node {
                position_type: PositionType::Absolute,
                right: px(16),
                top: px(12),
                ..default()
            },
            ignore(),
        ))
        .with_children(|corner| zoom_control(corner, pen, entity));
        legend(c, pen, dots);
    });
    view
}

/// The graph, right of the room the level labels take.
fn spawn_content(
    p: &mut ChildSpawnerCommands,
    pen: &Pen,
    canvas: Entity,
    pr: &PrRef,
    dots: Option<&Dots>,
    g: &CallGraph,
    geometry: &Geometry,
) {
    p.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(LABEL_W),
            top: px(0),
            width: px(geometry.size.0),
            height: px(geometry.size.1),
            ..default()
        },
        ignore(),
    ))
    .with_children(|graph| {
        for edge in &geometry.edges {
            draw_edge(graph, pen, dots, edge, false);
        }
        for b in &geometry.boxes {
            let Some(node) = g.nodes.get(b.index) else {
                continue;
            };
            let look = call_look(node.kind);
            if look.dashed {
                dashed_ring(graph, dots, b.rect, BOX_RADIUS, 0.0, look.ring);
            }
        }
        let first = |kind: NodeKind| !changed_kind(kind);
        let mut order = reading_order(&geometry.boxes);
        order.sort_by_key(|&i| {
            g.nodes
                .get(geometry.boxes[i].index)
                .is_none_or(|n| first(n.kind))
        });
        for i in order {
            let b = &geometry.boxes[i];
            if let Some(node) = g.nodes.get(b.index) {
                draw_call_box(graph, pen, canvas, pr, node, b);
            }
        }
    });
}

fn changed_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Changed | NodeKind::Signature | NodeKind::Moved | NodeKind::New
    )
}

fn draw_call_box(
    p: &mut ChildSpawnerCommands,
    pen: &Pen,
    canvas: Entity,
    pr: &PrRef,
    node: &clusia_calls::GraphNode,
    b: &BoxGeo,
) {
    let look = call_look(node.kind);
    let shifted = GRect {
        x: b.rect.x + LABEL_W,
        ..b.rect
    };
    let border = if look.dashed { 0.0 } else { look.ring_w };
    p.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(b.rect.x),
            top: px(b.rect.y),
            width: px(b.rect.w),
            height: px(b.rect.h),
            border: px(border).all(),
            border_radius: BorderRadius::all(px(BOX_RADIUS)),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
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
        TabIndex(if changed_kind(node.kind) { 0 } else { 1 }),
        DiagramBox {
            canvas,
            pr: pr.clone(),
            number: 0,
            key: "calls".into(),
            node: node.key.clone(),
            rect: shifted,
            anchored: false,
        },
        CallsMarker,
    ))
    .insert((
        observe(on_activate),
        observe(on_double),
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
        if node.kind == NodeKind::More {
            bx.spawn((
                text(pen, node.label.clone(), Type::BODY.ink(look.title)),
                TextLayout::no_wrap(),
                ignore(),
            ));
            return;
        }
        let origin = Vec2::new(b.rect.x + border, b.rect.y + border);
        text_block(bx, pen, &b.title, look.title, false, origin);
        if let Some(sub) = &b.sub {
            text_block(bx, pen, sub, look.sub, false, origin);
        }
    });
}

fn header(c: &mut ChildSpawnerCommands, pen: &Pen, pr: &PrRef, g: &CallGraph) {
    let crumbs = breadcrumb(&g.breadcrumb, CRUMB_CHARS);
    c.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(0),
            right: px(0),
            top: px(0),
            height: px(HEADER_H),
            padding: UiRect::new(px(16), px(150), px(0), px(0)),
            column_gap: px(8),
            align_items: AlignItems::Center,
            border: UiRect::bottom(px(1)),
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Bg),
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|h| {
        let last = crumbs.len().saturating_sub(1);
        for (i, label) in crumbs.iter().enumerate() {
            if i > 0 {
                h.spawn((text(pen, "›", Type::MUTED), ignore()));
            }
            let t = Type {
                size: 12.0,
                weight: if i == last { 600 } else { 400 },
                ink: if i == last { Swatch::Fg } else { Swatch::Muted },
                mono: true,
            };
            h.spawn((text(pen, label.clone(), t), TextLayout::no_wrap(), ignore()));
        }
        if g.hidden_edges > 0 {
            let note = match g.hidden_edges {
                1 => "· 1 edge not shown".to_string(),
                n => format!("· {n} edges not shown"),
            };
            h.spawn((text(pen, note, Type::META), TextLayout::no_wrap(), ignore()));
        }
        h.spawn((
            Node {
                flex_grow: 1.0,
                ..default()
            },
            ignore(),
        ));
        h.spawn((
            Node::default(),
            DepthPart {
                pr: pr.clone(),
                built: None,
            },
        ));
    });
}

/// The Depth button's label, with the dropdown mark.
fn depth_label(depth: u8) -> String {
    format!("Depth {depth} ▼")
}

fn depth_control(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, depth: u8, open: bool) {
    p.spawn((
        button(fonts, &depth_label(depth), Variant::Ghost),
        DepthButton(pr.clone()),
        observe(on_depth_button),
    ));
    if !open {
        return;
    }
    p.spawn((
        card(Node {
            position_type: PositionType::Absolute,
            top: px(34),
            right: px(0),
            flex_direction: FlexDirection::Column,
            padding: UiRect::all(px(4)),
            min_width: px(96),
            ..default()
        }),
        GlobalZIndex(20),
    ))
    .with_children(|menu| {
        for n in clusia_core::config::CALLS_DEPTH_RANGE {
            menu.spawn((
                Node {
                    padding: UiRect::axes(px(10), px(5)),
                    border_radius: BorderRadius::all(px(4)),
                    ..default()
                },
                WidgetButton,
                Clickable,
                Hovered::default(),
                TabIndex(0),
                BackgroundColor::default(),
                Fill(if n == depth {
                    Swatch::Selected
                } else {
                    Swatch::Clear
                }),
                HoverFill(Swatch::Hover),
                DepthItem {
                    pr: pr.clone(),
                    depth: n,
                },
                observe(on_depth_item),
                children![text(fonts, format!("Depth {n}"), Type::BODY)],
            ));
        }
    });
}

fn legend(c: &mut ChildSpawnerCommands, pen: &Pen, dots: Option<&Dots>) {
    c.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(16),
            bottom: px(16),
            column_gap: px(16),
            align_items: AlignItems::Center,
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
    ))
    .with_children(|l| {
        legend_item(
            l,
            pen,
            dots,
            "Changed",
            Swatch::ChangedBg,
            Swatch::Green,
            false,
        );
        legend_item(
            l,
            pen,
            dots,
            "Signature changed",
            Swatch::Clear,
            Swatch::Orange,
            false,
        );
        legend_item(l, pen, dots, "Not touched", Swatch::Bg, Swatch::Line, false);
        legend_item(
            l,
            pen,
            dots,
            "Outside this repo",
            Swatch::Clear,
            Swatch::Edge,
            true,
        );
        l.spawn((
            text(pen, "+N · click to expand", Type::MUTED.size(12.0)),
            ignore(),
        ));
    });
}

/// The canvas with no graph: the card of a file with no pack, or one centred line.
fn draw_blank(c: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, blank: &Blank) {
    let centred = Node {
        position_type: PositionType::Absolute,
        left: px(0),
        top: px(HEADER_H),
        right: px(0),
        bottom: px(0),
        align_items: AlignItems::Center,
        justify_content: JustifyContent::Center,
        ..default()
    };
    match blank {
        Blank::Line(words, failed) => {
            c.spawn((centred, ignore())).with_children(|m| {
                let t = if *failed {
                    Type::BODY.ink(Swatch::Orange)
                } else {
                    Type::MUTED
                };
                m.spawn((text(fonts, words.clone(), t), ignore()));
            });
        }
        Blank::File(path) => {
            c.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: px(0),
                    right: px(0),
                    top: px(0),
                    height: px(HEADER_H),
                    padding: UiRect::horizontal(px(16)),
                    align_items: AlignItems::Center,
                    border: UiRect::bottom(px(1)),
                    ..default()
                },
                BackgroundColor::default(),
                Fill(Swatch::Bg),
                BorderColor::default(),
                Stroke(Swatch::Line),
                children![(
                    text(fonts, path.clone(), Type::MONO.ink(Swatch::Fg)),
                    ignore()
                )],
            ));
            c.spawn((centred, ignore())).with_children(|m| {
                m.spawn(card(Node {
                    width: px(440),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(12),
                    padding: UiRect::all(px(24)),
                    ..default()
                }))
                .with_children(|k| {
                    k.spawn(text(fonts, unsupported_title(path), Type::HEADING));
                    k.spawn(text(
                        fonts,
                        "Clúsia draws calls for Rust, TypeScript, JavaScript and Python. A plugin \
                         can add more languages.",
                        Type::MUTED,
                    ));
                    k.spawn(text(
                        fonts,
                        format!("The change in {path} is still in Story and in the Unified diff."),
                        Type::MUTED,
                    ));
                    k.spawn(Node {
                        column_gap: px(8),
                        ..default()
                    })
                    .with_children(|b| {
                        for (label, what) in [
                            ("Show in Unified", UnsupportedWhat::Unified),
                            ("Open in Story", UnsupportedWhat::Story),
                        ] {
                            b.spawn((
                                button(fonts, label, Variant::Secondary),
                                UnsupportedAction {
                                    pr: pr.clone(),
                                    what,
                                },
                                observe(on_unsupported),
                            ));
                        }
                    });
                });
            });
        }
    }
}

fn expansion_card(c: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, v: &CardView) {
    let ring = match v.tone {
        Tone::Changed | Tone::Moved | Tone::New => Swatch::Green,
        Tone::Signature => Swatch::Orange,
        Tone::Plain => Swatch::Line,
    };
    c.spawn((
        card(Node {
            position_type: PositionType::Absolute,
            width: px(CARD_WIDTH),
            flex_direction: FlexDirection::Column,
            overflow: Overflow::clip(),
            ..default()
        }),
        Visibility::Hidden,
        GlobalZIndex(10),
        ExpansionCard(pr.clone()),
    ))
    .insert(Stroke(ring))
    .with_children(|k| {
        k.spawn((
            Node {
                column_gap: px(8),
                align_items: AlignItems::Center,
                padding: UiRect::new(px(14), px(8), px(10), px(8)),
                border: UiRect::bottom(px(1)),
                ..default()
            },
            BorderColor::default(),
            Stroke(Swatch::Line),
        ))
        .with_children(|h| {
            h.spawn((
                Node {
                    flex_grow: 1.0,
                    min_width: px(0),
                    overflow: Overflow::clip(),
                    ..default()
                },
                children![(
                    text(
                        fonts,
                        v.title.clone(),
                        Type {
                            size: 13.0,
                            weight: 600,
                            ink: Swatch::Fg,
                            mono: true,
                        }
                    ),
                    TextLayout::no_wrap(),
                )],
            ));
            if let Some(tag) = &v.tag {
                let tone = match v.tone {
                    Tone::Changed | Tone::New => kit::Tone::Green,
                    Tone::Signature => kit::Tone::Orange,
                    Tone::Moved | Tone::Plain => kit::Tone::Neutral,
                };
                h.spawn(badge(fonts, tag, tone));
            }
            h.spawn((
                button(fonts, "Full view", Variant::Secondary),
                CardFull(pr.clone()),
                observe(on_card_full),
            ));
            h.spawn((
                button(fonts, "×", Variant::Ghost),
                CardClose(pr.clone()),
                observe(on_card_close),
            ));
        });
        k.spawn(Node {
            flex_direction: FlexDirection::Column,
            padding: UiRect::vertical(px(8)),
            ..default()
        })
        .with_children(|b| body_block(b, fonts, &v.body, 12.0));
        if let Some(note) = &v.note {
            k.spawn((
                Node {
                    padding: UiRect::new(px(14), px(14), px(2), px(12)),
                    ..default()
                },
                children![text(fonts, note.clone(), Type::META)],
            ));
        }
    });
}

// ---- observers and systems

/// The graph node a box stands for, and whether the click or key that activates it ends a pan.
fn node_of_box<'a>(calls: &'a Calls, b: &DiagramBox) -> Option<&'a clusia_calls::GraphNode> {
    calls
        .0
        .get(&b.pr)?
        .graph
        .as_ref()?
        .nodes
        .iter()
        .find(|n| n.key == b.node)
}

/// A click selects the node and opens its card; Enter re-centres the graph on it. The
/// click that ends a pan does nothing.
#[allow(clippy::too_many_arguments)]
fn on_activate(
    activate: On<Activate>,
    keys: Res<ButtonInput<KeyCode>>,
    boxes: Query<&DiagramBox, With<CallsMarker>>,
    mut states: Query<&mut CanvasState>,
    mut tabs: ResMut<ReviewTabs>,
    calls: Res<Calls>,
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
    let enter = keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::NumpadEnter);
    if let (Some(node), Some(tab)) = (node_of_box(&calls, b), tabs.0.get_mut(&b.pr)) {
        pick(
            &mut tab.ui,
            node,
            if enter { How::Enter } else { How::Click },
        );
    }
}

/// A double-click re-centres the graph on the node.
fn on_double(
    click: On<Pointer<Click>>,
    boxes: Query<&DiagramBox, With<CallsMarker>>,
    states: Query<&CanvasState>,
    mut tabs: ResMut<ReviewTabs>,
    calls: Res<Calls>,
    time: Res<Time>,
    frame: Res<FrameCount>,
) {
    if click.event.button != PointerButton::Primary || click.event.count != 2 {
        return;
    }
    let Ok(b) = boxes.get(click.event_target()) else {
        return;
    };
    if states
        .get(b.canvas)
        .is_ok_and(|s| swallows(s, time.elapsed_secs_f64(), frame.0))
    {
        return;
    }
    if let (Some(node), Some(tab)) = (node_of_box(&calls, b), tabs.0.get_mut(&b.pr)) {
        pick(&mut tab.ui, node, How::DoubleClick);
    }
}

fn on_depth_button(
    activate: On<Activate>,
    buttons: Query<&DepthButton>,
    mut tabs: ResMut<ReviewTabs>,
) {
    if let Ok(DepthButton(pr)) = buttons.get(activate.entity)
        && let Some(tab) = tabs.0.get_mut(pr)
    {
        tab.ui.calls_depth_menu = !tab.ui.calls_depth_menu;
    }
}

fn on_depth_item(
    activate: On<Activate>,
    items: Query<&DepthItem>,
    mut tabs: ResMut<ReviewTabs>,
    mut asks: ResMut<Asks>,
    mut model: ResMut<Model>,
) {
    let Ok(item) = items.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(&item.pr) {
        set_depth(&mut tab.ui, item.depth);
        set_config(
            &mut asks,
            &mut model,
            "diff.calls_depth",
            item.depth.to_string(),
        );
    }
}

fn on_card_full(activate: On<Activate>, buttons: Query<&CardFull>, mut tabs: ResMut<ReviewTabs>) {
    if let Ok(CardFull(pr)) = buttons.get(activate.entity)
        && let Some(tab) = tabs.0.get_mut(pr)
    {
        tab.ui.modal = Some(Modal::CallsNode);
    }
}

fn on_card_close(activate: On<Activate>, buttons: Query<&CardClose>, mut tabs: ResMut<ReviewTabs>) {
    if let Ok(CardClose(pr)) = buttons.get(activate.entity)
        && let Some(tab) = tabs.0.get_mut(pr)
    {
        tab.ui.calls_node = None;
    }
}

/// Puts each card beside its node as the view draws it now, and shows it once its canvas has
/// been fitted.
pub(super) fn place_card(
    tabs: Res<ReviewTabs>,
    canvases: Query<(&CallsCanvas, &CanvasState)>,
    boxes: Query<&DiagramBox, With<CallsMarker>>,
    mut cards: Query<(&ExpansionCard, &ComputedNode, &mut Node, &mut Visibility)>,
) {
    for (card, computed, mut node, mut visibility) in &mut cards {
        let Some(key) = tabs.0.get(&card.0).and_then(|t| t.ui.calls_node.as_ref()) else {
            continue;
        };
        let Some((_, state)) = canvases.iter().find(|(c, _)| c.0 == card.0) else {
            continue;
        };
        let Some(b) = boxes.iter().find(|b| b.pr == card.0 && &b.node == key) else {
            continue;
        };
        let laid = computed.size * computed.inverse_scale_factor;
        let size = if laid.x > 0.0 && laid.y > 0.0 {
            laid
        } else {
            CARD_GUESS
        };
        let at = card_origin(b.rect, &state.view, size);
        if node.left != px(at.x) {
            node.left = px(at.x);
        }
        if node.top != px(at.y) {
            node.top = px(at.y);
        }
        let want = if state.fitted {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if *visibility != want {
            *visibility = want;
        }
    }
}

/// Keeps each level label level with its band as the view draws it now, and shows it once its
/// canvas has been fitted.
pub(super) fn place_level_labels(
    canvases: Query<&CanvasState, With<CallsCanvas>>,
    mut labels: Query<(&LevelLabel, &mut Node, &mut Visibility)>,
) {
    for (label, mut node, mut visibility) in &mut labels {
        let Ok(state) = canvases.get(label.canvas) else {
            continue;
        };
        let top = px(state.view.pan.y + label.mid * state.view.zoom - LEVEL_LABEL_H / 2.0);
        if node.top != top {
            node.top = top;
        }
        let want = if state.fitted {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if *visibility != want {
            *visibility = want;
        }
    }
}

/// The green ring of a call box: hovered, focused, or the selected node.
pub(super) fn outline_selected(
    focus: Res<InputFocus>,
    theme: Res<Theme>,
    tabs: Res<ReviewTabs>,
    mut boxes: Query<(Entity, &DiagramBox, &Hovered, &mut Outline), With<CallsMarker>>,
) {
    for (entity, b, hovered, mut outline) in &mut boxes {
        let selected = tabs
            .0
            .get(&b.pr)
            .is_some_and(|t| t.ui.calls_node.as_deref() == Some(b.node.as_str()));
        let on = selected || hovered.get() || focus.get() == Some(entity);
        let want = if on { theme.tokens.green } else { Color::NONE };
        if outline.color != want {
            outline.color = want;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Ask, CallsTell, Tell};
    use crate::review_state::{DiffMode, EditTarget, Modal, ReviewSection, ReviewTabs};
    use crate::screens::review::calls::model::{Calls, CallsRequest};
    use crate::screens::review::calls::testkit::*;
    use crate::screens::review::calls::*;
    use crate::screens::review::diagrams::canvas::{CanvasState, DiagramBox};
    use crate::testing::{self, NOW};
    use clusia_core::PrRef;
    use clusia_protocol::{CallsIndexState, FunctionHit};

    const COMMIT: &str = "9b0c7de4a1c2e3f4";

    fn request(center: Option<&str>, depth: u8, expand: &[i8]) -> CallsRequest {
        CallsRequest {
            center: center.map(String::from),
            depth,
            expand: expand.to_vec(),
        }
    }

    fn get_calls(pr: &PrRef, center: Option<&str>, depth: u8, expand: &[i8]) -> Ask {
        Ask::GetCalls {
            pr: pr.clone(),
            center: center.map(String::from),
            depth: Some(depth),
            expand: expand.to_vec(),
        }
    }

    /// The demo review in Calls: asks recorded while it opened are dropped.
    fn calls_app() -> (App, PrRef) {
        let mut app = testing::app(crate::fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .mode = DiffMode::Calls;
        testing::settle(&mut app);
        (app, pr)
    }

    fn answer(
        app: &mut App,
        pr: &PrRef,
        request: CallsRequest,
        graph: Option<CallGraph>,
        unsupported: &[&str],
    ) {
        testing::tell(
            app,
            Tell::Calls(CallsTell::Graph {
                pr: pr.clone(),
                request,
                graph: graph.map(Box::new),
                index: CallsIndexState::Ready {
                    commit: COMMIT.into(),
                    cut: false,
                },
                unsupported: unsupported.iter().map(|s| s.to_string()).collect(),
            }),
        );
        testing::settle(app);
    }

    fn shown_graph() -> (App, PrRef) {
        let (mut app, pr) = calls_app();
        answer(&mut app, &pr, request(None, 3, &[]), Some(sample()), &[]);
        testing::recorded(&mut app);
        (app, pr)
    }

    fn ui(app: &App, pr: &PrRef) -> crate::review_state::TabUi {
        testing::tab(app, pr).ui
    }

    fn node(app: &mut App, key: &str) -> Entity {
        testing::find::<DiagramBox>(app, |b| b.node == key)
    }

    #[test]
    fn the_level_labels_sit_outside_the_zoomed_content_at_their_band() {
        let (mut app, pr) = shown_graph();
        let canvas = testing::find::<CallsCanvas>(&mut app, |c| c.0 == pr);
        let view = {
            let mut state = app.world_mut().get_mut::<CanvasState>(canvas).unwrap();
            state.view.zoom = 0.5;
            state.view.pan = Vec2::new(10.0, 40.0);
            state.fitted = true;
            state.view
        };
        app.update();
        let labels = testing::entities::<LevelLabel>(&mut app);
        assert_eq!(labels.len(), 3, "Up 2, Up 1, Down 1");
        for e in labels {
            let world = app.world();
            assert_eq!(world.get::<ChildOf>(e).unwrap().parent(), canvas);
            let label = world.get::<LevelLabel>(e).unwrap();
            let node = world.get::<Node>(e).unwrap();
            assert_eq!(
                node.top,
                px(view.pan.y + label.mid * view.zoom - LEVEL_LABEL_H / 2.0)
            );
            assert_eq!(world.get::<Visibility>(e), Some(&Visibility::Inherited));
        }
    }

    #[test]
    fn the_depth_label_has_only_glyphs_the_bundled_font_has() {
        let label = depth_label(3);
        assert!(label.starts_with("Depth 3 "));
        for ch in label.chars() {
            assert!(crate::fonts::covers(crate::fonts::INTER, ch), "{ch:?}");
        }
    }

    #[test]
    fn opening_calls_asks_for_the_graph_and_the_changed_list_once() {
        let (mut app, pr) = calls_app();
        let asked = testing::recorded(&mut app);
        assert_eq!(
            asked
                .iter()
                .filter(|a| **a == get_calls(&pr, None, 3, &[]))
                .count(),
            1,
            "{asked:?}"
        );
        assert!(
            asked.contains(&Ask::FindFunction {
                pr: pr.clone(),
                query: String::new()
            }),
            "{asked:?}"
        );
        testing::settle(&mut app);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "nothing is asked twice"
        );
        answer(&mut app, &pr, request(None, 3, &[]), Some(sample()), &[]);
        assert!(
            !testing::recorded(&mut app)
                .iter()
                .any(|a| matches!(a, Ask::GetCalls { .. } | Ask::FindFunction { .. })),
            "an answer asks for no graph and no search"
        );
        assert_eq!(
            app.world().resource::<Calls>().0[&pr].shown,
            Some(request(None, 3, &[]))
        );
    }

    #[test]
    fn the_graph_is_drawn_with_one_box_per_node_changed_ones_first_in_tab_order() {
        let (mut app, _) = shown_graph();
        assert_eq!(testing::count::<DiagramBox>(&mut app), 9);
        assert_eq!(testing::count::<CallsCanvas>(&mut app), 1);
        let index = |app: &mut App, key: &str| {
            let e = node(app, key);
            app.world()
                .get::<bevy::input_focus::tab_navigation::TabIndex>(e)
                .unwrap()
                .0
        };
        for changed in [REFRESH, EXCHANGE, SAVE, RACE, NEW] {
            assert_eq!(index(&mut app, changed), 0, "{changed}");
        }
        for rest in [AUTH, TOKEN, LOCK, MORE] {
            assert_eq!(index(&mut app, rest), 1, "{rest}");
        }
        for needle in [
            "UP 2",
            "UP 1",
            "DOWN 1",
            "TokenStore::refresh",
            "+2 more calls",
            "Depth",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
    }

    #[test]
    fn the_code_of_the_centre_and_its_nearest_caller_is_asked_for_once_the_graph_arrives() {
        let (mut app, pr) = calls_app();
        testing::recorded(&mut app);
        answer(&mut app, &pr, request(None, 3, &[]), Some(sample()), &[]);
        let asked = testing::recorded(&mut app);
        for function in [REFRESH, TOKEN] {
            let ask = Ask::ReadFunctionCode {
                pr: pr.clone(),
                function: function.into(),
            };
            assert_eq!(
                asked.iter().filter(|a| **a == ask).count(),
                1,
                "{function}: {asked:?}"
            );
        }
        testing::settle(&mut app);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "each code is asked for once"
        );
    }

    #[test]
    fn a_click_selects_and_opens_the_card_and_asks_only_for_the_code_it_lacks() {
        let (mut app, pr) = shown_graph();
        let token = node(&mut app, TOKEN);
        testing::activate(&mut app, token);
        testing::settle(&mut app);
        let u = ui(&app, &pr);
        assert_eq!(
            (u.calls_node.as_deref(), u.calls_center),
            (Some(TOKEN), None)
        );
        assert_eq!(testing::count::<ExpansionCard>(&mut app), 1);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::ReadFunctionCode {
                pr: pr.clone(),
                function: AUTH.into()
            }],
            "the centre and TokenStore::token were asked for with the graph; its caller is new"
        );
    }

    #[test]
    fn enter_recentres_the_graph_on_the_focused_node() {
        let (mut app, pr) = shown_graph();
        let token = node(&mut app, TOKEN);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Enter);
        testing::activate(&mut app, token);
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.release(KeyCode::Enter);
            keys.clear();
        }
        testing::settle(&mut app);
        let u = ui(&app, &pr);
        assert_eq!(
            (u.calls_center.as_deref(), u.calls_node.as_deref()),
            (Some(TOKEN), Some(TOKEN))
        );
        assert!(testing::recorded(&mut app).contains(&get_calls(&pr, Some(TOKEN), 3, &[])));
    }

    #[test]
    fn a_double_click_recentres_the_graph_on_the_node() {
        let (mut app, pr) = shown_graph();
        let exchange = node(&mut app, EXCHANGE);
        testing::click_n(&mut app, exchange, 2);
        testing::settle(&mut app);
        assert_eq!(ui(&app, &pr).calls_center.as_deref(), Some(EXCHANGE));
        assert!(testing::recorded(&mut app).contains(&get_calls(&pr, Some(EXCHANGE), 3, &[])));
        let token = node(&mut app, TOKEN);
        testing::click_n(&mut app, token, 1);
        assert_eq!(
            ui(&app, &pr).calls_center.as_deref(),
            Some(EXCHANGE),
            "one click never recentres"
        );
    }

    #[test]
    fn a_drag_that_ends_on_a_node_selects_nothing_and_the_next_click_does() {
        let (mut app, pr) = shown_graph();
        let canvas = testing::find::<CallsCanvas>(&mut app, |c| c.0 == pr);
        {
            let mut state = app.world_mut().get_mut::<CanvasState>(canvas).unwrap();
            crate::screens::review::diagrams::canvas::drag(
                &mut state,
                Vec2::new(30.0, 0.0),
                Vec2::new(30.0, 0.0),
                0.0,
            );
        }
        let token = node(&mut app, TOKEN);
        testing::activate(&mut app, token);
        assert_eq!(
            ui(&app, &pr).calls_node,
            None,
            "the click that ends a pan opens nothing"
        );
        testing::activate(&mut app, token);
        assert_eq!(ui(&app, &pr).calls_node.as_deref(), Some(TOKEN));
    }

    #[test]
    fn a_more_node_expands_its_level_and_asks_again() {
        let (mut app, pr) = shown_graph();
        let more = node(&mut app, MORE);
        testing::activate(&mut app, more);
        testing::settle(&mut app);
        assert_eq!(ui(&app, &pr).calls_expand, [1]);
        assert!(testing::recorded(&mut app).contains(&get_calls(&pr, None, 3, &[1])));
    }

    #[test]
    fn the_depth_menu_writes_the_config_and_asks_for_the_new_depth() {
        let (mut app, pr) = shown_graph();
        let button = testing::find::<DepthButton>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, button);
        testing::settle(&mut app);
        assert!(ui(&app, &pr).calls_depth_menu);
        for depth in 1..=5 {
            testing::find::<DepthItem>(&mut app, |i| i.depth == depth);
        }
        let four = testing::find::<DepthItem>(&mut app, |i| i.depth == 4);
        testing::activate(&mut app, four);
        testing::settle(&mut app);
        let u = ui(&app, &pr);
        assert_eq!((u.calls_depth, u.calls_depth_menu), (Some(4), false));
        assert_eq!(
            testing::recorded(&mut app),
            [
                Ask::SetConfig {
                    key: "diff.calls_depth".into(),
                    value: "4".into()
                },
                get_calls(&pr, None, 4, &[]),
            ]
        );
    }

    #[test]
    fn a_new_graph_without_the_selected_node_closes_its_card() {
        let (mut app, pr) = shown_graph();
        let token = node(&mut app, TOKEN);
        testing::activate(&mut app, token);
        testing::settle(&mut app);
        assert_eq!(testing::count::<ExpansionCard>(&mut app), 1);
        let mut g = sample();
        g.nodes.retain(|n| n.key != TOKEN);
        g.edges.retain(|e| e.from != TOKEN && e.to != TOKEN);
        answer(&mut app, &pr, request(None, 3, &[]), Some(g), &[]);
        assert_eq!(ui(&app, &pr).calls_node, None);
        assert_eq!(testing::count::<ExpansionCard>(&mut app), 0);
    }

    #[test]
    fn the_cross_closes_the_card_and_full_view_opens_the_modal_with_a_comment_editor() {
        let (mut app, pr) = shown_graph();
        let token = node(&mut app, TOKEN);
        testing::activate(&mut app, token);
        testing::settle(&mut app);
        let close = testing::find::<CardClose>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, close);
        testing::settle(&mut app);
        assert_eq!(
            (
                ui(&app, &pr).calls_node,
                testing::count::<ExpansionCard>(&mut app)
            ),
            (None, 0)
        );
        testing::activate(&mut app, token);
        testing::settle(&mut app);
        let full = testing::find::<CardFull>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, full);
        testing::settle(&mut app);
        let u = ui(&app, &pr);
        assert_eq!(u.modal, Some(Modal::CallsNode));
        assert_eq!(
            u.editor.map(|e| e.target),
            Some(EditTarget::CallsNode {
                function: TOKEN.into()
            })
        );
        assert_eq!(testing::count::<FullModal>(&mut app), 1);
        assert!(testing::shows(
            &mut app,
            "Not in the diff: the comment goes to the review body."
        ));
        let cancel = testing::find::<FullCancel>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, cancel);
        testing::settle(&mut app);
        let u = ui(&app, &pr);
        assert_eq!(
            (u.modal, u.editor),
            (None, None),
            "Cancel closes the modal and its editor"
        );
        assert_eq!(testing::count::<FullModal>(&mut app), 0);
    }

    #[test]
    fn a_comment_from_full_view_sends_add_calls_comment_and_keeps_its_text_on_a_refusal() {
        let (mut app, pr) = shown_graph();
        let refresh = node(&mut app, REFRESH);
        testing::activate(&mut app, refresh);
        let full = testing::find::<CardFull>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, full);
        testing::settle(&mut app);
        assert!(
            !testing::shows(&mut app, "Not in the diff"),
            "a changed function has no hint"
        );
        let area =
            testing::find::<crate::screens::review::editor::EditorArea>(&mut app, |a| a.0 == pr);
        testing::type_into(&mut app, area, "Why one minute?");
        testing::recorded(&mut app);
        let submit =
            testing::find::<crate::screens::review::editor::EditorSubmit>(&mut app, |s| s.0 == pr);
        testing::activate(&mut app, submit);
        let asked = testing::recorded(&mut app);
        let Some(Ask::AddCallsComment {
            function,
            body,
            ticket,
            ..
        }) = asked
            .into_iter()
            .find(|a| matches!(a, Ask::AddCallsComment { .. }))
        else {
            panic!("an AddCallsComment");
        };
        assert_eq!(
            (function.as_str(), body.as_str()),
            (REFRESH, "Why one minute?")
        );
        testing::tell(
            &mut app,
            Tell::Refused {
                pr: pr.clone(),
                ticket,
                message: "Not saved".into(),
            },
        );
        testing::settle(&mut app);
        let u = ui(&app, &pr);
        assert_eq!(u.modal, Some(Modal::CallsNode), "the modal stays");
        let e = u.editor.expect("the editor stays");
        assert_eq!(
            (e.text.as_str(), e.error.as_deref()),
            ("Why one minute?", Some("Not saved"))
        );
    }

    #[test]
    fn escape_and_a_click_outside_keep_a_full_view_comment_that_has_text() {
        let (mut app, pr) = shown_graph();
        let token = node(&mut app, TOKEN);
        testing::activate(&mut app, token);
        let full = testing::find::<CardFull>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, full);
        testing::settle(&mut app);
        let area =
            testing::find::<crate::screens::review::editor::EditorArea>(&mut app, |a| a.0 == pr);
        testing::type_into(&mut app, area, "Half a thought");
        testing::settle(&mut app);
        let backdrop = testing::find::<crate::screens::review::shell::ModalFor>(&mut app, |m| {
            m.modal == Modal::CallsNode
        });
        testing::click(&mut app, backdrop);
        assert_eq!(ui(&app, &pr).modal, Some(Modal::CallsNode));
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();
        let u = ui(&app, &pr);
        assert_eq!(u.modal, Some(Modal::CallsNode));
        assert_eq!(u.editor.map(|e| e.text), Some("Half a thought".to_string()));
    }

    #[test]
    fn comment_in_the_panel_opens_the_editor_and_a_second_one_never_replaces_typed_text() {
        let (mut app, pr) = shown_graph();
        let comment =
            testing::find::<PanelButton>(&mut app, |b| matches!(b.what, PanelAction::Comment));
        testing::activate(&mut app, comment);
        testing::settle(&mut app);
        assert_eq!(
            ui(&app, &pr).editor.map(|e| e.target),
            Some(EditTarget::CallsNode {
                function: REFRESH.into()
            }),
            "the centre is the panel's subject"
        );
        let area =
            testing::find::<crate::screens::review::editor::EditorArea>(&mut app, |a| a.0 == pr);
        testing::type_into(&mut app, area, "Draft words");
        let token = node(&mut app, TOKEN);
        testing::activate(&mut app, token);
        testing::settle(&mut app);
        let comment =
            testing::find::<PanelButton>(&mut app, |b| matches!(b.what, PanelAction::Comment));
        testing::activate(&mut app, comment);
        testing::settle(&mut app);
        let e = ui(&app, &pr).editor.expect("still open");
        assert_eq!(
            (e.target, e.text.as_str()),
            (
                EditTarget::CallsNode {
                    function: REFRESH.into()
                },
                "Draft words"
            )
        );
        assert!(testing::shows(
            &mut app,
            "Finish or cancel the comment you are writing first"
        ));
    }

    #[test]
    fn show_body_diff_goes_to_unified_at_the_function_and_a_file_outside_the_diff_only_says_so() {
        let (mut app, pr) = shown_graph();
        let diff =
            testing::find::<PanelButton>(&mut app, |b| matches!(b.what, PanelAction::ShowDiff));
        testing::activate(&mut app, diff);
        testing::settle(&mut app);
        let u = ui(&app, &pr);
        assert_eq!(
            (u.mode, u.file.as_deref()),
            (DiffMode::Unified, Some("src/auth/refresh.rs"))
        );
        assert_eq!(u.focus_line, Some((clusia_core::draft::Side::Right, 38)));
        let mut t = testing::tab(&app, &pr);
        t.ui.mode = DiffMode::Calls;
        t.ui.calls_node = Some(TOKEN.into());
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .insert(pr.clone(), t);
        testing::settle(&mut app);
        let diff =
            testing::find::<PanelButton>(&mut app, |b| matches!(b.what, PanelAction::ShowDiff));
        testing::activate(&mut app, diff);
        testing::settle(&mut app);
        assert_eq!(ui(&app, &pr).mode, DiffMode::Calls, "nothing changed");
        assert!(testing::shows(&mut app, "This file is not in the diff"));
    }

    #[test]
    fn open_in_editor_asks_for_the_function_line_in_the_worktree() {
        let (mut app, pr) = shown_graph();
        let worktree = testing::ready(&app, &pr)
            .view
            .worktree
            .clone()
            .expect("a worktree");
        let open =
            testing::find::<PanelButton>(&mut app, |b| matches!(b.what, PanelAction::OpenEditor));
        testing::activate(&mut app, open);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::OpenInEditor {
                path: format!("{worktree}/src/auth/refresh.rs"),
                line: Some(38)
            }]
        );
    }

    #[test]
    fn show_them_recentres_on_the_signature_changed_function() {
        let (mut app, pr) = shown_graph();
        let show = testing::find::<PanelButton>(
            &mut app,
            |b| matches!(&b.what, PanelAction::ShowThem(k) if k == NEW),
        );
        testing::activate(&mut app, show);
        testing::settle(&mut app);
        assert_eq!(ui(&app, &pr).calls_center.as_deref(), Some(NEW));
        assert!(testing::recorded(&mut app).contains(&get_calls(&pr, Some(NEW), 3, &[])));
    }

    #[test]
    fn typing_in_the_find_box_asks_and_a_hit_recentres_the_graph() {
        let (mut app, pr) = shown_graph();
        let field = testing::find::<FindField>(&mut app, |f| f.0 == pr);
        testing::type_into(&mut app, field, "refr");
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::FindFunction {
                pr: pr.clone(),
                query: "refr".into()
            }]
        );
        testing::tell(
            &mut app,
            Tell::Calls(CallsTell::Found {
                pr: pr.clone(),
                query: "refr".into(),
                hits: vec![FunctionHit {
                    id: REFRESH.into(),
                    qualified: "TokenStore::refresh".into(),
                    path: "src/auth/refresh.rs".into(),
                    line: 38,
                    changed: true,
                    kind: Some(clusia_calls::ChangeKind::Changed),
                }],
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "src/auth/refresh.rs:38"));
        let row = testing::find::<LeftRow>(&mut app, |r| r.1 == REFRESH);
        testing::activate(&mut app, row);
        testing::settle(&mut app);
        assert_eq!(ui(&app, &pr).calls_center.as_deref(), Some(REFRESH));
        assert!(testing::recorded(&mut app).contains(&get_calls(&pr, Some(REFRESH), 3, &[])));
    }

    #[test]
    fn a_row_of_the_changed_list_recentres() {
        let (mut app, pr) = shown_graph();
        let row = testing::find::<LeftRow>(&mut app, |r| r.1 == EXCHANGE);
        testing::activate(&mut app, row);
        testing::settle(&mut app);
        assert_eq!(ui(&app, &pr).calls_center.as_deref(), Some(EXCHANGE));
    }

    #[test]
    fn with_only_unsupported_files_the_card_replaces_the_graph() {
        let (mut app, pr) = calls_app();
        answer(
            &mut app,
            &pr,
            request(None, 3, &[]),
            None,
            &["server/handler.go", "src/auth/refresh.rs"],
        );
        for needle in [
            "Call graph not available for Go yet",
            "NO CALL GRAPH YET",
            "Show in Unified",
            "Open in Story",
            "server/handler.go",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert_eq!(testing::count::<DiagramBox>(&mut app), 0);
        let row = testing::find::<LeftRow>(&mut app, |r| r.1 == "src/auth/refresh.rs");
        testing::activate(&mut app, row);
        testing::settle(&mut app);
        assert_eq!(
            ui(&app, &pr).calls_node.as_deref(),
            Some("src/auth/refresh.rs")
        );
        assert!(
            testing::shows(&mut app, "Call graph not available for Rust yet")
                || testing::shows(&mut app, "refresh.rs")
        );
        let unified = testing::find::<UnsupportedAction>(&mut app, |a| {
            matches!(a.what, UnsupportedWhat::Unified)
        });
        testing::activate(&mut app, unified);
        testing::settle(&mut app);
        let u = ui(&app, &pr);
        assert_eq!(
            (u.mode, u.file.as_deref()),
            (DiffMode::Unified, Some("src/auth/refresh.rs"))
        );
    }

    #[test]
    fn open_in_story_switches_the_tab_to_story() {
        let (mut app, pr) = calls_app();
        answer(
            &mut app,
            &pr,
            request(None, 3, &[]),
            None,
            &["server/handler.go"],
        );
        let story = testing::find::<UnsupportedAction>(&mut app, |a| {
            matches!(a.what, UnsupportedWhat::Story)
        });
        testing::activate(&mut app, story);
        testing::settle(&mut app);
        assert_eq!(ui(&app, &pr).mode, DiffMode::Story);
    }

    #[test]
    fn a_cut_or_failed_index_shows_in_the_status_bar_and_the_canvas_never_stays_empty() {
        let (mut app, pr) = calls_app();
        let mut g = sample();
        g.cut = true;
        answer(&mut app, &pr, request(None, 3, &[]), Some(g), &[]);
        assert!(testing::shows(&mut app, "Partial: stopped after 10 s"));
        testing::tell(
            &mut app,
            Tell::Calls(CallsTell::Graph {
                pr: pr.clone(),
                request: request(Some("x"), 3, &[]),
                graph: None,
                index: CallsIndexState::Failed {
                    message: "The repository could not be read".into(),
                },
                unsupported: vec![],
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "The repository could not be read"));
    }

    #[test]
    fn a_graph_that_does_not_fit_opens_at_fifty_percent_on_the_centre() {
        let (mut app, pr) = shown_graph();
        let canvas = testing::find::<CallsCanvas>(&mut app, |c| c.0 == pr);
        let state = app
            .world()
            .get::<CanvasState>(canvas)
            .expect("a canvas state");
        assert_eq!(state.view.fit_floor, 0.5);
        assert!(state.view.focus.is_some(), "the centre node is the focus");
    }

    #[test]
    fn a_theme_switch_keeps_the_graph_and_its_selection() {
        let (mut app, pr) = shown_graph();
        let token = node(&mut app, TOKEN);
        testing::activate(&mut app, token);
        testing::settle(&mut app);
        let before = testing::count::<DiagramBox>(&mut app);
        app.world_mut()
            .resource_mut::<crate::theme::Theme>()
            .set_changed();
        testing::settle(&mut app);
        assert_eq!(testing::count::<DiagramBox>(&mut app), before);
        assert_eq!(ui(&app, &pr).calls_node.as_deref(), Some(TOKEN));
        assert_eq!(testing::count::<ExpansionCard>(&mut app), 1);
    }

    #[test]
    fn a_closed_tab_forgets_its_calls() {
        let (mut app, pr) = shown_graph();
        app.world_mut()
            .resource_mut::<crate::nav::Nav>()
            .close_review(&pr);
        testing::settle(&mut app);
        assert!(app.world().resource::<Calls>().0.is_empty());
    }

    #[test]
    fn the_index_becoming_ready_asks_again_once() {
        let (mut app, pr) = calls_app();
        testing::tell(
            &mut app,
            Tell::Calls(CallsTell::Graph {
                pr: pr.clone(),
                request: request(None, 3, &[]),
                graph: None,
                index: CallsIndexState::Building,
                unsupported: vec![],
            }),
        );
        testing::settle(&mut app);
        testing::recorded(&mut app);
        for _ in 0..2 {
            testing::tell(
                &mut app,
                Tell::Calls(CallsTell::Index {
                    pr: pr.clone(),
                    state: CallsIndexState::Ready {
                        commit: COMMIT.into(),
                        cut: false,
                    },
                }),
            );
            testing::settle(&mut app);
        }
        let asked = testing::recorded(&mut app);
        assert_eq!(
            asked
                .iter()
                .filter(|a| matches!(a, Ask::GetCalls { .. }))
                .count(),
            1,
            "once per commit: {asked:?}"
        );
    }

    #[test]
    fn nothing_is_asked_in_another_mode_or_section_or_while_offline() {
        let mut app = testing::app(crate::fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::settle(&mut app);
        assert!(
            !testing::recorded(&mut app)
                .iter()
                .any(|a| matches!(a, Ask::GetCalls { .. })),
            "Unified"
        );
        testing::set_section(&mut app, &pr, ReviewSection::Comments);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .mode = DiffMode::Calls;
        testing::settle(&mut app);
        assert!(
            !testing::recorded(&mut app)
                .iter()
                .any(|a| matches!(a, Ask::GetCalls { .. })),
            "Comments"
        );
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::set_section(&mut app, &pr, ReviewSection::Diff);
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty(), "no connection");
    }
}
