//! The pure part of the Calls canvas: the spec and ranks the graph is laid out from, the level
//! bands, the look of each kind of node, the breadcrumb, the caller notes, where the expansion
//! card goes, and what a click, Enter or a double-click on a node does to the tab.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use bevy::math::Vec2;
use clusia_calls::{CallGraph, GraphEdge, GraphNode, NodeKind};
use clusia_core::diagram::{Change, DiagramKind, DiagramSpec, Edge, Node, Shape};
use clusia_diagram::Rect;

use crate::review_state::TabUi;
use crate::screens::review::diagrams::view::CanvasView;
use crate::theme::Swatch;

/// The room left of the graph for the level labels.
pub const LABEL_W: f32 = 76.0;
/// Space between the expansion card and its node, and between the card and the canvas edge.
pub const CARD_GAP: f32 = 8.0;

/// The spec of a graph, the rank of each of its nodes, and for each spec edge the graph edge
/// it draws.
#[derive(Debug, Clone, PartialEq)]
pub struct Built {
    pub spec: DiagramSpec,
    pub ranks: Vec<usize>,
    pub edges: Vec<usize>,
}

/// How many levels of callers the graph shows.
pub fn up_levels(graph: &CallGraph) -> u8 {
    graph
        .nodes
        .iter()
        .map(|n| -i16::from(n.level))
        .max()
        .unwrap_or(0)
        .clamp(0, i16::from(u8::MAX)) as u8
}

/// The row a level is drawn in: the top caller level first, the centre at `up`.
pub fn rank_of(level: i8, up: u8) -> usize {
    (i16::from(level) + i16::from(up)).max(0) as usize
}

/// The spec `layout_ranked` lays out: one box per node, in order, and one line per call between
/// two consecutive levels. Calls that skip a level or go back are left out; the graph counts
/// them in `hidden_edges`. The spec is not validated (a graph may hold more nodes than a
/// drawing may).
pub fn build(graph: &CallGraph) -> Built {
    let up = up_levels(graph);
    let ranks: Vec<usize> = graph.nodes.iter().map(|n| rank_of(n.level, up)).collect();
    let at: HashMap<&str, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.key.as_str(), i))
        .collect();
    let mut spec = DiagramSpec::new("calls", DiagramKind::Graph, "Calls");
    for n in &graph.nodes {
        let mut node = Node::new(&n.key, &n.label, Shape::Box);
        node.sub = (!n.sub.is_empty()).then(|| n.sub.clone());
        node.change = match n.kind {
            NodeKind::Changed => Change::Changed,
            NodeKind::New => Change::New,
            NodeKind::Signature => Change::Risk,
            _ => Change::Unchanged,
        };
        spec.nodes.push(node);
    }
    let mut edges = Vec::new();
    for (i, e) in graph.edges.iter().enumerate() {
        let (Some(&from), Some(&to)) = (at.get(e.from.as_str()), at.get(e.to.as_str())) else {
            continue;
        };
        if ranks[to] != ranks[from] + 1 {
            continue;
        }
        let mut edge = Edge::new(&e.from, &e.to);
        if e.changed {
            edge.change = Change::Changed;
        }
        spec.edges.push(edge);
        edges.push(i);
    }
    Built { spec, ranks, edges }
}

/// The label of a level, at the top of its row of boxes.
#[derive(Debug, Clone, PartialEq)]
pub struct LevelBand {
    pub level: i8,
    pub label: String,
    pub y: f32,
    pub h: f32,
}

/// One band per level that has a box, top to bottom; the centre's level has none.
pub fn bands(boxes: &[(i8, Rect)]) -> Vec<LevelBand> {
    let mut levels: BTreeMap<i8, (f32, f32)> = BTreeMap::new();
    for (level, r) in boxes {
        let e = levels.entry(*level).or_insert((r.y, r.y + r.h));
        e.0 = e.0.min(r.y);
        e.1 = e.1.max(r.y + r.h);
    }
    levels
        .into_iter()
        .filter(|(level, _)| *level != 0)
        .map(|(level, (y, bottom))| LevelBand {
            level,
            label: if level < 0 {
                format!("Up {}", -i16::from(level))
            } else {
                format!("Down {level}")
            },
            y,
            h: bottom - y,
        })
        .collect()
}

/// How a node of the call graph is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CallLook {
    pub fill: Swatch,
    pub ring: Swatch,
    pub ring_w: f32,
    /// A dashed ring on the border instead of a solid one.
    pub dashed: bool,
    pub title: Swatch,
    pub sub: Swatch,
}

pub fn call_look(kind: NodeKind) -> CallLook {
    let look = |fill, ring, ring_w, title, sub| CallLook {
        fill,
        ring,
        ring_w,
        dashed: false,
        title,
        sub,
    };
    match kind {
        NodeKind::Changed | NodeKind::Moved => look(
            Swatch::ChangedBg,
            Swatch::Green,
            1.5,
            Swatch::Fg,
            Swatch::Muted,
        ),
        NodeKind::Signature => look(
            Swatch::Clear,
            Swatch::Orange,
            1.5,
            Swatch::Fg,
            Swatch::Muted,
        ),
        NodeKind::New => look(Swatch::Clear, Swatch::Green, 1.5, Swatch::Fg, Swatch::Muted),
        NodeKind::Untouched => look(Swatch::Bg, Swatch::Line, 1.0, Swatch::Muted, Swatch::Faint),
        NodeKind::Outside => CallLook {
            dashed: true,
            ..look(
                Swatch::Clear,
                Swatch::Edge,
                0.0,
                Swatch::GhostInk,
                Swatch::GhostInk,
            )
        },
        NodeKind::More => look(
            Swatch::Surface,
            Swatch::Line,
            1.0,
            Swatch::Fg,
            Swatch::Muted,
        ),
    }
}

/// The labels of the breadcrumb that fit in `max_chars` with their ` › ` separators, kept from
/// the centre up; a leading `…` says the top was dropped. The centre always stays.
pub fn breadcrumb(labels: &[String], max_chars: usize) -> Vec<String> {
    let Some((last, above)) = labels.split_last() else {
        return Vec::new();
    };
    let mut used = last.chars().count();
    let mut kept = vec![last.clone()];
    let mut dropped = false;
    for label in above.iter().rev() {
        let wide = used + 3 + label.chars().count();
        if wide > max_chars {
            dropped = true;
            break;
        }
        used = wide;
        kept.push(label.clone());
    }
    if dropped {
        kept.push("…".into());
    }
    kept.reverse();
    kept
}

fn node_of<'a>(graph: &'a CallGraph, key: &str) -> Option<&'a GraphNode> {
    graph.nodes.iter().find(|n| n.key == key)
}

/// The caller of `key` drawn nearest to it: the highest level, an untouched one first (its code
/// is what the reader has not seen), then by label.
pub fn nearest_caller<'a>(
    graph: &'a CallGraph,
    key: &str,
) -> Option<(&'a GraphNode, &'a GraphEdge)> {
    graph
        .edges
        .iter()
        .filter(|e| e.to == key)
        .filter_map(|e| Some((node_of(graph, &e.from)?, e)))
        .min_by(|(a, _), (b, _)| {
            (
                std::cmp::Reverse(a.level),
                a.kind != NodeKind::Untouched,
                &a.label,
            )
                .cmp(&(
                    std::cmp::Reverse(b.level),
                    b.kind != NodeKind::Untouched,
                    &b.label,
                ))
        })
}

fn plural(n: u32, one: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {one}s")
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `Reached from 9 callers in 2 levels, up to auth_middleware. TokenStore::token calls it at
/// store.rs:25.`: the callers of the whole index (or, when it has no count, the ones the graph
/// shows), how many levels above the node they span, the top one, and the nearest call.
pub fn reach_note(graph: &CallGraph, key: &str) -> Option<String> {
    let node = node_of(graph, key)?;
    let mut seen: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::from([key]);
    let mut above: Vec<&GraphNode> = Vec::new();
    while let Some(at) = queue.pop_front() {
        for e in graph.edges.iter().filter(|e| e.to == at) {
            if e.from == key || !seen.insert(e.from.as_str()) {
                continue;
            }
            if let Some(caller) = node_of(graph, &e.from) {
                above.push(caller);
                queue.push_back(caller.key.as_str());
            }
        }
    }
    let top = above
        .iter()
        .min_by(|a, b| (a.level, &a.label).cmp(&(b.level, &b.label)))?;
    let (near, edge) = nearest_caller(graph, key)?;
    let count = if node.callers > 0 {
        node.callers
    } else {
        above.len() as u32
    };
    let levels = (i16::from(node.level) - i16::from(top.level)).max(0) as u32;
    let place = match &near.path {
        Some(path) => format!(" at {}:{}", basename(path), edge.line),
        None => String::new(),
    };
    Some(format!(
        "Reached from {} in {}, up to {}. {} calls it{place}.",
        plural(count, "caller"),
        plural(levels, "level"),
        top.label,
        near.label,
    ))
}

/// Where the expansion card of the box `node` (content pixels) goes in the canvas: centred under
/// the box as the view draws it, above it when it does not fit below, and kept `CARD_GAP` inside
/// the canvas whatever the zoom.
pub fn card_origin(node: Rect, view: &CanvasView, card: Vec2) -> Vec2 {
    let (x0, y0) = (
        view.pan.x + node.x * view.zoom,
        view.pan.y + node.y * view.zoom,
    );
    let (w, h) = (node.w * view.zoom, node.h * view.zoom);
    let below = y0 + h + CARD_GAP;
    let y = if below + card.y <= view.viewport.y - CARD_GAP {
        below
    } else {
        y0 - CARD_GAP - card.y
    };
    let x = x0 + w / 2.0 - card.x / 2.0;
    Vec2::new(
        x.min(view.viewport.x - card.x - CARD_GAP).max(CARD_GAP),
        y.min(view.viewport.y - card.y - CARD_GAP).max(CARD_GAP),
    )
}

/// How a node was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    Click,
    Enter,
    DoubleClick,
}

/// What choosing `node` does: a click selects it (its card opens), Enter or a double-click
/// re-centres the graph on it. A *+N more* node expands its level instead; the two clicks of a
/// double-click have expanded it already. An outside node can only be selected.
pub fn pick(ui: &mut TabUi, node: &GraphNode, how: How) {
    match (node.kind, how) {
        (NodeKind::More, How::DoubleClick) => {}
        (NodeKind::More, _) => ui.calls_expand.push(node.level),
        (NodeKind::Outside, _) | (_, How::Click) => ui.calls_node = Some(node.key.clone()),
        (_, How::Enter | How::DoubleClick) => {
            ui.calls_center = Some(node.key.clone());
            ui.calls_node = Some(node.key.clone());
            ui.calls_expand.clear();
        }
    }
}

/// The selection once a new graph arrives: a node still in it, an unsupported file, or the node
/// a comment is being written on stays; anything else is dropped.
pub fn kept_selection(
    node: &Option<String>,
    graph: &CallGraph,
    unsupported: &[String],
    editing: Option<&str>,
) -> Option<String> {
    let k = node.as_ref()?;
    let stays = graph.nodes.iter().any(|n| &n.key == k)
        || unsupported.iter().any(|u| u == k)
        || editing == Some(k.as_str());
    stays.then(|| k.clone())
}

/// A Depth chosen in the menu: the expansions are of the old graph, and the menu closes.
pub fn set_depth(ui: &mut TabUi, depth: u8) {
    ui.calls_depth = Some(depth);
    ui.calls_expand.clear();
    ui.calls_depth_menu = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screens::review::calls::testkit::*;
    use clusia_core::diagram::{Change, DiagramKind, Shape};
    use clusia_diagram::{FaceMeasure, layout_ranked};

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn a_level_is_a_rank_with_the_top_caller_first() {
        assert_eq!(rank_of(-3, 3), 0);
        assert_eq!(rank_of(0, 3), 3, "the centre is rank `up`");
        assert_eq!(rank_of(2, 3), 5);
        assert_eq!(rank_of(1, 0), 1, "no callers: the centre is rank 0");
        let g = sample();
        assert_eq!(up_levels(&g), 2);
    }

    #[test]
    fn build_makes_one_node_per_graph_node_and_ranks_them_by_level() {
        let b = build(&sample());
        assert_eq!(b.spec.kind, DiagramKind::Graph);
        assert_eq!(b.ranks, [0, 1, 1, 1, 2, 3, 3, 3, 3]);
        let ids: Vec<&str> = b.spec.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(
            ids,
            [AUTH, TOKEN, RACE, NEW, REFRESH, EXCHANGE, SAVE, LOCK, MORE]
        );
        assert!(b.spec.nodes.iter().all(|n| n.shape == Shape::Box));
        let refresh = &b.spec.nodes[4];
        assert_eq!(refresh.label, "TokenStore::refresh");
        assert_eq!(refresh.sub.as_deref(), Some("+9 −2 · body changed"));
        assert_eq!(
            b.spec.nodes[8].sub, None,
            "an empty subtitle is no subtitle"
        );
    }

    #[test]
    fn the_change_of_a_node_follows_its_kind() {
        let b = build(&sample());
        let change: Vec<Change> = b.spec.nodes.iter().map(|n| n.change).collect();
        assert_eq!(
            change,
            [
                Change::Unchanged, // AUTH untouched
                Change::Unchanged, // TOKEN untouched
                Change::New,       // RACE
                Change::Risk,      // NEW: signature changed
                Change::Changed,   // REFRESH
                Change::Changed,   // EXCHANGE
                Change::Unchanged, // SAVE moved
                Change::Unchanged, // LOCK outside
                Change::Unchanged, // MORE
            ]
        );
    }

    #[test]
    fn only_edges_between_consecutive_ranks_are_drawn_and_changed_ones_are_green() {
        let b = build(&sample());
        assert_eq!(
            b.edges,
            [0, 1, 3, 4, 5, 6, 7],
            "the edge that skips a rank is left out"
        );
        assert_eq!(b.spec.edges.len(), 7);
        let green: Vec<bool> = b
            .spec
            .edges
            .iter()
            .map(|e| e.change == Change::Changed)
            .collect();
        assert_eq!(green, [false, false, true, false, true, true, false]);
        assert_eq!(
            (b.spec.edges[2].from.as_str(), b.spec.edges[2].to.as_str()),
            (RACE, REFRESH)
        );
    }

    #[test]
    fn an_edge_to_a_node_that_is_not_in_the_graph_is_dropped() {
        let mut g = sample();
        g.edges.push(edge(REFRESH, "src/x.rs#gone#1", 50, false));
        assert_eq!(build(&g).edges.len(), 7);
    }

    #[test]
    fn a_graph_with_no_callers_ranks_the_centre_first() {
        let mut g = sample();
        g.nodes.retain(|n| n.level >= 0);
        g.edges.retain(|e| e.from == REFRESH);
        let b = build(&g);
        assert_eq!(b.ranks, [0, 1, 1, 1, 1]);
        assert_eq!(b.edges, [0, 1, 2], "indices into the graph's own edges");
    }

    #[test]
    fn the_ranked_layout_keeps_the_levels_in_rows_and_every_node_in_place() {
        let b = build(&sample());
        let geometry = layout_ranked(&b.spec, &b.ranks, &FaceMeasure::default());
        assert_eq!(geometry.boxes.len(), 9);
        let rank = |r: usize| -> (f32, f32) {
            let rects = geometry
                .boxes
                .iter()
                .filter(|bx| b.ranks[bx.index] == r)
                .map(|bx| bx.rect);
            let top = rects.clone().map(|c| c.y).fold(f32::MAX, f32::min);
            let bottom = rects.map(|c| c.y + c.h).fold(f32::MIN, f32::max);
            (top, bottom)
        };
        for r in 0..3 {
            assert!(
                rank(r).1 < rank(r + 1).0,
                "rank {r} sits above rank {}",
                r + 1
            );
        }
        assert_eq!(geometry.edges.len(), 7);
        assert!(geometry.edges.iter().all(|e| e.points.len() >= 2));
    }

    #[test]
    fn a_band_is_the_top_and_height_of_the_level_and_the_centre_has_none() {
        let boxes = [
            (-2, r(300.0, 20.0, 160.0, 44.0)),
            (-1, r(100.0, 110.0, 160.0, 44.0)),
            (-1, r(300.0, 112.0, 160.0, 48.0)),
            (0, r(200.0, 200.0, 190.0, 50.0)),
            (1, r(80.0, 300.0, 160.0, 44.0)),
            (1, r(300.0, 300.0, 160.0, 44.0)),
        ];
        let b = bands(&boxes);
        let seen: Vec<(i8, &str, f32, f32)> = b
            .iter()
            .map(|b| (b.level, b.label.as_str(), b.y, b.h))
            .collect();
        assert_eq!(
            seen,
            [
                (-2, "Up 2", 20.0, 44.0),
                (-1, "Up 1", 110.0, 50.0),
                (1, "Down 1", 300.0, 44.0),
            ]
        );
    }

    #[test]
    fn every_kind_has_its_own_look() {
        use Swatch::*;
        let look = |k| call_look(k);
        assert_eq!(
            look(NodeKind::Changed),
            CallLook {
                fill: ChangedBg,
                ring: Green,
                ring_w: 1.5,
                dashed: false,
                title: Fg,
                sub: Muted
            }
        );
        assert_eq!(look(NodeKind::Moved), look(NodeKind::Changed));
        assert_eq!(
            look(NodeKind::Signature),
            CallLook {
                fill: Clear,
                ring: Orange,
                ring_w: 1.5,
                dashed: false,
                title: Fg,
                sub: Muted
            }
        );
        assert_eq!(
            look(NodeKind::New),
            CallLook {
                fill: Clear,
                ring: Green,
                ring_w: 1.5,
                dashed: false,
                title: Fg,
                sub: Muted
            }
        );
        assert_eq!(
            look(NodeKind::Untouched),
            CallLook {
                fill: Bg,
                ring: Line,
                ring_w: 1.0,
                dashed: false,
                title: Muted,
                sub: Faint
            }
        );
        assert_eq!(
            look(NodeKind::Outside),
            CallLook {
                fill: Clear,
                ring: Edge,
                ring_w: 0.0,
                dashed: true,
                title: GhostInk,
                sub: GhostInk
            }
        );
        assert_eq!(
            look(NodeKind::More),
            CallLook {
                fill: Surface,
                ring: Line,
                ring_w: 1.0,
                dashed: false,
                title: Fg,
                sub: Muted
            }
        );
    }

    #[test]
    fn a_long_breadcrumb_drops_its_top_and_says_so() {
        let labels: Vec<String> = [
            "auth_middleware",
            "TokenStore::token",
            "TokenStore::refresh",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(breadcrumb(&labels, 60), labels, "57 characters fit");
        assert_eq!(
            breadcrumb(&labels, 40),
            ["…", "TokenStore::token", "TokenStore::refresh"]
        );
        assert_eq!(
            breadcrumb(&labels, 0),
            ["…", "TokenStore::refresh"],
            "the centre always stays"
        );
        assert_eq!(breadcrumb(&labels[2..], 0), ["TokenStore::refresh"]);
        assert!(breadcrumb(&[], 10).is_empty());
    }

    #[test]
    fn the_nearest_caller_is_the_highest_level_then_the_untouched_one() {
        let g = sample();
        let (n, e) = nearest_caller(&g, REFRESH).unwrap();
        assert_eq!((n.key.as_str(), e.line), (TOKEN, 25));
        let (n, e) = nearest_caller(&g, TOKEN).unwrap();
        assert_eq!((n.key.as_str(), e.line), (AUTH, 30));
        assert!(nearest_caller(&g, AUTH).is_none());
    }

    #[test]
    fn the_reach_note_counts_every_caller_above_the_node() {
        let g = sample();
        assert_eq!(
            reach_note(&g, REFRESH).as_deref(),
            Some(
                "Reached from 9 callers in 2 levels, up to auth_middleware. TokenStore::token calls it at store.rs:25."
            )
        );
        assert_eq!(
            reach_note(&g, TOKEN).as_deref(),
            Some(
                "Reached from 4 callers in 1 level, up to auth_middleware. auth_middleware calls it at middleware.rs:30."
            )
        );
        let mut few = sample();
        few.nodes
            .iter_mut()
            .find(|n| n.key == TOKEN)
            .unwrap()
            .callers = 1;
        assert_eq!(
            reach_note(&few, TOKEN).as_deref(),
            Some(
                "Reached from 1 caller in 1 level, up to auth_middleware. auth_middleware calls it at middleware.rs:30."
            )
        );
        let mut none = sample();
        none.nodes
            .iter_mut()
            .find(|n| n.key == REFRESH)
            .unwrap()
            .callers = 0;
        assert!(
            reach_note(&none, REFRESH)
                .unwrap()
                .starts_with("Reached from 4 callers in 2 levels"),
            "no index count: the callers in the graph"
        );
        assert_eq!(reach_note(&g, AUTH), None, "nothing above the top");
        assert_eq!(reach_note(&g, "nope"), None);
    }

    #[test]
    fn the_card_sits_under_its_node_when_there_is_room() {
        let mut v = CanvasView::new(Vec2::new(2000.0, 1200.0));
        v.set_viewport(Vec2::new(900.0, 600.0));
        v.zoom = 0.5;
        v.pan = Vec2::new(100.0, 80.0);
        let at = card_origin(r(200.0, 100.0, 170.0, 44.0), &v, Vec2::new(320.0, 260.0));
        assert_eq!(at, Vec2::new(82.5, 160.0));
    }

    #[test]
    fn the_card_goes_above_its_node_near_the_bottom() {
        let mut v = CanvasView::new(Vec2::new(2000.0, 1200.0));
        v.set_viewport(Vec2::new(900.0, 600.0));
        v.zoom = 0.5;
        v.pan = Vec2::new(100.0, 80.0);
        let at = card_origin(r(200.0, 1000.0, 170.0, 44.0), &v, Vec2::new(320.0, 260.0));
        assert_eq!(at, Vec2::new(82.5, 312.0));
    }

    #[test]
    fn the_card_never_leaves_the_canvas() {
        let mut v = CanvasView::new(Vec2::new(2000.0, 1200.0));
        v.set_viewport(Vec2::new(900.0, 600.0));
        v.zoom = 0.5;
        v.pan = Vec2::new(100.0, 80.0);
        let card = Vec2::new(320.0, 260.0);
        assert_eq!(
            card_origin(r(1600.0, 100.0, 170.0, 44.0), &v, card),
            Vec2::new(572.0, 160.0),
            "right edge"
        );
        assert_eq!(
            card_origin(r(0.0, 100.0, 170.0, 44.0), &v, card),
            Vec2::new(8.0, 160.0),
            "left edge"
        );
        let mut small = CanvasView::new(Vec2::new(2000.0, 1200.0));
        small.set_viewport(Vec2::new(300.0, 200.0));
        assert_eq!(
            card_origin(r(10.0, 10.0, 100.0, 40.0), &small, card),
            Vec2::new(8.0, 8.0),
            "a canvas smaller than the card pins it to the corner"
        );
    }

    fn ui() -> TabUi {
        TabUi::default()
    }

    #[test]
    fn a_click_selects_and_expands_without_moving_the_centre() {
        let g = sample();
        let mut u = ui();
        pick(&mut u, &g.nodes[1], How::Click);
        assert_eq!(
            (u.calls_node.as_deref(), u.calls_center.clone()),
            (Some(TOKEN), None)
        );
        assert!(u.calls_expand.is_empty());
    }

    #[test]
    fn enter_and_a_double_click_recentre_and_forget_what_was_expanded() {
        let g = sample();
        for how in [How::Enter, How::DoubleClick] {
            let mut u = ui();
            u.calls_expand = vec![1, -1];
            pick(&mut u, &g.nodes[1], how);
            assert_eq!(u.calls_center.as_deref(), Some(TOKEN));
            assert_eq!(u.calls_node.as_deref(), Some(TOKEN));
            assert!(u.calls_expand.is_empty());
        }
    }

    #[test]
    fn a_more_node_expands_its_level_on_a_click_or_enter_and_not_on_a_double_click() {
        let g = sample();
        let more = &g.nodes[8];
        let mut u = ui();
        pick(&mut u, more, How::Click);
        pick(&mut u, more, How::Enter);
        assert_eq!(u.calls_expand, [1, 1]);
        pick(&mut u, more, How::DoubleClick);
        assert_eq!(
            u.calls_expand,
            [1, 1],
            "the two clicks of a double-click already expanded"
        );
        assert_eq!((u.calls_center, u.calls_node), (None, None));
    }

    #[test]
    fn an_outside_node_only_selects() {
        let g = sample();
        for how in [How::Click, How::Enter, How::DoubleClick] {
            let mut u = ui();
            pick(&mut u, &g.nodes[7], how);
            assert_eq!(u.calls_node.as_deref(), Some(LOCK));
            assert_eq!(u.calls_center, None);
        }
    }

    #[test]
    fn a_selection_survives_a_graph_that_still_has_the_node() {
        let g = sample();
        let keep = |node: &str, editing| kept_selection(&Some(node.to_string()), &g, &[], editing);
        assert_eq!(keep(TOKEN, None).as_deref(), Some(TOKEN));
        assert_eq!(keep("src/gone.rs#gone#1", None), None, "the card closes");
        assert_eq!(
            keep("src/gone.rs#gone#1", Some("src/gone.rs#gone#1")).as_deref(),
            Some("src/gone.rs#gone#1"),
            "a comment being written keeps its node"
        );
        assert_eq!(kept_selection(&None, &g, &[], None), None);
        assert_eq!(
            kept_selection(
                &Some("server/handler.go".into()),
                &g,
                &["server/handler.go".into()],
                None
            )
            .as_deref(),
            Some("server/handler.go"),
            "an unsupported file is a selection too"
        );
    }

    #[test]
    fn a_new_depth_clears_the_expansion_and_closes_the_menu() {
        let mut u = ui();
        u.calls_expand = vec![1];
        u.calls_depth_menu = true;
        u.calls_node = Some(TOKEN.into());
        set_depth(&mut u, 4);
        assert_eq!(u.calls_depth, Some(4));
        assert!(u.calls_expand.is_empty() && !u.calls_depth_menu);
        assert_eq!(
            u.calls_node.as_deref(),
            Some(TOKEN),
            "the selection is judged when the new graph arrives"
        );
    }
}
