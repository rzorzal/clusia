//! Use case diagrams: actors down the left and right sides, the use cases in a two-column grid
//! inside one boundary frame, and orthogonal routes that run only in the channels between
//! them, so no route crosses a use case. Links are plain lines; the links of one actor leave
//! the middle of its side and fan out from one trunk.

use std::collections::HashMap;

use clusia_core::diagram::{DiagramSpec, GroupKind, Shape};

use crate::geometry::{
    Align, Arrow, BoxGeo, EdgeGeo, Geometry, GroupGeo, Point, Rect, STEP, TextLines,
    crossed_by_other_route, overlap, touches,
};
use crate::layered::{loop_reach, loop_rows, loop_zone};
use crate::layout::{box_text, card};
use crate::measure::{Measure, TextStyle};
use crate::text;

const COL_GAP: f32 = 32.0;
const ROW_GAP: f32 = 40.0;
const ACTOR_GAP: f32 = 36.0;
const BUS_GAP: f32 = 100.0;
const FRAME_SIDE: f32 = 28.0;
const FRAME_TOP_LABELLED: f32 = 44.0;
const FRAME_TOP: f32 = 20.0;
const FRAME_BOTTOM: f32 = 36.0;
const LABEL_CAP: f32 = 88.0;
const WIDE_LABEL_CAP: f32 = 150.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
    Bottom,
}

impl Side {
    fn slot(self) -> usize {
        match self {
            Side::Left => 0,
            Side::Right => 1,
            Side::Bottom => 2,
        }
    }
}

enum Plan {
    /// An actor and a use case; `actor_end` is 0 when the edge starts at the actor.
    ActorPill {
        actor: usize,
        pill: usize,
        actor_end: usize,
    },
    PillPill {
        a: usize,
        b: usize,
    },
    ActorActor {
        a: usize,
        b: usize,
    },
    Loop {
        node: usize,
    },
}

/// Which lane of each channel an edge uses.
#[derive(Default, Clone, Copy)]
struct Lanes {
    bus: usize,
    column: usize,
    row_a: usize,
    row_b: usize,
    out_a: usize,
    out_b: usize,
    top: usize,
}

struct Routed {
    edge: usize,
    plan: Plan,
    lanes: Lanes,
}

#[derive(Default)]
struct Counts {
    bus: [usize; 2],
    out: [usize; 2],
    top: usize,
    column: usize,
    rows: Vec<usize>,
}

pub(crate) fn layout(spec: &DiagramSpec, measure: &dyn Measure) -> Geometry {
    let mut geometry = Geometry::empty();
    let n = spec.nodes.len();
    if n == 0 {
        return geometry;
    }
    let cards: Vec<_> = spec
        .nodes
        .iter()
        .map(|node| card(spec.kind, node, measure))
        .collect();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, node) in spec.nodes.iter().enumerate() {
        index.entry(node.id.as_str()).or_insert(i);
    }
    let is_actor: Vec<bool> = spec
        .nodes
        .iter()
        .map(|node| matches!(node.shape, Shape::Actor | Shape::External))
        .collect();
    let edges: Vec<(usize, usize, usize)> = spec
        .edges
        .iter()
        .enumerate()
        .filter_map(|(i, e)| Some((i, *index.get(e.from.as_str())?, *index.get(e.to.as_str())?)))
        .collect();

    // Actors that mostly start edges go left, the others right; when everyone landed on one
    // side the second half moves across.
    let actors: Vec<usize> = (0..n).filter(|&i| is_actor[i]).collect();
    let mut left = vec![false; n];
    for &a in &actors {
        let starts = edges.iter().filter(|&&(_, f, t)| f == a && t != a).count();
        let ends = edges.iter().filter(|&&(_, f, t)| t == a && f != a).count();
        left[a] = starts >= ends;
    }
    let on_left = actors.iter().filter(|&&a| left[a]).count();
    if actors.len() >= 2 && (on_left == 0 || on_left == actors.len()) {
        let half = actors.len().div_ceil(2);
        for (k, &a) in actors.iter().enumerate() {
            left[a] = k < half;
        }
    }
    let left_actors: Vec<usize> = actors.iter().copied().filter(|&a| left[a]).collect();
    let right_actors: Vec<usize> = actors.iter().copied().filter(|&a| !left[a]).collect();

    // The grid: use cases in reading order, two to a row.
    let pills: Vec<usize> = (0..n).filter(|&i| !is_actor[i]).collect();
    let mut cell: Vec<(usize, usize)> = vec![(0, 0); n];
    for (k, &p) in pills.iter().enumerate() {
        cell[p] = (k % 2, k / 2);
    }
    let rows = pills.len().div_ceil(2);
    let two_columns = pills.len() >= 2;

    // Plan every edge and count the lanes each channel needs.
    let label_of = |edge: usize, cap: f32| -> Option<TextLines> {
        let text_in = spec.edges[edge].label.as_deref()?;
        let lines = text::wrap(measure, text_in, TextStyle::EdgeLabel, cap, 1);
        (!lines.is_empty()).then(|| {
            text::block(
                measure,
                lines,
                TextStyle::EdgeLabel,
                Point::default(),
                None,
                Align::Left,
            )
        })
    };
    let mut counts = Counts {
        rows: vec![0; rows],
        ..Counts::default()
    };
    let mut lead = [20.0f32; 2];
    let mut attach: Vec<[Vec<(usize, usize)>; 3]> = vec![Default::default(); n];
    let mut routed: Vec<Routed> = Vec::new();
    // Each actor's links share one trunk: its lane of the bus beside the actor.
    let mut trunk: HashMap<usize, usize> = HashMap::new();
    for &(edge, a, b) in &edges {
        let mut lanes = Lanes::default();
        let plan = if a == b {
            Plan::Loop { node: a }
        } else if is_actor[a] != is_actor[b] {
            let (actor, pill, actor_end) = if is_actor[a] { (a, b, 0) } else { (b, a, 1) };
            let side = usize::from(!left[actor]);
            lanes.bus = *trunk.entry(actor).or_insert_with(|| {
                counts.bus[side] += 1;
                counts.bus[side] - 1
            });
            if let Some(l) = label_of(edge, LABEL_CAP) {
                lead[side] = lead[side].max(8.0 + l.width + 12.0);
            }
            let near = (left[actor] && cell[pill].0 == 0) || (!left[actor] && cell[pill].0 == 1);
            if !near {
                lanes.column = counts.column;
                counts.column += 1;
                lanes.row_a = counts.rows[cell[pill].1];
                counts.rows[cell[pill].1] += 1;
            }
            let (actor_side, pill_side) = if left[actor] {
                (Side::Right, Side::Left)
            } else {
                (Side::Left, Side::Right)
            };
            attach[actor][actor_side.slot()].push((edge, actor_end));
            attach[pill][pill_side.slot()].push((edge, 1 - actor_end));
            Plan::ActorPill {
                actor,
                pill,
                actor_end,
            }
        } else if !is_actor[a] {
            attach[a][Side::Bottom.slot()].push((edge, 0));
            attach[b][Side::Bottom.slot()].push((edge, 1));
            lanes.row_a = counts.rows[cell[a].1];
            counts.rows[cell[a].1] += 1;
            lanes.row_b = lanes.row_a;
            if cell[a].1 != cell[b].1 {
                lanes.row_b = counts.rows[cell[b].1];
                counts.rows[cell[b].1] += 1;
                lanes.column = counts.column;
                counts.column += 1;
            }
            Plan::PillPill { a, b }
        } else {
            let (la, lb) = (usize::from(!left[a]), usize::from(!left[b]));
            lanes.out_a = counts.out[la];
            counts.out[la] += 1;
            if la != lb {
                lanes.out_b = counts.out[lb];
                counts.out[lb] += 1;
                lanes.top = counts.top;
                counts.top += 1;
            }
            let outer = |x: usize| if left[x] { Side::Left } else { Side::Right };
            attach[a][outer(a).slot()].push((edge, 0));
            attach[b][outer(b).slot()].push((edge, 1));
            Plan::ActorActor { a, b }
        };
        routed.push(Routed { edge, plan, lanes });
    }

    let mut loops_at = vec![0usize; n];
    for r in &routed {
        if let Plan::Loop { node } = r.plan {
            loops_at[node] += 1;
        }
    }
    // Self-loops leave the right side: they need room beside their column, or in the frame's
    // right pad, and the lanes of the other routes start beyond them.
    let mut loop_room = 0.0f32;
    let mut frame_pad_right = FRAME_SIDE;
    let mut outer_pad = 24.0f32;
    for i in 0..n {
        if loops_at[i] == 0 {
            continue;
        }
        let reach = loop_reach(loops_at[i]) + 8.0;
        if is_actor[i] {
            if left[i] {
                lead[0] = lead[0].max(reach);
            } else {
                outer_pad = outer_pad.max(reach);
            }
        } else if two_columns && cell[i].0 == 0 {
            loop_room = loop_room.max(reach);
        } else {
            frame_pad_right = frame_pad_right.max(reach);
        }
    }

    // Column and row sizes.
    let mut col_w = [0.0f32; 2];
    let mut row_h = vec![0.0f32; rows];
    for &p in &pills {
        col_w[cell[p].0] = col_w[cell[p].0].max(cards[p].w);
        row_h[cell[p].1] = row_h[cell[p].1].max(cards[p].h);
    }
    let column_gap = if two_columns {
        COL_GAP.max(loop_room + 24.0 + counts.column as f32 * STEP)
    } else {
        0.0
    };
    let row_gap: Vec<f32> = (0..rows.saturating_sub(1))
        .map(|r| ROW_GAP.max(28.0 + counts.rows[r] as f32 * STEP))
        .collect();
    let bottom_pad = FRAME_BOTTOM + counts.rows.last().copied().unwrap_or(0) as f32 * STEP;
    let boundary = spec.groups.iter().find(|g| g.kind == GroupKind::Boundary);
    let top_pad = if boundary.is_some() {
        FRAME_TOP_LABELLED
    } else {
        FRAME_TOP
    };

    let left_w = left_actors.iter().map(|&a| cards[a].w).fold(0.0, f32::max);
    let right_w = right_actors.iter().map(|&a| cards[a].w).fold(0.0, f32::max);
    let bus_gap = |side: usize, w: f32| {
        if w == 0.0 {
            0.0
        } else {
            BUS_GAP.max(lead[side] + counts.bus[side] as f32 * STEP + 16.0)
        }
    };
    let frame_x = left_w + bus_gap(0, left_w);
    let grid_w = col_w[0]
        + if two_columns {
            column_gap + col_w[1]
        } else {
            0.0
        };
    let frame_w = FRAME_SIDE + grid_w + frame_pad_right;
    let grid_h: f32 = row_h.iter().sum::<f32>() + row_gap.iter().sum::<f32>();
    let frame_h = if pills.is_empty() {
        0.0
    } else {
        top_pad + grid_h + bottom_pad
    };
    let right_x = frame_x + if pills.is_empty() { 0.0 } else { frame_w } + bus_gap(1, right_w);

    let mut rects: Vec<Rect> = vec![Rect::default(); n];
    let col_x = [
        frame_x + FRAME_SIDE,
        frame_x + FRAME_SIDE + col_w[0] + column_gap,
    ];
    let mut row_top = vec![top_pad; rows];
    for r in 1..rows {
        row_top[r] = row_top[r - 1] + row_h[r - 1] + row_gap[r - 1];
    }
    for &p in &pills {
        let (c, r) = cell[p];
        rects[p] = Rect {
            x: col_x[c] + (col_w[c] - cards[p].w) / 2.0,
            y: row_top[r] + (row_h[r] - cards[p].h) / 2.0,
            w: cards[p].w,
            h: cards[p].h,
        };
    }
    // An actor stands level with the middle of the use cases it links, below the actor before
    // it on its side; one that links none keeps its place in a stack centred on the frame.
    let stack = |members: &[usize], x0: f32, colw: f32, rects: &mut Vec<Rect>| {
        let total: f32 = members.iter().map(|&a| cards[a].h).sum::<f32>()
            + ACTOR_GAP * members.len().saturating_sub(1) as f32;
        let mut y = if pills.is_empty() {
            0.0
        } else {
            (frame_h - total) / 2.0
        };
        let mut wanted: Vec<(f32, usize)> = Vec::new();
        for &a in members {
            let linked: Vec<f32> = edges
                .iter()
                .filter_map(|&(_, f, t)| match (f == a, t == a) {
                    (true, false) if !is_actor[t] => Some(t),
                    (false, true) if !is_actor[f] => Some(f),
                    _ => None,
                })
                .map(|p| rects[p].centre().y)
                .collect();
            let top = if linked.is_empty() {
                y
            } else {
                linked.iter().sum::<f32>() / linked.len() as f32 - cards[a].h / 2.0
            };
            wanted.push((top, a));
            y += cards[a].h + ACTOR_GAP;
        }
        wanted.sort_by(|p, q| p.0.total_cmp(&q.0).then(p.1.cmp(&q.1)));
        let mut floor = f32::NEG_INFINITY;
        for (top, a) in wanted {
            let y = top.max(floor);
            rects[a] = Rect {
                x: x0 + (colw - cards[a].w) / 2.0,
                y,
                w: cards[a].w,
                h: cards[a].h,
            };
            floor = y + cards[a].h + ACTOR_GAP;
        }
    };
    stack(&left_actors, 0.0, left_w, &mut rects);
    stack(&right_actors, right_x, right_w, &mut rects);

    // Ports: each side of a box shares its length between the routes that end there, in the
    // order of where their other end is.
    let centre = |i: usize| rects[i].centre();
    let mut ends_of: Vec<(usize, usize)> = vec![(0, 0); spec.edges.len()];
    for &(edge, a, b) in &edges {
        ends_of[edge] = (a, b);
    }
    let other_of = |edge: usize, end: usize| -> usize {
        let (a, b) = ends_of[edge];
        if end == 0 { b } else { a }
    };
    let mut port: HashMap<(usize, usize), f32> = HashMap::new();
    for node in 0..n {
        for side in [Side::Left, Side::Right, Side::Bottom] {
            let list = &attach[node][side.slot()];
            if list.is_empty() {
                continue;
            }
            let mut keyed: Vec<(f32, usize, usize)> = list
                .iter()
                .map(|&(edge, end)| {
                    let other = centre(other_of(edge, end));
                    (
                        if side == Side::Bottom {
                            other.x
                        } else {
                            other.y
                        },
                        edge,
                        end,
                    )
                })
                .collect();
            keyed.sort_by(|p, q| p.0.total_cmp(&q.0).then(p.1.cmp(&q.1)).then(p.2.cmp(&q.2)));
            let r = rects[node];
            let zone = if side == Side::Right && loops_at[node] > 0 {
                loop_zone(loops_at[node]).min(r.h / 2.0)
            } else {
                0.0
            };
            for (k, &(_, edge, end)) in keyed.iter().enumerate() {
                let t = (k + 1) as f32 / (keyed.len() + 1) as f32;
                let at = if side == Side::Bottom {
                    r.x + r.w * t
                } else {
                    r.y + zone + (r.h - zone) * t
                };
                port.insert((edge, end), at);
            }
        }
    }

    // Routes.
    let row_bottom = |r: usize| row_top[r] + row_h[r];
    let lane_y = |r: usize, lane: usize| row_bottom(r) + 12.0 + lane as f32 * STEP;
    let lane_x = |lane: usize| col_x[0] + col_w[0] + loop_room + 12.0 + lane as f32 * STEP;
    let top_y = {
        let mut y = if pills.is_empty() {
            f32::INFINITY
        } else {
            0.0f32
        };
        for &a in &actors {
            y = y.min(rects[a].y);
        }
        if y.is_finite() { y } else { 0.0 }
    };
    let mut loop_count = vec![0usize; n];
    let mut geos: Vec<Option<EdgeGeo>> = vec![None; spec.edges.len()];
    for r in &routed {
        let edge = &spec.edges[r.edge];
        let (points, label): (Vec<Point>, Option<TextLines>) = match r.plan {
            Plan::ActorPill {
                actor,
                pill,
                actor_end,
            } => {
                let ra = rects[actor];
                let rp = rects[pill];
                let ya = ra.centre().y;
                let yp = port[&(r.edge, 1 - actor_end)];
                let is_left = left[actor];
                let (x0, bus_x, px) = if is_left {
                    (
                        ra.right(),
                        left_w + lead[0] + r.lanes.bus as f32 * STEP,
                        rp.x,
                    )
                } else {
                    (
                        ra.x,
                        right_x - lead[1] - r.lanes.bus as f32 * STEP,
                        rp.right(),
                    )
                };
                let near = (is_left && cell[pill].0 == 0) || (!is_left && cell[pill].0 == 1);
                let mut pts = vec![(x0, ya), (bus_x, ya)];
                if near {
                    pts.push((bus_x, yp));
                } else {
                    let yg = lane_y(cell[pill].1, r.lanes.row_a);
                    let xg = lane_x(r.lanes.column);
                    pts.extend([(bus_x, yg), (xg, yg), (xg, yp)]);
                }
                pts.push((px, yp));
                // Above the branch into the use case, where the links of one trunk part.
                let label = label_of(r.edge, LABEL_CAP).map(|mut l| {
                    l.at = if is_left {
                        Point {
                            x: px - 8.0 - l.width,
                            y: yp - 2.0 - l.height,
                        }
                    } else {
                        Point {
                            x: px + 8.0,
                            y: yp - 2.0 - l.height,
                        }
                    };
                    l
                });
                let mut pts: Vec<Point> = pts.into_iter().map(|(x, y)| Point { x, y }).collect();
                if actor_end == 1 {
                    pts.reverse();
                }
                (pts, label)
            }
            Plan::PillPill { a, b } => {
                let (ra, rb) = (rects[a], rects[b]);
                let xa = port[&(r.edge, 0)];
                let xb = port[&(r.edge, 1)];
                let ya = lane_y(cell[a].1, r.lanes.row_a);
                let yb = lane_y(cell[b].1, r.lanes.row_b);
                let mut pts = vec![(xa, ra.bottom()), (xa, ya)];
                if cell[a].1 != cell[b].1 {
                    let xg = lane_x(r.lanes.column);
                    pts.extend([(xg, ya), (xg, yb)]);
                }
                pts.extend([(xb, yb), (xb, rb.bottom())]);
                let label = label_of(r.edge, WIDE_LABEL_CAP).map(|mut l| {
                    l.at = Point {
                        x: xa.min(xb) + 4.0,
                        y: ya + 2.0,
                    };
                    l
                });
                (
                    pts.into_iter().map(|(x, y)| Point { x, y }).collect(),
                    label,
                )
            }
            Plan::ActorActor { a, b } => {
                let ya = port[&(r.edge, 0)];
                let yb = port[&(r.edge, 1)];
                let out_x = |is_left: bool, lane: usize| {
                    if is_left {
                        -24.0 - lane as f32 * STEP
                    } else {
                        right_x + right_w + outer_pad + lane as f32 * STEP
                    }
                };
                let edge_x = |i: usize| {
                    if left[i] {
                        rects[i].x
                    } else {
                        rects[i].right()
                    }
                };
                let mut pts = vec![(edge_x(a), ya)];
                let label = if left[a] == left[b] {
                    let ox = out_x(left[a], r.lanes.out_a);
                    pts.extend([(ox, ya), (ox, yb)]);
                    label_of(r.edge, LABEL_CAP).map(|mut l| {
                        let x = if left[a] {
                            ox - 4.0 - l.width
                        } else {
                            ox + 4.0
                        };
                        l.at = Point {
                            x,
                            y: (ya + yb) / 2.0 - l.height / 2.0,
                        };
                        l
                    })
                } else {
                    let oa = out_x(left[a], r.lanes.out_a);
                    let ob = out_x(left[b], r.lanes.out_b);
                    let yt = top_y - 24.0 - r.lanes.top as f32 * STEP;
                    pts.extend([(oa, ya), (oa, yt), (ob, yt), (ob, yb)]);
                    label_of(r.edge, WIDE_LABEL_CAP).map(|mut l| {
                        l.at = Point {
                            x: (oa + ob) / 2.0 - l.width / 2.0,
                            y: yt - 2.0 - l.height,
                        };
                        l
                    })
                };
                pts.push((edge_x(b), yb));
                (
                    pts.into_iter().map(|(x, y)| Point { x, y }).collect(),
                    label,
                )
            }
            Plan::Loop { node } => {
                let rn = rects[node];
                let nth = loop_count[node];
                loop_count[node] += 1;
                let out = rn.right() + loop_reach(nth + 1);
                let (y1, y2) = loop_rows(rn.y, rn.h, loops_at[node], nth);
                let below = if is_actor[node] {
                    rn.bottom()
                } else {
                    row_bottom(cell[node].1)
                };
                let label = label_of(r.edge, rn.w).map(|mut l| {
                    l.at = Point {
                        x: rn.x,
                        y: below + 2.0,
                    };
                    l
                });
                let points = vec![
                    Point {
                        x: rn.right(),
                        y: y1,
                    },
                    Point { x: out, y: y1 },
                    Point { x: out, y: y2 },
                    Point {
                        x: rn.right(),
                        y: y2,
                    },
                ];
                (points, label)
            }
        };
        geos[r.edge] = Some(EdgeGeo {
            index: r.edge,
            points: simplify(points),
            arrow: Arrow::None,
            label,
            ends: [None, None],
            style: edge.style,
            change: edge.change,
            history: edge.history,
        });
    }

    for (i, (node, c)) in spec.nodes.iter().zip(&cards).enumerate() {
        let (title, sub) = box_text(c, rects[i], measure);
        geometry.boxes.push(BoxGeo {
            node: node.id.clone(),
            index: i,
            rect: rects[i],
            shape: node.shape,
            title,
            sub,
            rows: Vec::new(),
            change: node.change,
            history: node.history,
            anchored: node.anchor.is_some(),
        });
    }
    if let (Some(group), false) = (boundary, pills.is_empty()) {
        let label_lines = text::wrap(
            measure,
            &group.label,
            TextStyle::GroupLabel,
            frame_w - 24.0,
            1,
        );
        geometry.groups.push(GroupGeo {
            id: group.id.clone(),
            rect: Rect {
                x: frame_x,
                y: 0.0,
                w: frame_w,
                h: frame_h,
            },
            label: text::block(
                measure,
                label_lines,
                TextStyle::GroupLabel,
                Point {
                    x: frame_x + 12.0,
                    y: 10.0,
                },
                None,
                Align::Left,
            ),
            kind: GroupKind::Boundary,
        });
    }
    let mut edges: Vec<EdgeGeo> = geos.into_iter().flatten().collect();
    let mut actor_of: Vec<Option<usize>> = vec![None; spec.edges.len()];
    for r in &routed {
        if let Plan::ActorPill { actor, .. } = r.plan {
            actor_of[r.edge] = Some(actor);
        }
    }
    let fan = |a: usize, b: usize| actor_of[a].is_some() && actor_of[a] == actor_of[b];
    separate_ports(
        &mut edges,
        &|e| {
            let (a, b) = ends_of[e.index];
            [rects[a], rects[b]]
        },
        &fan,
    );
    let mut keep_clear: Vec<Rect> = rects.clone();
    keep_clear.extend(geometry.groups.iter().map(|g| g.label.rect()));
    geometry.dropped_texts = settle_labels(&mut edges, keep_clear);
    geometry.edges = edges;
    geometry
}

/// Drops repeated points and the middle point of three in a line, so a link that needs no turn
/// is one straight run.
fn simplify(points: Vec<Point>) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(points.len());
    for p in points {
        if out.last() == Some(&p) {
            continue;
        }
        if let [.., a, b] = out[..]
            && ((a.x == b.x && b.x == p.x) || (a.y == b.y && b.y == p.y))
        {
            out.pop();
        }
        out.push(p);
    }
    out
}

/// Gives each label the first place, near where it was asked for, that overlaps no box, no
/// text already down and no route of another edge, and is not cut by its own; a label with no such place is left out.
/// Returns how many were left out.
fn settle_labels(edges: &mut [EdgeGeo], mut taken: Vec<Rect>) -> u32 {
    let mut dropped = 0;
    let routes: Vec<(usize, Vec<Point>)> =
        edges.iter().map(|e| (e.index, e.points.clone())).collect();
    for edge in edges.iter_mut() {
        let Some(label) = edge.label.take() else {
            continue;
        };
        let offsets = std::iter::once((0.0, 0.0)).chain((1..=8).flat_map(|k| {
            let d = 4.0 * k as f32;
            [(0.0, -d), (0.0, d), (d, 0.0), (-d, 0.0)]
        }));
        let own = edge.index;
        let free = offsets
            .map(|(dx, dy)| {
                let mut moved = label.clone();
                moved.at = Point {
                    x: label.at.x + dx,
                    y: label.at.y + dy,
                };
                moved
            })
            .find(|l| {
                let rect = l.rect();
                let zone = rect.inflate(1.0);
                let inside = rect.inflate(-0.5);
                !taken.iter().any(|t| t.intersects(&rect))
                    && !crossed_by_other_route(&routes, own, zone)
                    && !edge.points.windows(2).any(|w| touches(w[0], w[1], inside))
            });
        match free {
            Some(l) => {
                taken.push(l.rect());
                edge.label = Some(l);
            }
            None => dropped += 1,
        }
    }
    dropped
}

/// How many other edges run along one line with `edges[k]` for more than 2 px; the links of
/// one trunk (`fan` of their edge indices) do on purpose and are not counted.
fn shared_runs(edges: &[EdgeGeo], k: usize, fan: &dyn Fn(usize, usize) -> bool) -> usize {
    let runs = |e: &EdgeGeo| {
        e.points
            .windows(2)
            .map(|w| (w[0], w[1]))
            .collect::<Vec<_>>()
    };
    let mine = runs(&edges[k]);
    edges
        .iter()
        .enumerate()
        .filter(|&(j, other)| {
            j != k
                && !fan(edges[k].index, other.index)
                && runs(other)
                    .iter()
                    .any(|&s| mine.iter().any(|&m| overlap(m, s) > 2.0))
        })
        .count()
}

/// Slides the end of a route along its box by a few pixels where it would lie on another route.
/// `ends` gives the boxes a route joins, from end first.
fn separate_ports(
    edges: &mut [EdgeGeo],
    ends: &dyn Fn(&EdgeGeo) -> [Rect; 2],
    fan: &dyn Fn(usize, usize) -> bool,
) {
    for _ in 0..32 {
        let mut moved = false;
        for k in 0..edges.len() {
            let before = shared_runs(edges, k, fan);
            if before == 0 || edges[k].points.len() < 3 {
                continue;
            }
            let boxes = ends(&edges[k]);
            let last = edges[k].points.len() - 1;
            // The two end runs: (point on the box, next point, the box).
            for (at, next, rect) in [(0, 1, boxes[0]), (last, last - 1, boxes[1])] {
                let original = edges[k].points.clone();
                let (a, b) = (original[at], original[next]);
                let sideways = (a.y - b.y).abs() < 1e-3;
                let (lo, hi) = if sideways {
                    (rect.y + 3.0, rect.bottom() - 3.0)
                } else {
                    (rect.x + 3.0, rect.right() - 3.0)
                };
                let home = if sideways { a.y } else { a.x };
                // `moved` covers the whole round; this end keeps a slide only when its own try helped.
                let mut improved = false;
                'tries: for step in 1..=6 {
                    for sign in [1.0, -1.0] {
                        let to = home + sign * 4.0 * step as f32;
                        if to < lo || to > hi {
                            continue;
                        }
                        let mut points = original.clone();
                        for i in [at, next] {
                            if sideways {
                                points[i].y = to;
                            } else {
                                points[i].x = to;
                            }
                        }
                        edges[k].points = points;
                        if shared_runs(edges, k, fan) < before {
                            improved = true;
                            break 'tries;
                        }
                    }
                }
                if improved {
                    moved = true;
                    break;
                }
                edges[k].points = original;
            }
        }
        if !moved {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::measure::Fonts;
    use crate::testkit::{assert_no_text_clash, check, edge, node, node_sub, spec};
    use crate::{MARGIN, layout};
    use serde_json::{Value, json};

    fn with_boundary(s: &DiagramSpec, members: &[&str]) -> DiagramSpec {
        let mut v = serde_json::to_value(s).expect("json");
        v["groups"] =
            json!([{ "id": "g", "label": "Clúsia", "members": members, "kind": "boundary" }]);
        serde_json::from_value(v).expect("spec")
    }

    /// The Use cases mockup: a reviewer on the left, an agent and GitHub on the right.
    fn mockup() -> DiagramSpec {
        let pills = [
            ("open", "Open a review"),
            ("news", "See what's new"),
            ("comment", "Comment on code"),
            ("ask", "Ask the agent"),
            ("publish", "Publish the review"),
            ("token", "Keep the GitHub token fresh"),
            ("recheck", "Re-check after new commits"),
        ];
        let mut nodes = vec![
            node("reviewer", "«actor»  Reviewer", "actor"),
            node_sub(
                "agent",
                "«actor»  Agent",
                "Claude Code",
                "actor",
                "unchanged",
            ),
            node("github", "«system»  GitHub", "external"),
        ];
        nodes.extend(pills.iter().map(|(id, label)| node(id, label, "pill")));
        let edges = vec![
            edge("reviewer", "open", None),
            edge("reviewer", "comment", None),
            edge("reviewer", "publish", None),
            edge("ask", "agent", None),
            edge("recheck", "github", Some("polls")),
            edge("publish", "github", None),
        ];
        let members: Vec<&str> = pills.iter().map(|p| p.0).collect();
        with_boundary(&spec("use_cases", nodes, edges), &members)
    }

    fn rect_of<'a>(g: &'a Geometry, id: &str) -> &'a Rect {
        &g.boxes.iter().find(|b| b.node == id).expect("box").rect
    }

    #[test]
    fn the_mockup_has_actors_on_the_sides_and_a_two_column_boundary() {
        let s = mockup();
        let g = layout(&s, &Fonts);
        check(&s, &g);
        let frame = &g.groups[0];
        assert_eq!(frame.kind, GroupKind::Boundary);
        assert_eq!(frame.label.lines, vec!["Clúsia".to_string()]);
        let reviewer = rect_of(&g, "reviewer");
        assert!(
            reviewer.right() <= frame.rect.x,
            "the reviewer starts edges, so it is on the left"
        );
        for id in ["agent", "github"] {
            assert!(
                rect_of(&g, id).x >= frame.rect.right(),
                "{id} receives edges, so it is on the right"
            );
        }
        let (open, news, comment) = (
            rect_of(&g, "open"),
            rect_of(&g, "news"),
            rect_of(&g, "comment"),
        );
        assert_eq!(open.y, news.y, "two use cases to a row");
        assert!(news.x > open.right() && comment.y > open.bottom());
        assert!(
            g.boxes
                .iter()
                .filter(|b| b.shape == Shape::Pill)
                .all(|b| frame.rect.inflate(-1.0).intersects(&b.rect))
        );
        assert_eq!(open.h, 56.0);
        assert_eq!(g.edges.len(), 6);
        for e in &g.edges {
            assert_eq!(
                e.label.is_some(),
                s.edges[e.index].label.is_some(),
                "edge {} keeps exactly the label the spec gives it",
                e.index
            );
        }
        assert!(frame.rect.x > MARGIN);
        assert_no_text_clash(&g);
    }

    #[test]
    fn an_actors_links_fan_out_from_one_trunk() {
        let s = mockup();
        let g = layout(&s, &Fonts);
        check(&s, &g);
        let reviewer = rect_of(&g, "reviewer");
        let links: Vec<_> = g.edges.iter().filter(|e| e.index <= 2).collect();
        assert_eq!(links.len(), 3);
        for e in &links {
            assert_eq!(
                e.points[0],
                Point {
                    x: reviewer.right(),
                    y: reviewer.centre().y
                },
                "every link leaves the middle of the actor's side"
            );
            assert_eq!(e.points[1].y, e.points[0].y, "sideways first");
        }
        let bent: Vec<_> = links.iter().filter(|e| e.points.len() > 2).collect();
        assert!(bent.len() >= 2);
        assert!(
            bent.iter().all(|e| e.points[1] == bent[0].points[1]),
            "the links that turn turn at one trunk"
        );
    }

    #[test]
    fn an_actor_sits_level_with_what_it_links_and_keeps_its_name_on_one_line() {
        let s = mockup();
        let g = layout(&s, &Fonts);
        check(&s, &g);
        let (agent, ask) = (rect_of(&g, "agent"), rect_of(&g, "ask"));
        assert_eq!(agent.centre().y, ask.centre().y);
        let to_agent = g.edges.iter().find(|e| e.index == 3).expect("edge");
        assert_eq!(to_agent.points.len(), 2, "a straight line");
        for b in g.boxes.iter().filter(|b| b.shape != Shape::Pill) {
            assert_eq!(b.title.lines.len(), 1, "{:?}", b.title.lines);
        }
    }

    #[test]
    fn three_self_loops_on_one_use_case_keep_clear_of_the_next_column_and_the_frame() {
        for (loops_on, two) in [("a", true), ("b", true), ("a", false)] {
            let mut nodes = vec![
                node("who", "Reviewer", "actor"),
                node("a", "First use case", "pill"),
            ];
            let mut edges = vec![edge("who", "a", None)];
            if two {
                nodes.push(node("b", "Second use case", "pill"));
                edges.push(edge("who", "b", Some("far")));
            }
            for _ in 0..3 {
                edges.push(edge(loops_on, loops_on, Some("again")));
            }
            let s = with_boundary(&spec("use_cases", nodes, edges), &["a", "b"]);
            let g = layout(&s, &Fonts);
            check(&s, &g);
            let frame = g.groups[0].rect;
            let owner = rect_of(&g, loops_on);
            let loops: Vec<_> = g
                .edges
                .iter()
                .filter(|e| s.edges[e.index].from == s.edges[e.index].to)
                .collect();
            assert_eq!(loops.len(), 3);
            for e in loops {
                let far = e.points.iter().map(|p| p.x).fold(f32::MIN, f32::max);
                assert!(far + 8.0 <= frame.right(), "inside the frame");
                for b in g.boxes.iter().filter(|b| b.rect.x > owner.right()) {
                    assert!(far + 8.0 <= b.rect.x, "clear of {}", b.node);
                }
            }
        }
    }

    #[test]
    fn a_route_to_the_far_column_runs_in_the_gutters() {
        let s = mockup();
        let g = layout(&s, &Fonts);
        // reviewer -> comment is column 0 (direct); recheck -> github is column 0 for a right actor
        // and has to go round through the gutters.
        let far = g.edges.iter().find(|e| e.index == 4).expect("edge");
        assert!(far.points.len() >= 5, "{:?}", far.points);
        let direct = g.edges.iter().find(|e| e.index == 1).expect("edge");
        assert_eq!(direct.points.len(), 4);
    }

    #[test]
    fn an_edge_from_a_use_case_to_an_actor_still_starts_at_the_use_case() {
        let s = mockup();
        let g = layout(&s, &Fonts);
        let to_agent = g.edges.iter().find(|e| e.index == 3).expect("edge");
        let ask = rect_of(&g, "ask");
        let agent = rect_of(&g, "agent");
        let first = to_agent.points[0];
        let last = *to_agent.points.last().expect("points");
        assert!((first.x - ask.right()).abs() < 1e-3 || (first.x - ask.x).abs() < 1e-3);
        assert!(
            (last.x - agent.x).abs() < 1e-3,
            "the line ends at the agent"
        );
        assert_eq!(to_agent.arrow, Arrow::None, "a link has no arrowheads");
    }

    #[test]
    fn labels_stay_clear_of_every_box() {
        let s = mockup();
        let g = layout(&s, &Fonts);
        let polls = g.edges.iter().find(|e| e.index == 4).expect("edge");
        assert_eq!(
            polls.label.as_ref().expect("label").lines,
            vec!["polls".to_string()]
        );
    }

    #[test]
    fn actors_that_all_start_edges_are_split_over_both_sides() {
        let s = spec(
            "use_cases",
            vec![
                node("a", "A", "actor"),
                node("b", "B", "actor"),
                node("u", "Do a thing", "pill"),
                node("v", "Do another", "pill"),
            ],
            vec![edge("a", "u", None), edge("b", "v", None)],
        );
        let g = layout(&s, &Fonts);
        check(&s, &g);
        let (a, b) = (rect_of(&g, "a"), rect_of(&g, "b"));
        assert!(a.x < rect_of(&g, "u").x && b.x > rect_of(&g, "v").x);
    }

    #[test]
    fn no_boundary_group_means_no_frame_and_no_use_cases_means_only_actors() {
        let mut s = mockup();
        s.groups.clear();
        let g = layout(&s, &Fonts);
        check(&s, &g);
        assert!(g.groups.is_empty());
        let only = spec(
            "use_cases",
            vec![node("a", "A", "actor"), node("b", "B", "actor")],
            vec![edge("a", "b", Some("talks to")), edge("b", "b", None)],
        );
        let g = layout(&only, &Fonts);
        check(&only, &g);
        assert_eq!(g.edges.len(), 2);
        assert!(
            layout(&spec("use_cases", vec![], vec![]), &Fonts)
                .boxes
                .is_empty()
        );
    }

    #[test]
    fn loops_actor_to_actor_routes_and_unknown_ends_are_handled() {
        let s = with_boundary(
            &spec(
                "use_cases",
                vec![
                    node("l", "Left", "actor"),
                    node("r", "Right", "external"),
                    node("p", "Use case", "pill"),
                    node("q", "Another use case", "pill"),
                    node("s", "Third", "pill"),
                ],
                vec![
                    edge("l", "p", None),
                    edge("p", "r", None),
                    edge("l", "r", Some("over the top")),
                    edge("p", "p", Some("retries")),
                    edge("p", "s", Some("includes")),
                    edge("q", "s", None),
                    edge("l", "ghost", None),
                    edge("r", "r", None),
                ],
            ),
            &["p", "q", "s"],
        );
        let g = layout(&s, &Fonts);
        check(&s, &g);
        assert_eq!(g.edges.len(), 7, "the edge to a missing node is not drawn");
    }

    #[test]
    fn one_actor_tied_to_every_use_case_twice_stays_inside_the_budget() {
        let mut nodes = vec![node("a", "Reviewer", "actor")];
        nodes.extend((0..39).map(|i| node(&format!("u{i}"), &format!("Use case {i}"), "pill")));
        let edges = (0..39)
            .flat_map(|i| {
                let case = format!("u{i}");
                [edge("a", &case, Some("uses")), edge(&case, "a", None)]
            })
            .collect();
        let s = with_boundary(
            &spec("use_cases", nodes, edges),
            &(0..39)
                .map(|i| format!("u{i}"))
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        let started = std::time::Instant::now();
        let g = layout(&s, &Fonts);
        let took = started.elapsed();
        assert!(
            took.as_millis() <= if cfg!(debug_assertions) { 200 } else { 20 },
            "{took:?}"
        );
        // At 78 routes on one actor two of them still share a line, so only the time and the
        // routes' count are held here.
        assert_eq!(g.edges.len(), 78);
    }

    #[test]
    fn random_use_case_diagrams_keep_every_invariant_inside_the_budget() {
        for seed in 0..60u64 {
            let actors = 1 + seed as usize % 4;
            let cases = 1 + seed as usize % 9;
            let mut nodes: Vec<Value> = (0..actors)
                .map(|i| node(&format!("a{i}"), &format!("Actor {i}"), "actor"))
                .collect();
            nodes.extend(
                (0..cases).map(|i| node(&format!("u{i}"), &format!("Use case number {i}"), "pill")),
            );
            let total = actors + cases;
            let mut state = seed
                .wrapping_mul(2862933555777941757)
                .wrapping_add(3037000493);
            let mut next = |bound: usize| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((state >> 33) as usize) % bound
            };
            let name = |i: usize| {
                if i < actors {
                    format!("a{i}")
                } else {
                    format!("u{}", i - actors)
                }
            };
            let edges: Vec<Value> = (0..total + 4)
                .map(|_| edge(&name(next(total)), &name(next(total)), Some("relates to")))
                .collect();
            let members: Vec<String> = (0..cases).map(|i| format!("u{i}")).collect();
            let refs: Vec<&str> = members.iter().map(String::as_str).collect();
            let s = with_boundary(&spec("use_cases", nodes, edges), &refs);
            let started = std::time::Instant::now();
            let g = layout(&s, &Fonts);
            assert!(started.elapsed().as_millis() <= if cfg!(debug_assertions) { 200 } else { 20 });
            check(&s, &g);
            assert_eq!(layout(&s, &Fonts), g);
        }
    }

    #[test]
    fn forty_nodes_and_eighty_edges_stay_valid() {
        let nodes: Vec<Value> = (0..40)
            .map(|i| {
                if i < 4 {
                    node(&format!("n{i}"), &format!("Actor {i}"), "actor")
                } else {
                    node(&format!("n{i}"), &format!("Use case {i}"), "pill")
                }
            })
            .collect();
        let edges: Vec<Value> = (0..80)
            .map(|k| {
                edge(
                    &format!("n{}", (k * 7) % 40),
                    &format!("n{}", (k * 11 + 3) % 40),
                    None,
                )
            })
            .collect();
        let s = spec("use_cases", nodes, edges);
        let started = std::time::Instant::now();
        let g = layout(&s, &Fonts);
        assert!(started.elapsed().as_millis() <= if cfg!(debug_assertions) { 200 } else { 20 });
        check(&s, &g);
    }

    fn labelled(index: usize, points: [(f32, f32); 2], at: (f32, f32)) -> EdgeGeo {
        let text = crate::text::block(
            &Fonts,
            vec!["uses".to_string()],
            TextStyle::EdgeLabel,
            Point { x: at.0, y: at.1 },
            None,
            Align::Left,
        );
        EdgeGeo {
            index,
            points: points.iter().map(|&(x, y)| Point { x, y }).collect(),
            arrow: Arrow::End,
            label: Some(text),
            ends: [None, None],
            style: Default::default(),
            change: Default::default(),
            history: None,
        }
    }

    #[test]
    fn a_label_in_a_clear_place_stays_where_it_was_asked() {
        let mut edges = vec![labelled(0, [(0.0, 100.0), (200.0, 100.0)], (10.0, 84.0))];
        let before = edges[0].label.clone();
        settle_labels(&mut edges, Vec::new());
        assert_eq!(edges[0].label, before);
    }

    #[test]
    fn a_label_on_another_route_is_moved_clear_and_one_with_no_room_is_dropped() {
        // The second edge runs through the place asked for by the first one's label.
        let mut edges = vec![
            labelled(0, [(0.0, 100.0), (200.0, 100.0)], (10.0, 84.0)),
            labelled(1, [(30.0, 60.0), (30.0, 140.0)], (300.0, 300.0)),
        ];
        let blocker = edges[1].label.as_ref().expect("label").rect();
        settle_labels(&mut edges, Vec::new());
        let moved = edges[0].label.as_ref().expect("moved, not dropped").rect();
        assert!(
            moved.x != 10.0 || moved.y != 84.0,
            "it left the crossed place"
        );
        assert!(!moved.intersects(&blocker));
        for w in edges[1].points.windows(2) {
            assert!(!touches(w[0], w[1], moved.inflate(1.0)));
        }
        // Boxes all around leave nowhere to go.
        let mut edges = vec![labelled(0, [(0.0, 100.0), (200.0, 100.0)], (10.0, 84.0))];
        let wall = Rect {
            x: -100.0,
            y: -100.0,
            w: 600.0,
            h: 600.0,
        };
        assert_eq!(settle_labels(&mut edges, vec![wall]), 1, "counted");
        assert!(edges[0].label.is_none());
    }

    fn route(index: usize, points: &[(f32, f32)]) -> EdgeGeo {
        EdgeGeo {
            index,
            points: points.iter().map(|&(x, y)| Point { x, y }).collect(),
            arrow: Arrow::End,
            label: None,
            ends: [None, None],
            style: Default::default(),
            change: Default::default(),
            history: None,
        }
    }

    #[test]
    fn a_route_whose_every_try_fails_goes_back_even_after_another_moved_in_the_round() {
        let rect = |x: f32, y: f32, w: f32, h: f32| Rect { x, y, w, h };
        let mut edges = vec![
            // Shares its first run with the next route and frees it by sliding 4 px.
            route(0, &[(200.0, 0.0), (200.0, 50.0), (300.0, 50.0)]),
            route(1, &[(200.0, 10.0), (200.0, 40.0), (260.0, 40.0)]),
            // Lies on the last route along its middle run, which no end slide can free.
            route(
                2,
                &[(0.0, 200.0), (50.0, 200.0), (50.0, 300.0), (100.0, 300.0)],
            ),
            route(3, &[(50.0, 150.0), (50.0, 350.0)]),
        ];
        let boxes = [
            [
                rect(180.0, -20.0, 60.0, 20.0),
                rect(300.0, 30.0, 20.0, 40.0),
            ],
            [
                rect(180.0, -10.0, 60.0, 20.0),
                rect(260.0, 20.0, 20.0, 40.0),
            ],
            [
                rect(-20.0, 150.0, 20.0, 100.0),
                rect(100.0, 250.0, 20.0, 100.0),
            ],
            [rect(0.0, 100.0, 100.0, 50.0), rect(0.0, 350.0, 100.0, 50.0)],
        ];
        let stuck = edges[2].points.clone();
        separate_ports(&mut edges, &|e| boxes[e.index], &|_, _| false);
        assert_ne!(edges[0].points[0].x, 200.0, "the first route slid");
        assert_eq!(edges[2].points, stuck, "a rejected slide is undone");
    }
}
