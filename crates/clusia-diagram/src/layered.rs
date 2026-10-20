//! Layered placement and orthogonal routing for the box-and-arrow kinds.
//!
//! Work happens in *rank* (`a`) and *cross* (`c`) coordinates: ranks are columns for
//! `LeftToRight` and rows for `TopToBottom`, and `to_xy` turns a pair into a point at the end.
//! Edges that span several ranks pass through one dummy vertex per rank they cross, so a route
//! never runs through a box: it runs in the lane a dummy holds between two boxes.

use std::cmp::Reverse;
use std::collections::{HashMap, VecDeque};

use clusia_core::diagram::DiagramSpec;

use crate::geometry::{
    Align, Arrow, EdgeGeo, Point, Rect, STEP, TextLines, crossed_by_other_route, overlap, touches,
};
use crate::measure::{Measure, TextStyle};
use crate::spine;
use crate::text;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    LeftToRight,
    TopToBottom,
}

/// A node's box size before placement. A `fixed` box keeps its size; any other grows along
/// the cross axis so every edge end gets its own port.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NodeSize {
    pub w: f32,
    pub h: f32,
    pub fixed: bool,
}

pub(crate) struct Options<'a> {
    pub direction: Direction,
    pub arrow: Arrow,
    /// Whether the boxes of a row share their top edge rather than their middle line.
    pub top_aligned: bool,
    /// The face of the edge labels.
    pub label_style: TextStyle,
    /// A rank per node (a missing entry is 0) in place of the longest-path ranks.
    pub ranks: Option<&'a [usize]>,
    /// The least space between two ranks, in place of the direction's.
    pub rank_gap: Option<f32>,
    /// The space between two boxes of one rank, in place of `NODE_GAP`.
    pub node_gap: Option<f32>,
}

pub(crate) struct Placed {
    pub rects: Vec<Rect>,
    pub edges: Vec<EdgeGeo>,
    /// Labels and cardinalities left out for want of a free place.
    pub dropped_texts: u32,
}

/// Space between two boxes of one rank.
const NODE_GAP: f32 = 48.0;
/// Space between a lane and its neighbours in one rank.
const LANE_GAP: f32 = 16.0;
/// Space kept free after the last run of a gap.
const END_PAD: f32 = 16.0;
/// Space between a box and the box that hangs below it.
const STACK_GAP: f32 = 72.0;
/// The widest label a gap between two ranks grows to hold on one line.
const LABEL_LINE: f32 = 160.0;
/// Space between two ports on one side of a box.
const PORT_PITCH: f32 = 8.0;
/// How far a self-loop leaves its box.
const LOOP_OUT: f32 = 20.0;
const LOOP_SPAN: f32 = 12.0;

/// The two rows self-loop `nth` of `count` on one side of a box `height` tall runs along: 12 px
/// apart, each loop 5 px lower than the one before, or in a strip of its own on a box too short
/// for that.
pub(crate) fn loop_rows(top: f32, height: f32, count: usize, nth: usize) -> (f32, f32) {
    if loop_zone(count) <= height {
        let y1 = top + 6.0 + 5.0 * nth as f32;
        (y1, y1 + LOOP_SPAN)
    } else {
        let slot = (height - 4.0) / count as f32;
        (
            top + 3.0 + slot * nth as f32,
            top + 1.0 + slot * (nth + 1) as f32,
        )
    }
}

/// How far the last of `count` self-loops on one box reaches beyond its far side.
pub(crate) fn loop_reach(count: usize) -> f32 {
    let later = count.saturating_sub(1);
    LOOP_OUT + 6.0 * later.min(4) as f32 + 2.0 * later.saturating_sub(4) as f32
}

/// How much of a box's side its `count` self-loops take from the top.
pub(crate) fn loop_zone(count: usize) -> f32 {
    6.0 + 5.0 * count.saturating_sub(1) as f32 + LOOP_SPAN + 4.0
}

/// Where the ports of a side start, counted from the top of a box `height` tall that has
/// `count` self-loops: under them while 8 px are left, else from the middle.
fn ports_from(count: usize, height: f32) -> f32 {
    if count == 0 {
        return 0.0;
    }
    loop_zone(count).min(height - PORT_PITCH).max(height * 0.5)
}
const SWEEPS: usize = 4;
const RELAX_ROUNDS: usize = 3;

impl Direction {
    fn base_gap(self) -> f32 {
        match self {
            Direction::LeftToRight => 96.0,
            Direction::TopToBottom => 72.0,
        }
    }

    /// Space at the start of a gap kept for the labels of the edges that leave the rank before.
    fn lead(self) -> f32 {
        match self {
            Direction::LeftToRight => 24.0,
            Direction::TopToBottom => 32.0,
        }
    }

    fn to_xy(self, a: f32, c: f32) -> Point {
        match self {
            Direction::LeftToRight => Point { x: a, y: c },
            Direction::TopToBottom => Point { x: c, y: a },
        }
    }
}

struct Vertex {
    /// The node index, or `None` for a dummy.
    real: Option<usize>,
    ext_a: f32,
    ext_c: f32,
}

/// The nodes `(upper, lower)` of a column pair and the edges between them, each with its
/// `from` node.
type Column = ((usize, usize), Vec<(usize, usize)>);

/// One edge as a path through ranks: `verts` run from the lower rank to the higher.
struct Chain {
    edge: usize,
    reversed: bool,
    verts: Vec<usize>,
}

/// Places the nodes of `spec` (sized by `sizes`) and routes its edges.
pub(crate) fn place(
    spec: &DiagramSpec,
    sizes: &[NodeSize],
    measure: &dyn Measure,
    opts: &Options<'_>,
) -> Placed {
    let n = spec.nodes.len();
    if n == 0 {
        return Placed {
            rects: Vec::new(),
            edges: Vec::new(),
            dropped_texts: 0,
        };
    }
    let dir = opts.direction;
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, node) in spec.nodes.iter().enumerate() {
        index.entry(node.id.as_str()).or_insert(i);
    }
    // (edge index, from, to) for the edges between two nodes; self-loops apart.
    let mut links: Vec<(usize, usize, usize)> = Vec::new();
    let mut loops: Vec<(usize, usize)> = Vec::new();
    for (i, e) in spec.edges.iter().enumerate() {
        let (Some(&a), Some(&b)) = (index.get(e.from.as_str()), index.get(e.to.as_str())) else {
            continue;
        };
        if a == b {
            loops.push((i, a));
        } else {
            links.push((i, a, b));
        }
    }

    let reversed = back_edges(n, &links);
    let dag: Vec<(usize, usize)> = links
        .iter()
        .zip(&reversed)
        .map(|(&(_, a, b), &r)| if r { (b, a) } else { (a, b) })
        .collect();
    let mut rank = match opts.ranks {
        Some(given) => (0..n).map(|i| given.get(i).copied().unwrap_or(0)).collect(),
        None => ranks(n, &dag),
    };
    let mut depth = vec![0usize; n];
    if dir == Direction::LeftToRight
        && let Some(plan) = spine::plan(sizes, &dag, &rank)
    {
        rank = plan.layer;
        depth = plan.depth;
    }
    let stacked = depth.iter().any(|&d| d > 0);
    let layer_count = rank.iter().copied().max().unwrap_or(0) + 1;

    let mut verts: Vec<Vertex> = (0..n)
        .map(|i| Vertex {
            real: Some(i),
            ext_a: 0.0,
            ext_c: 0.0,
        })
        .collect();
    let mut layer_of: Vec<usize> = rank.clone();
    let mut chains: Vec<Chain> = Vec::new();
    // Edges between two nodes of one column, which run straight down the column.
    let mut verticals: Vec<(usize, usize, usize)> = Vec::new();
    for &(edge, a, b) in &links {
        if rank[a] == rank[b] {
            verticals.push((edge, a, b));
            continue;
        }
        let flipped = rank[a] > rank[b];
        let (u, v) = if flipped { (b, a) } else { (a, b) };
        let mut path = vec![u];
        for r in rank[u] + 1..rank[v] {
            verts.push(Vertex {
                real: None,
                ext_a: 0.0,
                ext_c: 0.0,
            });
            layer_of.push(r);
            path.push(verts.len() - 1);
        }
        path.push(v);
        chains.push(Chain {
            edge,
            reversed: flipped,
            verts: path,
        });
    }
    let total = verts.len();
    let mut up: Vec<Vec<usize>> = vec![Vec::new(); total];
    let mut down: Vec<Vec<usize>> = vec![Vec::new(); total];
    for chain in &chains {
        for pair in chain.verts.windows(2) {
            down[pair[0]].push(pair[1]);
            up[pair[1]].push(pair[0]);
        }
    }

    // Ports on each side of a box: how many routes start and end there.
    let mut out_count = vec![0usize; n];
    let mut in_count = vec![0usize; n];
    for chain in &chains {
        out_count[chain.verts[0]] += 1;
        in_count[chain.verts[chain.verts.len() - 1]] += 1;
    }
    let mut loops_at = vec![0usize; n];
    for &(_, node) in &loops {
        loops_at[node] += 1;
    }

    // Extents along the rank and cross axes; a box grows to fit its ports.
    let mut rects_size: Vec<(f32, f32)> = Vec::with_capacity(n);
    for (i, s) in sizes.iter().enumerate().take(n) {
        let (mut w, mut h) = (s.w, s.h);
        if !s.fixed {
            let need = (out_count[i].max(in_count[i]) + 1) as f32 * PORT_PITCH
                + if loops_at[i] > 0 {
                    loop_zone(loops_at[i])
                } else {
                    0.0
                };
            match dir {
                Direction::LeftToRight => h = h.max(need),
                Direction::TopToBottom => w = w.max(need),
            }
        }
        rects_size.push((w, h));
        match dir {
            Direction::LeftToRight => {
                verts[i].ext_a = w;
                verts[i].ext_c = h;
            }
            Direction::TopToBottom => {
                verts[i].ext_a = h;
                verts[i].ext_c = w;
            }
        }
    }

    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); layer_count];
    for v in 0..total {
        layers[layer_of[v]].push(v);
    }
    order_layers(&mut layers, &up, &down);
    let c_top = if stacked {
        gather_columns(&mut layers, &verts, &depth);
        column_positions(&layers, &verts, opts.top_aligned)
    } else {
        cross_positions(
            &layers,
            &verts,
            &up,
            &down,
            opts.node_gap.unwrap_or(NODE_GAP),
        )
    };

    // Ports along the cross axis, for the start and the end of every chain.
    let cross_mid = |v: usize| c_top[v] + verts[v].ext_c / 2.0;
    let mut starts: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut ends: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (k, chain) in chains.iter().enumerate() {
        starts[chain.verts[0]].push(k);
        ends[chain.verts[chain.verts.len() - 1]].push(k);
    }
    let mut start_port = vec![0.0f32; chains.len()];
    let mut end_port = vec![0.0f32; chains.len()];
    for v in 0..n {
        starts[v].sort_by(|&p, &q| {
            let kp = cross_mid(chains[p].verts[1]);
            let kq = cross_mid(chains[q].verts[1]);
            kp.total_cmp(&kq).then(chains[p].edge.cmp(&chains[q].edge))
        });
        ends[v].sort_by(|&p, &q| {
            let kp = cross_mid(chains[p].verts[chains[p].verts.len() - 2]);
            let kq = cross_mid(chains[q].verts[chains[q].verts.len() - 2]);
            kp.total_cmp(&kq).then(chains[p].edge.cmp(&chains[q].edge))
        });
        let top = c_top[v];
        let ext = verts[v].ext_c;
        let lo = top + ports_from(loops_at[v], ext);
        for (slot, &k) in starts[v].iter().enumerate() {
            start_port[k] =
                lo + (top + ext - lo) * (slot + 1) as f32 / (starts[v].len() + 1) as f32;
        }
        for (slot, &k) in ends[v].iter().enumerate() {
            end_port[k] = top + ext * (slot + 1) as f32 / (ends[v].len() + 1) as f32;
        }
    }
    // A route between neighbouring ranks runs straight when one end's port can move level with
    // the other's: inside its box and a port pitch from the other ports on that side.
    for (k, chain) in chains.iter().enumerate() {
        if chain.verts.len() != 2 {
            continue;
        }
        let (u, v) = (chain.verts[0], chain.verts[1]);
        let fits = |node: usize, at: f32, others: &[usize], ports: &[f32], from: f32| {
            at >= c_top[node] + from.max(3.0)
                && at <= c_top[node] + verts[node].ext_c - 3.0
                && others
                    .iter()
                    .all(|&j| j == k || (ports[j] - at).abs() >= PORT_PITCH)
        };
        if (start_port[k] - end_port[k]).abs() < 0.5 {
            continue;
        }
        if fits(v, start_port[k], &ends[v], &end_port, 0.0) {
            end_port[k] = start_port[k];
        } else if fits(
            u,
            end_port[k],
            &starts[u],
            &start_port,
            ports_from(loops_at[u], verts[u].ext_c),
        ) {
            start_port[k] = end_port[k];
        }
    }
    // The cross coordinate each chain has at each of its vertices.
    let mut cross_of: Vec<Vec<f32>> = chains
        .iter()
        .enumerate()
        .map(|(k, chain)| {
            let last = chain.verts.len() - 1;
            chain
                .verts
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    if i == 0 {
                        start_port[k]
                    } else if i == last {
                        end_port[k]
                    } else {
                        cross_mid(v)
                    }
                })
                .collect()
        })
        .collect();
    // A lane that is all but level with the run before it joins that run exactly.
    for lanes in cross_of.iter_mut() {
        for i in 1..lanes.len() - 1 {
            if (lanes[i] - lanes[i - 1]).abs() < 0.5 {
                lanes[i] = lanes[i - 1];
            }
        }
    }
    separate_runs(
        &chains,
        &layer_of,
        &verts,
        &layers,
        &c_top,
        &loops_at,
        &mut cross_of,
    );

    // Room each gap needs for the cardinalities and the label of an edge that has both.
    let card_w =
        |text: Option<&str>| text.map_or(0.0, |t| measure.width(t, TextStyle::Card).min(64.0));
    // A label of up to `LABEL_LINE` px keeps to one line: its gap grows to hold it.
    let mut label_room = vec![0.0f32; layer_count];
    if dir == Direction::LeftToRight {
        for chain in &chains {
            let edge = &spec.edges[chain.edge];
            let wide = card_w(edge.ends[0].as_deref()).max(card_w(edge.ends[1].as_deref()));
            if let Some(text) = edge.label.as_deref() {
                let natural = measure.width(text, opts.label_style).min(LABEL_LINE);
                let cards = if wide > 0.0 { 2.0 * (wide + 4.0) } else { 0.0 };
                let first = layer_of[chain.verts[0]];
                label_room[first] = label_room[first].max(natural + 12.0 + cards);
            }
        }
    }

    // Hops between neighbouring ranks and the vertical run each needs.
    let band: Vec<f32> = layers
        .iter()
        .map(|l| l.iter().map(|&v| verts[v].ext_a).fold(0.0, f32::max))
        .collect();
    let mut hops: Vec<Vec<(f32, f32, usize, usize)>> = vec![Vec::new(); layer_count];
    for (k, chain) in chains.iter().enumerate() {
        for i in 0..chain.verts.len() - 1 {
            let (p, q) = (cross_of[k][i], cross_of[k][i + 1]);
            if p != q {
                hops[layer_of[chain.verts[i]]].push((p.min(q), p.max(q), k, i));
            }
        }
    }
    // The first run of a gap starts past the self-loops of the boxes before it.
    let lead_of: Vec<f32> = (0..layer_count)
        .map(|l| {
            let reach = layers[l]
                .iter()
                .filter_map(|&v| verts[v].real)
                .filter(|&i| loops_at[i] > 0)
                .map(|i| loop_reach(loops_at[i]) + 4.0)
                .fold(0.0, f32::max);
            // A tighter rank gap starts its runs sooner, so one run still fits in it.
            let lead = opts
                .rank_gap
                .map_or(dir.lead(), |g| dir.lead().min(g / 2.0));
            lead.max(reach)
        })
        .collect();
    let mut slot_of: HashMap<(usize, usize), usize> = HashMap::new();
    let mut gap: Vec<f32> = Vec::new();
    for (l, layer_hops) in hops
        .iter_mut()
        .enumerate()
        .take(layer_count.saturating_sub(1))
    {
        layer_hops.sort_by(|x, y| {
            x.0.total_cmp(&y.0)
                .then(x.1.total_cmp(&y.1))
                .then(x.2.cmp(&y.2))
                .then(x.3.cmp(&y.3))
        });
        let mut slot_ends: Vec<f32> = Vec::new();
        for &(lo, hi, k, i) in layer_hops.iter() {
            let slot = match slot_ends.iter().position(|&end| end + 6.0 < lo) {
                Some(s) => {
                    slot_ends[s] = hi;
                    s
                }
                None => {
                    slot_ends.push(hi);
                    slot_ends.len() - 1
                }
            };
            slot_of.insert((k, i), slot);
        }
        gap.push(
            opts.rank_gap
                .unwrap_or(dir.base_gap())
                .max(lead_of[l] + slot_ends.len() as f32 * STEP + END_PAD)
                .max(label_room[l]),
        );
    }
    let mut band_start = vec![0.0f32; layer_count];
    for l in 1..layer_count {
        band_start[l] = band_start[l - 1] + band[l - 1] + gap[l - 1];
    }
    let band_end = |l: usize| band_start[l] + band[l];

    // Rectangles of the nodes.
    let rects: Vec<Rect> = (0..n)
        .map(|v| {
            let (w, h) = rects_size[v];
            let p = dir.to_xy(band_start[layer_of[v]], c_top[v]);
            Rect {
                x: p.x,
                y: p.y,
                w,
                h,
            }
        })
        .collect();

    // Routes.
    let mut geos: Vec<Option<EdgeGeo>> = vec![None; spec.edges.len()];
    let mut back_routes = BackRoutes::default();
    // Shorter ways back for edges against the flow, taken once every route is known.
    let mut backs: Vec<(usize, Vec<Point>)> = Vec::new();
    // Labels wait until every cardinality is down, then each takes the first free place.
    let mut texts = Texts::default();
    for (k, chain) in chains.iter().enumerate() {
        let edge = &spec.edges[chain.edge];
        let mut pts: Vec<(f32, f32)> = Vec::new();
        for i in 0..chain.verts.len() - 1 {
            let (u, w) = (chain.verts[i], chain.verts[i + 1]);
            let exit_a = if verts[u].real.is_some() {
                band_start[layer_of[u]] + verts[u].ext_a
            } else {
                band_end(layer_of[u])
            };
            let enter_a = band_start[layer_of[w]];
            let (p, q) = (cross_of[k][i], cross_of[k][i + 1]);
            pts.push((exit_a, p));
            if let Some(&slot) = slot_of.get(&(k, i)) {
                let a = band_end(layer_of[u]) + lead_of[layer_of[u]] + slot as f32 * STEP;
                pts.push((a, p));
                pts.push((a, q));
            }
            pts.push((enter_a, q));
        }
        let mut points: Vec<Point> = simplify(&pts)
            .into_iter()
            .map(|(a, c)| dir.to_xy(a, c))
            .collect();
        if chain.reversed {
            points.reverse();
        }
        if dir == Direction::LeftToRight && chain.reversed && edge.ends.iter().all(Option::is_none)
        {
            let (from, to) = (chain.verts[chain.verts.len() - 1], chain.verts[0]);
            if let Some(route) = back_route(&rects, from, to, points[0].y, &mut back_routes) {
                backs.push((chain.edge, route));
            }
        }

        // Labels: the edge's own in the first gap, the cardinalities at the two ends.
        let first_layer = layer_of[chain.verts[0]];
        let last_layer = layer_of[chain.verts[chain.verts.len() - 1]];
        let (start_at, end_at) = (cross_of[k][0], cross_of[k][chain.verts.len() - 1]);
        let near_start = end_label(
            measure,
            dir,
            edge.ends[usize::from(chain.reversed)].as_deref(),
            true,
            band_start[first_layer] + verts[chain.verts[0]].ext_a,
            start_at,
        );
        let near_end = end_label(
            measure,
            dir,
            edge.ends[usize::from(!chain.reversed)].as_deref(),
            false,
            band_start[last_layer],
            end_at,
        );
        let wide = card_w(edge.ends[0].as_deref()).max(card_w(edge.ends[1].as_deref()));
        if let Some(text) = edge.label.as_deref() {
            let label = edge_label(
                measure,
                dir,
                opts.label_style,
                text,
                band_end(first_layer),
                gap.get(first_layer).copied(),
                start_at,
                (wide > 0.0).then_some(wide),
            );
            let mut other_side = label.clone();
            match dir {
                Direction::LeftToRight => {
                    other_side.at.y = if wide > 0.0 {
                        start_at - 2.0 - label.height
                    } else {
                        start_at + 2.0
                    };
                }
                Direction::TopToBottom => other_side.at.x = start_at - 6.0 - label.width,
            }
            let mut places = vec![label.clone(), other_side.clone()];
            if dir == Direction::LeftToRight {
                let room = gap.get(first_layer).copied().unwrap_or(0.0);
                let mut dx = 8.0;
                let (from, to) = (band_end(first_layer), band_end(first_layer) + room);
                while label.at.x + dx + label.width <= to - 4.0 {
                    for base in [&label, &other_side] {
                        let mut shifted = base.clone();
                        shifted.at.x += dx;
                        if shifted.at.x >= from {
                            places.push(shifted);
                        }
                    }
                    dx += 8.0;
                }
            } else {
                let room = gap.get(first_layer).copied().unwrap_or(0.0);
                let to = band_end(first_layer) + room;
                let mut dy = 8.0;
                while label.at.y + dy + label.height <= to - 4.0 {
                    for base in [&label, &other_side] {
                        let mut shifted = base.clone();
                        shifted.at.y += dy;
                        places.push(shifted);
                    }
                    dy += 8.0;
                }
            }
            let line = text::block(
                measure,
                vec![text.to_string()],
                label.style,
                Point::default(),
                None,
                Align::Left,
            );
            texts.offer(chain.edge, (first_layer, start_at), places, Some(line));
        }
        let mut tags = [None, None];
        tags[usize::from(chain.reversed)] = near_start;
        tags[usize::from(!chain.reversed)] = near_end;
        texts.hold(tags.iter().flatten());
        geos[chain.edge] = Some(EdgeGeo {
            index: chain.edge,
            points,
            arrow: opts.arrow,
            label: None,
            ends: tags,
            style: edge.style,
            change: edge.change,
            history: edge.history,
        });
    }

    // Edges between a node and the node below it run straight down their column.
    let mut columns: Vec<Column> = Vec::new();
    for &(edge, a, b) in &verticals {
        let key = if c_top[a] <= c_top[b] { (a, b) } else { (b, a) };
        match columns.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push((edge, a)),
            None => columns.push((key, vec![(edge, a)])),
        }
    }
    for ((upper, lower), list) in &columns {
        let layer = layer_of[*upper];
        let (w, _) = rects_size[*upper];
        let across = w.min(rects_size[*lower].0);
        let (y_top, y_bottom) = (c_top[*upper] + verts[*upper].ext_c, c_top[*lower]);
        let room = band_start
            .get(layer + 1)
            .map_or(f32::INFINITY, |next| next - 4.0);
        for (j, &(i, from)) in list.iter().enumerate() {
            let edge = &spec.edges[i];
            let x = band_start[layer] + across * (j + 1) as f32 / (list.len() + 1) as f32;
            let mut points = vec![Point { x, y: y_top }, Point { x, y: y_bottom }];
            let from_top = from == *upper;
            if !from_top {
                points.reverse();
            }
            let tops = [
                card_text(measure, edge.ends[usize::from(!from_top)].as_deref()),
                card_text(measure, edge.ends[usize::from(from_top)].as_deref()),
            ];
            let mut ends = [None, None];
            let mut span = (y_top + 2.0, y_bottom - 2.0);
            if let Some(mut card) = tops[0].clone() {
                card.at = Point {
                    x: x + 4.0,
                    y: y_top + 2.0,
                };
                span.0 = card.rect().bottom() + 2.0;
                ends[usize::from(!from_top)] = Some(card);
            }
            if let Some(mut card) = tops[1].clone() {
                card.at = Point {
                    x: x + 4.0,
                    y: y_bottom - 2.0 - card.height,
                };
                span.1 = card.at.y - 2.0;
                ends[usize::from(from_top)] = Some(card);
            }
            texts.hold(ends.iter().flatten());
            if let Some(text) = edge.label.as_deref() {
                let label = column_label(measure, opts.label_style, text, x + 6.0, room, span);
                texts.offer(i, (layer, y_top), vec![label], None);
            }
            geos[i] = Some(EdgeGeo {
                index: i,
                points,
                arrow: opts.arrow,
                label: None,
                ends,
                style: edge.style,
                change: edge.change,
                history: edge.history,
            });
        }
    }

    // Self-loops leave the far side of the box and come back to it.
    let mut seen = vec![0usize; n];
    for &(i, node) in &loops {
        let edge = &spec.edges[i];
        let nth = seen[node];
        seen[node] += 1;
        let far = band_start[layer_of[node]] + verts[node].ext_a;
        let out = far + loop_reach(nth + 1);
        let (y1, y2) = loop_rows(c_top[node], verts[node].ext_c, loops_at[node], nth);
        let points = vec![
            dir.to_xy(far, y1),
            dir.to_xy(out, y1),
            dir.to_xy(out, y2),
            dir.to_xy(far, y2),
        ];
        if let Some(text) = edge.label.as_deref() {
            let label = loop_label(
                measure,
                dir,
                opts.label_style,
                text,
                far,
                out,
                (y1, y2),
                nth,
            );
            texts.offer(i, (layer_of[node], y1), vec![label], None);
        }
        geos[i] = Some(EdgeGeo {
            index: i,
            points,
            arrow: opts.arrow,
            label: None,
            ends: [None, None],
            style: edge.style,
            change: edge.change,
            history: edge.history,
        });
    }

    // A way back is taken only where it runs along no other route; its label then sits
    // beside one of its runs.
    for (edge, route) in backs {
        let alone = geos.iter().flatten().filter(|g| g.index != edge).all(|g| {
            g.points.windows(2).all(|a| {
                route
                    .windows(2)
                    .all(|b| overlap((a[0], a[1]), (b[0], b[1])) <= 2.0)
            })
        });
        if !alone {
            continue;
        }
        if let Some(text) = spec.edges[edge].label.as_deref() {
            let line = text::block(
                measure,
                vec![text.to_string()],
                opts.label_style,
                Point::default(),
                None,
                Align::Left,
            );
            texts.replace(edge, beside_runs(&route, &line));
        }
        if let Some(Some(geo)) = geos.get_mut(edge) {
            geo.points = route;
        }
    }

    let routes: Vec<(usize, &[Point])> = geos
        .iter()
        .flatten()
        .map(|g| (g.index, g.points.as_slice()))
        .collect();
    let (placed_labels, mut dropped_texts) = texts.place(&routes, &rects);
    for (edge, label) in placed_labels {
        if let Some(Some(geo)) = geos.get_mut(edge) {
            geo.label = Some(label);
        }
    }

    let mut edges: Vec<EdgeGeo> = geos.into_iter().flatten().collect();
    dropped_texts += settle_cardinalities(&mut edges, &rects);
    Placed {
        rects,
        edges,
        dropped_texts,
    }
}

/// How many back edges already run over the top of the drawing, and up into each box.
#[derive(Default)]
struct BackRoutes {
    over: usize,
    up_into: HashMap<usize, usize>,
}

/// A shorter way back for an edge that points against the flow, from box `from` to box `to`
/// on its left, when it is clear of every other box. When the two share a row with a box
/// between them, it runs over the top of the boxes between, from the top of one to the top of
/// the other; when `from` hangs below the row of `to`, it leaves `from`'s port at `port_y`
/// sideways and goes up into the middle of `to`'s bottom. Later routes of either kind keep
/// `STEP` from the earlier ones.
fn back_route(
    rects: &[Rect],
    from: usize,
    to: usize,
    port_y: f32,
    taken: &mut BackRoutes,
) -> Option<Vec<Point>> {
    let (f, t) = (rects[from], rects[to]);
    if f.x <= t.right() {
        return None;
    }
    let p = |x: f32, y: f32| Point { x, y };
    let between = rects
        .iter()
        .any(|r| r.x >= t.right() && r.right() <= f.x && r.y < f.bottom() && r.bottom() > f.y);
    let route = if (f.centre().y - t.centre().y).abs() < 0.5 && between {
        let top = rects
            .iter()
            .filter(|r| r.right() >= t.x && r.x <= f.right())
            .map(|r| r.y)
            .fold(f32::INFINITY, f32::min)
            - 24.0
            - taken.over as f32 * STEP;
        vec![
            p(f.centre().x, f.y),
            p(f.centre().x, top),
            p(t.centre().x, top),
            p(t.centre().x, t.y),
        ]
    } else if port_y > t.bottom() + 8.0 {
        let x = t.centre().x + taken.up_into.get(&to).copied().unwrap_or(0) as f32 * STEP;
        vec![p(f.x, port_y), p(x, port_y), p(x, t.bottom())]
    } else {
        return None;
    };
    let clear = route.windows(2).all(|w| {
        rects.iter().enumerate().all(|(i, r)| {
            let zone = if i == from || i == to {
                r.inflate(-0.5)
            } else {
                r.inflate(1.0)
            };
            !touches(w[0], w[1], zone)
        })
    });
    if !clear {
        return None;
    }
    if route.len() == 4 {
        taken.over += 1;
    } else {
        *taken.up_into.entry(to).or_insert(0) += 1;
    }
    Some(route)
}

/// Keeps every cardinality clear of other routes and off its own: one that touches the route
/// of another edge, or is cut by its own, is nudged along its own end, in 4 px steps, and
/// flipped to the other side of the route, to the first place that overlaps no box and no
/// text and touches no other route. A cardinality is data (`1` or `0..N`), so it is left out
/// only when no such place exists; returns how many were left out.
fn settle_cardinalities(edges: &mut [EdgeGeo], boxes: &[Rect]) -> u32 {
    let mut dropped = 0;
    let routes: Vec<(usize, Vec<Point>)> =
        edges.iter().map(|e| (e.index, e.points.clone())).collect();
    let crossed = |text: &TextLines, own: usize| {
        let zone = text.rect().inflate(1.0);
        let inside = text.rect().inflate(-0.5);
        crossed_by_other_route(&routes, own, zone)
            || routes
                .iter()
                .filter(|(e, _)| *e == own)
                .any(|(_, points)| points.windows(2).any(|w| touches(w[0], w[1], inside)))
    };
    for k in 0..edges.len() {
        for slot in 0..2 {
            let Some(text) = edges[k].ends[slot].clone() else {
                continue;
            };
            let own = edges[k].index;
            if !crossed(&text, own) {
                continue;
            }
            let points = &edges[k].points;
            let (first, last) = (points[0], points[points.len() - 1]);
            let (anchor, next) = if slot == 0 {
                (first, points[1])
            } else {
                (last, points[points.len() - 2])
            };
            let along_x = (anchor.y - next.y).abs() < 1e-3;
            let others: Vec<Rect> = edges
                .iter()
                .flat_map(|e| e.label.iter().chain(e.ends.iter().flatten()))
                .map(TextLines::rect)
                .filter(|r| *r != text.rect())
                .collect();
            let moved = |dx: f32, dy: f32, flip: bool| {
                let mut t = text.clone();
                t.at = Point {
                    x: text.at.x + dx,
                    y: text.at.y + dy,
                };
                if flip {
                    if along_x {
                        t.at.y = 2.0 * anchor.y - (t.at.y + t.height);
                    } else {
                        t.at.x = 2.0 * anchor.x - (t.at.x + t.width);
                    }
                }
                t
            };
            let mut found = None;
            'search: for step in 0..=6 {
                let d = 4.0 * step as f32;
                for flip in [false, true] {
                    for sign in [1.0, -1.0] {
                        if step == 0 && sign < 0.0 {
                            continue;
                        }
                        let (dx, dy) = if along_x {
                            (sign * d, 0.0)
                        } else {
                            (0.0, sign * d)
                        };
                        let t = moved(dx, dy, flip);
                        let rect = t.rect();
                        if !crossed(&t, own)
                            && !boxes.iter().any(|b| b.intersects(&rect))
                            && !others.iter().any(|o| o.intersects(&rect))
                        {
                            found = Some(t);
                            break 'search;
                        }
                    }
                }
            }
            dropped += u32::from(found.is_none());
            edges[k].ends[slot] = found;
        }
    }
    dropped
}

/// The labels of edges that ask for a place, and the texts already down. A label takes the
/// first of its places that overlaps no box and no text already down, touches no route of
/// another edge and is not cut by its own; when none is free it tries the runs of its own
/// route, beside each, longest first, on one line and then wrapped. A crowded port leaves a
/// label out instead of printing it over something.
#[derive(Default)]
struct Texts {
    taken: Vec<Rect>,
    offers: Vec<Offer>,
}

struct Offer {
    edge: usize,
    at: (usize, f32),
    places: Vec<TextLines>,
    /// The label on one line, for the runs of the route.
    line: Option<TextLines>,
}

impl Texts {
    /// Marks texts that are always drawn.
    fn hold<'a>(&mut self, texts: impl Iterator<Item = &'a TextLines>) {
        self.taken.extend(texts.map(TextLines::rect));
    }

    /// Asks for a place for the label of `edge`, with the places to try in order; `at` orders
    /// the asks: the rank, then where the edge leaves its box.
    fn offer(
        &mut self,
        edge: usize,
        at: (usize, f32),
        places: Vec<TextLines>,
        line: Option<TextLines>,
    ) {
        let places: Vec<TextLines> = places.into_iter().filter(|l| !l.lines.is_empty()).collect();
        if !places.is_empty() {
            self.offers.push(Offer {
                edge,
                at,
                places,
                line,
            });
        }
    }

    /// Gives the label of `edge` new places to try, and nothing else to fall back on.
    fn replace(&mut self, edge: usize, places: Vec<TextLines>) {
        if let Some(offer) = self.offers.iter_mut().find(|o| o.edge == edge) {
            offer.places = places;
            offer.line = None;
        }
    }

    /// Takes the first free place of each label (see `Texts`); a label with no free place is
    /// left out, and counted.
    fn place(
        mut self,
        routes: &[(usize, &[Point])],
        boxes: &[Rect],
    ) -> (Vec<(usize, TextLines)>, u32) {
        let asked = self.offers.len();
        self.offers.sort_by(|a, b| {
            a.at.0
                .cmp(&b.at.0)
                .then(a.at.1.total_cmp(&b.at.1))
                .then(a.edge.cmp(&b.edge))
        });
        let mut placed = Vec::new();
        for offer in self.offers {
            let own: &[Point] = routes
                .iter()
                .find(|(e, _)| *e == offer.edge)
                .map_or(&[], |(_, points)| points);
            let free = |label: &TextLines| {
                let rect = label.rect();
                let inside = rect.inflate(-0.5);
                !self.taken.iter().any(|t| t.intersects(&rect))
                    // Half a pixel of room, so a label that sits on a box's edge stays off it
                    // once the drawing is moved to the margin.
                    && !boxes.iter().any(|b| b.intersects(&rect.inflate(0.5)))
                    && !crossed_by_other_route(routes, offer.edge, rect.inflate(1.0))
                    && !own.windows(2).any(|w| touches(w[0], w[1], inside))
            };
            let wrapped = offer.places[0].clone();
            let found = offer.places.iter().find(|l| free(l)).cloned().or_else(|| {
                offer
                    .line
                    .iter()
                    .chain(std::iter::once(&wrapped))
                    .flat_map(|block| beside_runs(own, block))
                    .find(|l| free(l))
            });
            if let Some(label) = found {
                self.taken.push(label.rect());
                placed.push((offer.edge, label));
            }
        }
        let dropped = (asked - placed.len()) as u32;
        (placed, dropped)
    }
}

/// `block` moved beside each run of `route`, the longest run first: above and below the middle
/// of a run that goes sideways, right and left of the middle of one that goes up or down.
fn beside_runs(route: &[Point], block: &TextLines) -> Vec<TextLines> {
    let mut runs: Vec<(Point, Point)> = route.windows(2).map(|w| (w[0], w[1])).collect();
    let length = |r: &(Point, Point)| (r.0.x - r.1.x).abs() + (r.0.y - r.1.y).abs();
    runs.sort_by(|a, b| length(b).total_cmp(&length(a)));
    let (w, h) = (block.width, block.height);
    let mut out = Vec::new();
    for (a, b) in runs {
        let mid = Point {
            x: (a.x + b.x) / 2.0,
            y: (a.y + b.y) / 2.0,
        };
        let spots = if (a.y - b.y).abs() < 1e-3 {
            [
                Point {
                    x: mid.x - w / 2.0,
                    y: mid.y - 2.0 - h,
                },
                Point {
                    x: mid.x - w / 2.0,
                    y: mid.y + 2.0,
                },
            ]
        } else {
            [
                Point {
                    x: mid.x + 6.0,
                    y: mid.y - h / 2.0,
                },
                Point {
                    x: mid.x - 6.0 - w,
                    y: mid.y - h / 2.0,
                },
            ]
        };
        for at in spots {
            let mut moved = block.clone();
            moved.at = at;
            out.push(moved);
        }
    }
    out
}

/// Moves a lane or a port by a few pixels where two edges would otherwise run along one line
/// in the same gap, so no two routes lie on top of each other.
fn separate_runs(
    chains: &[Chain],
    layer_of: &[usize],
    verts: &[Vertex],
    layers: &[Vec<usize>],
    c_top: &[f32],
    loops_at: &[usize],
    cross_of: &mut [Vec<f32>],
) {
    let mut by_gap: Vec<Vec<(usize, usize)>> = vec![Vec::new(); layers.len()];
    for (k, chain) in chains.iter().enumerate() {
        for i in 0..chain.verts.len() - 1 {
            by_gap[layer_of[chain.verts[i]]].push((k, i));
        }
    }
    // How many of the four pairs of runs of two hops lie on one line.
    let shared = |cross_of: &[Vec<f32>], a: (usize, usize), b: (usize, usize)| -> usize {
        let (p1, q1) = (cross_of[a.0][a.1], cross_of[a.0][a.1 + 1]);
        let (p2, q2) = (cross_of[b.0][b.1], cross_of[b.0][b.1 + 1]);
        [(p1, p2), (q1, q2), (p1, q2), (q1, p2)]
            .iter()
            .filter(|&&(x, y)| (x - y).abs() < 1.0)
            .count()
    };
    let clashes = |cross_of: &[Vec<f32>], k: usize| -> usize {
        let mut count = 0;
        for i in 0..chains[k].verts.len() - 1 {
            for &other in &by_gap[layer_of[chains[k].verts[i]]] {
                if other.0 != k {
                    count += shared(cross_of, (k, i), other);
                }
            }
        }
        count
    };
    // Where vertex `at` of chain `k` may stand: inside its box, or clear of the boxes of its
    // column when it is a lane.
    let allowed = |k: usize, at: usize, y: f32| -> bool {
        let v = chains[k].verts[at];
        if let Some(node) = verts[v].real {
            let ext = verts[v].ext_c;
            let free = ports_from(loops_at[node], ext).max(3.0);
            return y >= c_top[v] + free && y <= c_top[v] + ext - 3.0;
        }
        layers[layer_of[v]].iter().all(|&w| {
            verts[w].real.is_none() || y < c_top[w] - 4.0 || y > c_top[w] + verts[w].ext_c + 4.0
        })
    };
    for _ in 0..64 {
        let mut moved = false;
        for gap in &by_gap {
            'gap: for (x, &a) in gap.iter().enumerate() {
                for &b in &gap[x + 1..] {
                    if a.0 == b.0 || shared(cross_of, a, b) == 0 {
                        continue;
                    }
                    // The ends that stand in the way: lanes first, then the later edge.
                    let mut slots = [(b.0, b.1), (b.0, b.1 + 1), (a.0, a.1), (a.0, a.1 + 1)];
                    slots.sort_by_key(|&(k, at)| {
                        let lane = verts[chains[k].verts[at]].real.is_none();
                        (Reverse(lane), Reverse(k))
                    });
                    for (k, at) in slots {
                        let before = clashes(cross_of, k);
                        if before == 0 {
                            continue;
                        }
                        let home = cross_of[k][at];
                        for step in 1..=6 {
                            for sign in [1.0, -1.0] {
                                let y = home + sign * 4.0 * step as f32;
                                let beside = |j: usize| (y - cross_of[k][j]).abs() < 1.0;
                                let near = (at > 0 && beside(at - 1))
                                    || (at + 1 < cross_of[k].len() && beside(at + 1));
                                if near || !allowed(k, at, y) {
                                    continue;
                                }
                                cross_of[k][at] = y;
                                if clashes(cross_of, k) < before {
                                    moved = true;
                                    continue 'gap;
                                }
                            }
                        }
                        cross_of[k][at] = home;
                    }
                }
            }
        }
        if !moved {
            break;
        }
    }
}

/// Flags the edges to reverse so the graph has no cycle: the edges that close a loop in a
/// depth-first walk that follows the nodes and edges in spec order.
fn back_edges(n: usize, links: &[(usize, usize, usize)]) -> Vec<bool> {
    let mut out: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    for (k, &(_, a, b)) in links.iter().enumerate() {
        out[a].push((k, b));
    }
    let mut flagged = vec![false; links.len()];
    // 0 = not seen, 1 = on the walk's path, 2 = finished.
    let mut state = vec![0u8; n];
    for root in 0..n {
        if state[root] != 0 {
            continue;
        }
        state[root] = 1;
        let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
        while let Some(&(u, next)) = stack.last() {
            if next < out[u].len() {
                let (k, v) = out[u][next];
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                match state[v] {
                    0 => {
                        state[v] = 1;
                        stack.push((v, 0));
                    }
                    1 => flagged[k] = true,
                    _ => {}
                }
            } else {
                state[u] = 2;
                stack.pop();
            }
        }
    }
    flagged
}

/// The rank of each node: the longest path from a node with no incoming edge.
fn ranks(n: usize, dag: &[(usize, usize)]) -> Vec<usize> {
    let mut indegree = vec![0usize; n];
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(u, v) in dag {
        indegree[v] += 1;
        out[u].push(v);
    }
    let mut ready: VecDeque<usize> = (0..n).filter(|&v| indegree[v] == 0).collect();
    let mut rank = vec![0usize; n];
    while let Some(u) = ready.pop_front() {
        for &v in &out[u] {
            rank[v] = rank[v].max(rank[u] + 1);
            indegree[v] -= 1;
            if indegree[v] == 0 {
                ready.push_back(v);
            }
        }
    }
    rank
}

/// Orders the vertices inside each layer with barycenter sweeps, down then up, `SWEEPS` times
/// in all; a tie keeps the order the vertices already had.
fn order_layers(layers: &mut [Vec<usize>], up: &[Vec<usize>], down: &[Vec<usize>]) {
    let mut pos = vec![0usize; up.len()];
    let refresh = |layer: &[usize], pos: &mut [usize]| {
        for (i, &v) in layer.iter().enumerate() {
            pos[v] = i;
        }
    };
    for layer in layers.iter() {
        refresh(layer, &mut pos);
    }
    let count = layers.len();
    for sweep in 0..SWEEPS {
        let order: Vec<usize> = if sweep % 2 == 0 {
            (1..count).collect()
        } else {
            (0..count.saturating_sub(1)).rev().collect()
        };
        let nbrs = if sweep % 2 == 0 { up } else { down };
        for l in order {
            let mut keyed: Vec<(f64, usize, usize)> = layers[l]
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    let ns = &nbrs[v];
                    let key = if ns.is_empty() {
                        i as f64
                    } else {
                        ns.iter().map(|&w| pos[w] as f64).sum::<f64>() / ns.len() as f64
                    };
                    (key, i, v)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            layers[l] = keyed.into_iter().map(|(_, _, v)| v).collect();
            refresh(&layers[l], &mut pos);
        }
    }
}

/// Where each vertex starts on the cross axis. Layers start centred on each other, then a few
/// rounds pull every vertex towards the middle of its neighbours without breaking the gaps.
fn cross_positions(
    layers: &[Vec<usize>],
    verts: &[Vertex],
    up: &[Vec<usize>],
    down: &[Vec<usize>],
    node_gap: f32,
) -> Vec<f32> {
    let gap_between = |a: usize, b: usize| {
        if verts[a].real.is_none() || verts[b].real.is_none() {
            LANE_GAP.min(node_gap)
        } else {
            node_gap
        }
    };
    let extent = |layer: &[usize]| -> f32 {
        let mut total = 0.0;
        for (i, &v) in layer.iter().enumerate() {
            total += verts[v].ext_c;
            if i > 0 {
                total += gap_between(layer[i - 1], v);
            }
        }
        total
    };
    let widest = layers.iter().map(|l| extent(l)).fold(0.0, f32::max);
    let mut c = vec![0.0f32; verts.len()];
    for layer in layers {
        let mut at = (widest - extent(layer)) / 2.0;
        for (i, &v) in layer.iter().enumerate() {
            if i > 0 {
                at += gap_between(layer[i - 1], v);
            }
            c[v] = at;
            at += verts[v].ext_c;
        }
    }
    for _ in 0..RELAX_ROUNDS {
        for pass in 0..2 {
            let order: Vec<usize> = if pass == 0 {
                (1..layers.len()).collect()
            } else {
                (0..layers.len().saturating_sub(1)).rev().collect()
            };
            let nbrs = if pass == 0 { up } else { down };
            for l in order {
                let layer = &layers[l];
                let desired: Vec<f32> = layer
                    .iter()
                    .map(|&v| {
                        if nbrs[v].is_empty() {
                            c[v]
                        } else {
                            let mid = nbrs[v]
                                .iter()
                                .map(|&w| c[w] + verts[w].ext_c / 2.0)
                                .sum::<f32>()
                                / nbrs[v].len() as f32;
                            mid - verts[v].ext_c / 2.0
                        }
                    })
                    .collect();
                let m = layer.len();
                let mut forward = desired.clone();
                for i in 1..m {
                    let floor = forward[i - 1]
                        + verts[layer[i - 1]].ext_c
                        + gap_between(layer[i - 1], layer[i]);
                    forward[i] = forward[i].max(floor);
                }
                let mut backward = desired;
                for i in (0..m.saturating_sub(1)).rev() {
                    let ceil = backward[i + 1]
                        - gap_between(layer[i], layer[i + 1])
                        - verts[layer[i]].ext_c;
                    backward[i] = backward[i].min(ceil);
                }
                for (i, &v) in layer.iter().enumerate() {
                    c[v] = (forward[i] + backward[i]) / 2.0;
                }
            }
        }
    }
    c
}

/// Puts the nodes of each column next to each other, the spine node first, with the lanes of
/// the long edges that pass through the column above or below them.
fn gather_columns(layers: &mut [Vec<usize>], verts: &[Vertex], depth: &[usize]) {
    for layer in layers.iter_mut() {
        let mut members: Vec<(usize, usize)> = layer
            .iter()
            .enumerate()
            .filter_map(|(at, &v)| verts[v].real.map(|node| (depth[node], at)))
            .collect();
        if members.is_empty() {
            continue;
        }
        let middle = members.iter().map(|&(_, at)| at as f32).sum::<f32>() / members.len() as f32;
        members.sort();
        let stack: Vec<usize> = members.iter().map(|&(_, at)| layer[at]).collect();
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for (at, &v) in layer.iter().enumerate() {
            if verts[v].real.is_none() {
                if (at as f32) < middle {
                    before.push(v);
                } else {
                    after.push(v);
                }
            }
        }
        *layer = before.into_iter().chain(stack).chain(after).collect();
    }
}

/// Where each vertex starts on the cross axis when columns hold stacks. The spine nodes are
/// centred on one line, or all start at the same one when `top_aligned`. The nodes that hang
/// below them follow `STACK_GAP` apart, and the lanes of the long edges keep `LANE_GAP` from
/// the stack, above or below.
fn column_positions(layers: &[Vec<usize>], verts: &[Vertex], top_aligned: bool) -> Vec<f32> {
    let mut c = vec![0.0f32; verts.len()];
    for layer in layers {
        let Some(first) = layer.iter().position(|&v| verts[v].real.is_some()) else {
            continue;
        };
        let count = layer.iter().filter(|&&v| verts[v].real.is_some()).count();
        let mut y = if top_aligned {
            0.0
        } else {
            -verts[layer[first]].ext_c / 2.0
        };
        let top = y;
        for (i, &v) in layer[first..first + count].iter().enumerate() {
            if i > 0 {
                y += STACK_GAP;
            }
            c[v] = y;
            y += verts[v].ext_c;
        }
        for (j, &lane) in layer[..first].iter().enumerate() {
            c[lane] = top - LANE_GAP * (first - j) as f32;
        }
        for (j, &lane) in layer[first + count..].iter().enumerate() {
            c[lane] = y + LANE_GAP * (j + 1) as f32;
        }
    }
    c
}

/// Drops repeated points and the middle point of three in a line.
fn simplify(points: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut out: Vec<(f32, f32)> = Vec::new();
    for &p in points {
        if out.last() == Some(&p) {
            continue;
        }
        while out.len() >= 2 {
            let (a, b) = (out[out.len() - 2], out[out.len() - 1]);
            if (a.0 == b.0 && b.0 == p.0) || (a.1 == b.1 && b.1 == p.1) {
                out.pop();
            } else {
                break;
            }
        }
        out.push(p);
    }
    out
}

/// The label of an edge, in the free space at the start of the first gap after the source; when
/// the edge has cardinalities (`ends_w` is the widest) it sits under the route, between them.
#[allow(clippy::too_many_arguments)]
fn edge_label(
    measure: &dyn Measure,
    dir: Direction,
    style: TextStyle,
    text_in: &str,
    gap_start: f32,
    gap: Option<f32>,
    port: f32,
    ends_w: Option<f32>,
) -> TextLines {
    match dir {
        Direction::LeftToRight => {
            let width = gap.unwrap_or_else(|| dir.base_gap());
            let cap = match ends_w {
                Some(w) => width - 8.0 - 2.0 * (w + 4.0),
                None => width - 8.0,
            };
            let lines = text::wrap(measure, text_in, style, cap.max(24.0), 4);
            let mut block = text::block(measure, lines, style, Point::default(), None, Align::Left);
            block.at = match ends_w {
                Some(_) => Point {
                    x: gap_start + (width - block.width) / 2.0,
                    y: port + 2.0,
                },
                None => Point {
                    x: gap_start + 4.0,
                    y: port - 2.0 - block.height,
                },
            };
            block
        }
        Direction::TopToBottom => {
            let lines = text::wrap(measure, text_in, style, 160.0, 2);
            let mut block = text::block(measure, lines, style, Point::default(), None, Align::Left);
            block.at = Point {
                x: port + 6.0,
                y: gap_start + 4.0,
            };
            block
        }
    }
}

/// A cardinality as a block of text, not yet placed.
fn card_text(measure: &dyn Measure, text_in: Option<&str>) -> Option<TextLines> {
    let style = TextStyle::Card;
    let lines = text::wrap(measure, text_in?, style, 64.0, 1);
    if lines.is_empty() {
        return None;
    }
    Some(text::block(
        measure,
        lines,
        style,
        Point::default(),
        None,
        Align::Left,
    ))
}

/// The label of an edge that runs straight down a column, to the right of the run, centred in
/// the free span `(top, bottom)` between the two boxes and not reaching past `right`.
fn column_label(
    measure: &dyn Measure,
    style: TextStyle,
    text_in: &str,
    x: f32,
    right: f32,
    (top, bottom): (f32, f32),
) -> TextLines {
    let lines_fit = ((bottom - top) / measure.line_height(style))
        .floor()
        .max(0.0) as usize;
    let cap = (right - x).clamp(24.0, 160.0);
    let lines = text::wrap(measure, text_in, style, cap, lines_fit.min(4));
    let mut block = text::block(measure, lines, style, Point::default(), None, Align::Left);
    block.at = Point {
        x,
        y: top + ((bottom - top) - block.height) / 2.0,
    };
    block
}

/// A cardinality next to a box: above the route when it runs sideways, beside it when it
/// runs down. `start` is the end where the route leaves its box.
fn end_label(
    measure: &dyn Measure,
    dir: Direction,
    text_in: Option<&str>,
    start: bool,
    edge_a: f32,
    port: f32,
) -> Option<TextLines> {
    let mut block = card_text(measure, text_in)?;
    block.at = match (dir, start) {
        (Direction::LeftToRight, true) => Point {
            x: edge_a + 4.0,
            y: port - 2.0 - block.height,
        },
        (Direction::LeftToRight, false) => Point {
            x: edge_a - 4.0 - block.width,
            y: port - 2.0 - block.height,
        },
        (Direction::TopToBottom, true) => Point {
            x: port + 4.0,
            y: edge_a + 2.0,
        },
        (Direction::TopToBottom, false) => Point {
            x: port + 4.0,
            y: edge_a - 2.0 - block.height,
        },
    };
    Some(block)
}

/// The label of a self-loop, beside its far side.
#[allow(clippy::too_many_arguments)]
fn loop_label(
    measure: &dyn Measure,
    dir: Direction,
    style: TextStyle,
    text_in: &str,
    far: f32,
    out: f32,
    (y1, y2): (f32, f32),
    nth: usize,
) -> TextLines {
    match dir {
        Direction::LeftToRight => {
            let cap = (68.0 - 6.0 * nth.min(4) as f32).max(20.0);
            let lines = text::wrap(measure, text_in, style, cap, 4);
            let mut block = text::block(measure, lines, style, Point::default(), None, Align::Left);
            block.at = Point {
                x: out + 4.0,
                y: y1,
            };
            block
        }
        Direction::TopToBottom => {
            let lines = text::wrap(measure, text_in, style, 160.0, 2);
            let mut block = text::block(measure, lines, style, Point::default(), None, Align::Left);
            block.at = Point {
                x: y2 + 6.0,
                y: far + 4.0,
            };
            block
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::touches;

    fn card(text: &str, at: (f32, f32)) -> TextLines {
        text::block(
            &crate::measure::Fonts,
            vec![text.to_string()],
            TextStyle::Card,
            Point { x: at.0, y: at.1 },
            None,
            Align::Left,
        )
    }

    fn run(index: usize, points: &[(f32, f32)], start: Option<TextLines>) -> EdgeGeo {
        EdgeGeo {
            index,
            points: points.iter().map(|&(x, y)| Point { x, y }).collect(),
            arrow: Arrow::None,
            label: None,
            ends: [start, None],
            style: Default::default(),
            change: Default::default(),
            history: None,
        }
    }

    #[test]
    fn a_cardinality_clear_of_other_routes_stays_put() {
        let tag = card("1", (4.0, 80.0));
        let mut edges = vec![run(0, &[(0.0, 100.0), (200.0, 100.0)], Some(tag.clone()))];
        settle_cardinalities(&mut edges, &[]);
        assert_eq!(edges[0].ends[0], Some(tag));
    }

    #[test]
    fn a_cardinality_on_another_route_is_nudged_along_its_end_and_kept() {
        // A vertical route passes through the place the tag asked for.
        let tag = card("0..N", (4.0, 84.0));
        let mut edges = vec![
            run(0, &[(0.0, 100.0), (200.0, 100.0)], Some(tag.clone())),
            run(1, &[(8.0, 40.0), (8.0, 160.0)], None),
        ];
        settle_cardinalities(&mut edges, &[]);
        let moved = edges[0].ends[0].as_ref().expect("nudged, not dropped");
        assert_eq!(moved.lines, tag.lines);
        assert_ne!(moved.at, tag.at);
        let zone = moved.rect().inflate(1.0);
        for w in edges[1].points.windows(2) {
            assert!(!touches(w[0], w[1], zone));
        }
    }

    #[test]
    fn a_cardinality_with_no_place_left_is_dropped() {
        let tag = card("1", (4.0, 84.0));
        let mut edges = vec![
            run(0, &[(0.0, 100.0), (200.0, 100.0)], Some(tag)),
            run(1, &[(8.0, 40.0), (8.0, 160.0)], None),
        ];
        let wall = Rect {
            x: -100.0,
            y: -100.0,
            w: 600.0,
            h: 600.0,
        };
        assert_eq!(settle_cardinalities(&mut edges, &[wall]), 1, "counted");
        assert!(edges[0].ends[0].is_none());
    }
}
