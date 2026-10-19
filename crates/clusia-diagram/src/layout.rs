//! `layout`: sizes the boxes of a spec, places them and assembles the `Geometry`.

use clusia_core::diagram::{Change, DiagramKind, DiagramSpec, Node, Shape};

use crate::geometry::{Align, Arrow, BoxGeo, Geometry, Point, Rect, TextLines};
use crate::layered::{self, Direction, NodeSize, Options};
use crate::measure::{Measure, TextStyle};
use crate::text;
use crate::{sequence, tables, usecase};

/// Space around the drawing; it starts at `(MARGIN, MARGIN)`.
pub const MARGIN: f32 = 24.0;

const PAD: f32 = 14.0;
/// The tighter padding of a sequence participant.
const SEQUENCE_PAD: f32 = 12.0;
const TITLE_SUB_GAP: f32 = 2.0;
const MIN_WIDTH: f32 = 160.0;
const MAX_WIDTH: f32 = 240.0;
const BOX_WIDTH: f32 = 200.0;
const PILL_WIDTH: f32 = 170.0;
const PILL_HEIGHT: f32 = 56.0;
const CIRCLE: f32 = 140.0;
const CIRCLE_INSET: f32 = 20.0;
const DOT: f32 = 18.0;
const TITLE_LINES: usize = 3;
const SUB_LINES: usize = 2;

/// Turns `spec` into boxes with wrapped text and routed edges. Pure and deterministic: the
/// same spec and measure always give the same geometry, down to the bit.
pub fn layout(spec: &DiagramSpec, measure: &dyn Measure) -> Geometry {
    let mut geometry = match spec.kind {
        DiagramKind::Graph => {
            layered_geometry(spec, measure, Direction::TopToBottom, Arrow::End, None)
        }
        DiagramKind::Flow | DiagramKind::DataFlow | DiagramKind::States => {
            layered_geometry(spec, measure, Direction::LeftToRight, Arrow::End, None)
        }
        DiagramKind::EntityRelationship => {
            layered_geometry(spec, measure, Direction::LeftToRight, Arrow::None, None)
        }
        DiagramKind::Classes => {
            layered_geometry(spec, measure, Direction::LeftToRight, Arrow::End, None)
        }
        DiagramKind::Sequence => sequence::layout(spec, measure),
        DiagramKind::UseCases => usecase::layout(spec, measure),
        DiagramKind::Keywords => Geometry::empty(),
    };
    geometry.normalise(MARGIN);
    geometry
}

/// Lays `spec` out top-down like a `Graph`, but with `ranks[i]` as the rank of `spec.nodes[i]`
/// instead of the longest-path rank. A missing entry counts as 0 and extra ones are ignored;
/// the distinct ranks are renumbered densely, so an empty level leaves no blank row. The spec
/// is not validated.
pub fn layout_ranked(spec: &DiagramSpec, ranks: &[usize], measure: &dyn Measure) -> Geometry {
    let given: Vec<usize> = (0..spec.nodes.len())
        .map(|i| ranks.get(i).copied().unwrap_or(0))
        .collect();
    let mut distinct = given.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let dense: Vec<usize> = given
        .iter()
        .map(|r| distinct.binary_search(r).unwrap_or(0))
        .collect();
    let mut geometry = layered_geometry(
        spec,
        measure,
        Direction::TopToBottom,
        Arrow::End,
        Some(&dense),
    );
    geometry.normalise(MARGIN);
    geometry
}

/// A node's size and wrapped text before it has a place.
pub(crate) struct Card {
    pub(crate) w: f32,
    pub(crate) h: f32,
    pub(crate) fixed: bool,
    centred: bool,
    inset: f32,
    title: Vec<String>,
    sub: Vec<String>,
    title_style: TextStyle,
    sub_style: TextStyle,
}

impl Card {
    fn text_height(&self, measure: &dyn Measure) -> f32 {
        let title = self.title.len() as f32 * measure.line_height(self.title_style);
        if self.sub.is_empty() {
            title
        } else {
            title + TITLE_SUB_GAP + self.sub.len() as f32 * measure.line_height(self.sub_style)
        }
    }
}

pub(crate) fn card(kind: DiagramKind, node: &Node, measure: &dyn Measure) -> Card {
    let title_style = TextStyle::Title;
    let sub_style = if node.change == Change::Unchanged {
        TextStyle::Sub
    } else {
        TextStyle::SubStrong
    };
    let sub_text = node.sub.as_deref().unwrap_or("");
    let centred = match node.shape {
        Shape::Pill | Shape::Circle | Shape::Actor | Shape::Start => true,
        Shape::External => kind == DiagramKind::DataFlow,
        _ => matches!(kind, DiagramKind::Sequence | DiagramKind::UseCases),
    };
    let pad = if kind == DiagramKind::Sequence {
        SEQUENCE_PAD
    } else {
        PAD
    };
    let mut card = Card {
        w: 0.0,
        h: 0.0,
        fixed: false,
        centred,
        inset: pad,
        title: Vec::new(),
        sub: Vec::new(),
        title_style,
        sub_style,
    };
    match node.shape {
        Shape::Start => {
            card.w = DOT;
            card.h = DOT;
            card.fixed = true;
        }
        Shape::Circle => {
            let inner = CIRCLE - 2.0 * CIRCLE_INSET;
            card.w = CIRCLE;
            card.h = CIRCLE;
            card.fixed = true;
            card.inset = CIRCLE_INSET;
            card.title = text::wrap(measure, &node.label, title_style, inner, 2);
            card.sub = text::wrap(measure, sub_text, sub_style, inner, 2);
        }
        _ => {
            let (base, min_w, max_w) = match (kind, node.shape) {
                (DiagramKind::Sequence, _) => (130.0, 120.0, 180.0),
                (DiagramKind::UseCases, Shape::Pill) => (190.0, MIN_WIDTH, MAX_WIDTH),
                (DiagramKind::UseCases, _) => (150.0, 140.0, 200.0),
                (_, Shape::Pill) => (PILL_WIDTH, MIN_WIDTH, MAX_WIDTH),
                _ => (BOX_WIDTH, MIN_WIDTH, MAX_WIDTH),
            };
            let longest_word = |text: &str, style: TextStyle| {
                text.split_whitespace()
                    .map(|w| measure.width(w, style))
                    .fold(0.0, f32::max)
            };
            // A participant's or an actor's name keeps to one line while the widest box allows
            // it.
            let one_line = kind == DiagramKind::Sequence
                || (kind == DiagramKind::UseCases && node.shape != Shape::Pill);
            let title_w = if one_line {
                measure.width(&node.label, title_style)
            } else {
                longest_word(&node.label, title_style)
            };
            let wanted = title_w.max(longest_word(sub_text, sub_style)) + 2.0 * pad;
            card.w = base.max(wanted).clamp(min_w, max_w);
            // A hair of room, so a name that set the width is not wrapped by a rounding error.
            let inner = card.w - 2.0 * pad + 0.01;
            card.title = text::wrap(measure, &node.label, title_style, inner, TITLE_LINES);
            card.sub = text::wrap(measure, sub_text, sub_style, inner, SUB_LINES);
            let min_h = match (kind, node.shape) {
                (_, Shape::Pill) | (DiagramKind::UseCases, Shape::Actor | Shape::External) => {
                    PILL_HEIGHT
                }
                _ => 0.0,
            };
            card.h = (2.0 * pad + card.text_height(measure)).max(min_h);
        }
    }
    card
}

fn layered_geometry(
    spec: &DiagramSpec,
    measure: &dyn Measure,
    direction: Direction,
    arrow: Arrow,
    ranks: Option<&[usize]>,
) -> Geometry {
    let tables = matches!(
        spec.kind,
        DiagramKind::EntityRelationship | DiagramKind::Classes
    );
    let cards: Vec<Card> = spec
        .nodes
        .iter()
        .map(|n| card(spec.kind, n, measure))
        .collect();
    let tabular: Vec<Option<tables::Table>> = spec
        .nodes
        .iter()
        .map(|n| {
            (tables && matches!(n.shape, Shape::Table | Shape::Class))
                .then(|| tables::table(spec.kind, n, measure))
        })
        .collect();
    let sizes: Vec<NodeSize> = cards
        .iter()
        .zip(&tabular)
        .map(|(c, t)| match t {
            Some(t) => NodeSize {
                w: t.w,
                h: t.h,
                fixed: true,
            },
            None => NodeSize {
                w: c.w,
                h: c.h,
                fixed: c.fixed,
            },
        })
        .collect();
    let label_style = if spec.kind == DiagramKind::Flow {
        TextStyle::EdgeLabelMono
    } else {
        TextStyle::EdgeLabel
    };
    let options = Options {
        direction,
        arrow,
        top_aligned: tables,
        label_style,
        ranks,
    };
    let placed = layered::place(spec, &sizes, measure, &options);

    let mut geometry = Geometry::empty();
    geometry.dropped_texts = placed.dropped_texts;
    for (i, (node, card)) in spec.nodes.iter().zip(&cards).enumerate() {
        let rect = placed.rects[i];
        let (title, sub, rows) = match &tabular[i] {
            Some(t) => t.geometry(node, rect, measure),
            None => {
                let (title, sub) = box_text(card, rect, measure);
                (title, sub, Vec::new())
            }
        };
        if node.shape == Shape::Start {
            geometry.start_dot.get_or_insert(rect.centre());
        }
        geometry.boxes.push(BoxGeo {
            node: node.id.clone(),
            index: i,
            rect,
            shape: node.shape,
            title,
            sub,
            rows,
            change: node.change,
            history: node.history,
            anchored: node.anchor.is_some(),
        });
    }
    geometry.edges = placed.edges;
    geometry
}

/// The title and sub text of a card inside its final rectangle: at the top left for a left
/// aligned shape, in the middle for a centred one.
pub(crate) fn box_text(
    card: &Card,
    rect: Rect,
    measure: &dyn Measure,
) -> (TextLines, Option<TextLines>) {
    let width = rect.w - 2.0 * card.inset;
    let align = if card.centred {
        Align::Center
    } else {
        Align::Left
    };
    let content = card.text_height(measure);
    let top = if card.centred {
        rect.y + (rect.h - content) / 2.0
    } else {
        rect.y + card.inset
    };
    let x = rect.x + card.inset;
    let title = text::block(
        measure,
        card.title.clone(),
        card.title_style,
        Point { x, y: top },
        Some(width),
        align,
    );
    let sub = if card.sub.is_empty() {
        None
    } else {
        Some(text::block(
            measure,
            card.sub.clone(),
            card.sub_style,
            Point {
                x,
                y: top + title.height + TITLE_SUB_GAP,
            },
            Some(width),
            align,
        ))
    };
    (title, sub)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::measure::Fonts;
    use crate::testkit::{
        assert_no_text_clash, check, data_flow_mockup, deps_mockup, edge, flow_mockup, node,
        node_sub, random_graph, sparse_graph, spec, states_mockup,
    };
    use std::time::Instant;

    fn run(spec: &DiagramSpec) -> Geometry {
        let g = layout(spec, &Fonts);
        check(spec, &g);
        g
    }

    fn rect_of<'a>(g: &'a Geometry, id: &str) -> &'a Rect {
        &g.boxes.iter().find(|b| b.node == id).expect("box").rect
    }

    /// Whether `lower` stands in the same column as `upper`, 72 px under it.
    fn hangs_under(lower: &Rect, upper: &Rect) -> bool {
        lower.x == upper.x && (lower.y - upper.bottom() - 72.0).abs() < 0.01
    }

    #[test]
    fn the_flow_mockup_is_a_row_with_lock_under_refresh_and_save_under_oauth() {
        let s = flow_mockup();
        let g = run(&s);
        let client = rect_of(&g, "client");
        let refresh = rect_of(&g, "refresh");
        let oauth = rect_of(&g, "oauth");
        let lock = rect_of(&g, "lock");
        let save = rect_of(&g, "save");
        assert!(
            refresh.x >= client.right() + 96.0 - 0.01,
            "a rank gap of 96 px"
        );
        assert!(oauth.x >= refresh.right() + 96.0 - 0.01);
        assert!(hangs_under(lock, refresh));
        assert!(hangs_under(save, oauth));
        let centre = |r: &Rect| r.centre().y;
        assert_eq!(centre(client), centre(refresh));
        assert_eq!(centre(refresh), centre(oauth));
        assert_eq!(client.w, 200.0);
        assert_eq!(client.x, MARGIN);
        assert_eq!(g.edges.len(), 4);
        assert!(g.edges.iter().all(|e| e.arrow == Arrow::End));
        let label = g.edges[0].label.as_ref().expect("label");
        assert_eq!(label.lines, vec!["token()".to_string()]);
        assert!(g.edges[3].label.is_none());
        let down = &g.edges[2].points;
        assert_eq!(down.len(), 2, "a straight run down the column");
        assert_eq!(down[0].x, down[1].x);
        assert_eq!(down[0].y, refresh.bottom());
        assert_eq!(down[1].y, lock.y);
        assert_eq!(g.edges[2].label.as_ref().expect("label").lines, vec!["new"]);
        assert_no_text_clash(&g);
    }

    #[test]
    fn the_data_flow_mockup_keeps_the_file_under_the_circles() {
        let s = data_flow_mockup();
        let g = run(&s);
        let (refresh, exchange) = (rect_of(&g, "refresh"), rect_of(&g, "exchange"));
        let (client, github, file) = (
            rect_of(&g, "client"),
            rect_of(&g, "github"),
            rect_of(&g, "file"),
        );
        assert!(client.right() < refresh.x && exchange.right() < github.x);
        assert_eq!(refresh.centre().y, exchange.centre().y);
        assert_eq!(client.centre().y, refresh.centre().y);
        assert!(hangs_under(file, exchange));
        assert!(file.centre().x > refresh.x && file.centre().x < exchange.right());
        assert_no_text_clash(&g);
    }

    #[test]
    fn a_label_stays_on_one_line_when_its_gap_can_grow_for_it() {
        let g = run(&data_flow_mockup());
        for e in &g.edges {
            let label = e.label.as_ref().expect("label");
            assert_eq!(label.lines.len(), 1, "{:?}", label.lines);
        }
        let back = g.edges.iter().find(|e| e.index == 3).expect("edge");
        assert_eq!(
            back.points.len(),
            2,
            "a way back to the next box runs straight beside the way there"
        );
    }

    #[test]
    fn the_states_mockup_keeps_failed_under_refreshing() {
        let s = states_mockup();
        let g = run(&s);
        let (fresh, refreshing, failed) = (
            rect_of(&g, "fresh"),
            rect_of(&g, "refreshing"),
            rect_of(&g, "failed"),
        );
        assert!(hangs_under(failed, refreshing));
        assert_eq!(fresh.centre().y, refreshing.centre().y);
        assert!(rect_of(&g, "start").right() < fresh.x);
        let back = g.edges.iter().find(|e| e.index == 3).expect("edge");
        let lane = back.points.iter().map(|p| p.y).fold(f32::MAX, f32::min);
        assert!(lane < fresh.y, "the way back to Fresh runs above the row");
        assert_eq!(
            back.points[0],
            Point {
                x: refreshing.centre().x,
                y: refreshing.y
            },
            "it leaves the top of Refreshing"
        );
        assert_eq!(
            back.points[back.points.len() - 1],
            Point {
                x: fresh.centre().x,
                y: fresh.y
            },
            "and comes down into the top of Fresh"
        );
        let label = back.label.as_ref().expect("label");
        assert!(
            label.rect().bottom() <= lane,
            "its label sits on the top run"
        );
        let expiring = rect_of(&g, "expiring");
        let retry = g.edges.iter().find(|e| e.index == 5).expect("edge");
        assert_eq!(retry.points[0].x, failed.x, "retry leaves Failed sideways");
        assert_eq!(
            retry.points[retry.points.len() - 1],
            Point {
                x: expiring.centre().x,
                y: expiring.bottom()
            },
            "and goes up into Expiring"
        );
        assert_no_text_clash(&g);
    }

    #[test]
    fn the_graph_mockup_still_runs_top_to_bottom() {
        let s = deps_mockup();
        let g = run(&s);
        let (cli, auth, config) = (
            rect_of(&g, "cli"),
            rect_of(&g, "auth"),
            rect_of(&g, "config"),
        );
        assert!(cli.bottom() + 72.0 - 0.01 <= auth.y);
        assert!(auth.bottom() + 72.0 - 0.01 <= config.y);
        assert_no_text_clash(&g);
    }

    #[test]
    fn a_chain_with_no_branch_stays_one_row_of_ranks() {
        let s = spec(
            "flow",
            (0..5)
                .map(|i| node(&format!("n{i}"), &format!("Step {i}"), "box"))
                .collect(),
            (0..4)
                .map(|i| edge(&format!("n{i}"), &format!("n{}", i + 1), None))
                .collect(),
        );
        let g = run(&s);
        let tops: Vec<f32> = g.boxes.iter().map(|b| b.rect.y).collect();
        assert!(tops.windows(2).all(|w| w[0] == w[1]));
        assert!(g.boxes.windows(2).all(|w| w[0].rect.right() < w[1].rect.x));
    }

    #[test]
    fn a_diagram_with_a_part_off_the_spine_falls_back_to_ranks() {
        let s = spec(
            "flow",
            vec![
                node("a", "A", "box"),
                node("b", "B", "box"),
                node("c", "C", "box"),
                node("d", "D", "box"),
                node("e", "E", "box"),
            ],
            vec![
                edge("a", "b", None),
                edge("b", "c", None),
                edge("b", "d", None),
                edge("d", "e", None),
                edge("e", "d", None),
                edge("c", "e", None),
            ],
        );
        let g = run(&s);
        assert_eq!(g.edges.len(), 6);
        let (b, d) = (rect_of(&g, "b"), rect_of(&g, "d"));
        assert!(b.right() < d.x);
    }

    #[test]
    fn a_label_sits_under_the_route_between_the_cardinalities() {
        let mut s = flow_mockup();
        s.edges[1].ends = [Some("1".into()), Some("0..N".into())];
        let g = run(&s);
        let e = &g.edges[1];
        let (label, from, to) = (
            e.label.as_ref().expect("label"),
            e.ends[0].as_ref().expect("from"),
            e.ends[1].as_ref().expect("to"),
        );
        let run_y = e.points[0].y;
        assert!(label.at.y > run_y, "under the run");
        assert!(
            from.rect().bottom() <= run_y && to.rect().bottom() <= e.points[e.points.len() - 1].y
        );
        assert!(!label.rect().intersects(&from.rect()));
        assert!(!label.rect().intersects(&to.rect()));
    }

    #[test]
    fn labels_that_would_print_over_each_other_are_left_out() {
        let nodes: Vec<_> = std::iter::once(node("hub", "Hub", "box"))
            .chain((0..20).map(|i| node(&format!("s{i}"), &format!("Spoke {i}"), "box")))
            .collect();
        let edges = (0..20)
            .map(|i| edge("hub", &format!("s{i}"), Some("calls")))
            .collect();
        let s = spec("flow", nodes, edges);
        let g = run(&s);
        let labels: Vec<_> = g.edges.iter().filter_map(|e| e.label.as_ref()).collect();
        assert!(!labels.is_empty() && labels.len() < 20);
        assert_eq!(
            g.dropped_texts as usize,
            20 - labels.len(),
            "the left-out labels are counted"
        );
        for (i, a) in labels.iter().enumerate() {
            for b in &labels[i + 1..] {
                assert!(!a.rect().intersects(&b.rect()));
            }
        }
    }

    #[test]
    fn code_kinds_label_their_edges_in_mono_and_the_others_in_the_ui_face() {
        let flow = run(&flow_mockup());
        assert_eq!(
            flow.edges[0].label.as_ref().expect("label").style,
            TextStyle::EdgeLabelMono
        );
        for s in [deps_mockup(), data_flow_mockup(), states_mockup()] {
            let g = run(&s);
            for e in &g.edges {
                if let Some(label) = &e.label {
                    assert_eq!(label.style, TextStyle::EdgeLabel, "{:?}", s.kind);
                }
            }
        }
    }

    #[test]
    fn a_box_keeps_14_px_around_its_text() {
        let g = run(&flow_mockup());
        let refresh = g.boxes.iter().find(|b| b.node == "refresh").expect("box");
        assert_eq!(refresh.title.at.x - refresh.rect.x, 14.0);
        assert_eq!(refresh.title.at.y - refresh.rect.y, 14.0);
        let sub = refresh.sub.as_ref().expect("sub");
        assert_eq!(refresh.rect.bottom() - sub.rect().bottom(), 14.0);
        assert_eq!(refresh.rect.h, 60.0, "title, sub and padding");
    }

    #[test]
    fn a_name_that_sets_its_box_width_is_not_wrapped() {
        let s = spec(
            "use_cases",
            vec![node("r", "«actor» Reviewer", "actor")],
            vec![],
        );
        let c = card(s.kind, &s.nodes[0], &Fonts);
        assert_eq!(c.title, vec!["«actor» Reviewer".to_string()]);
    }

    #[test]
    fn box_text_wraps_inside_the_box_and_sub_text_follows_the_title() {
        let s = spec(
            "flow",
            vec![node_sub(
                "a",
                "An unusually long module title that has to wrap onto more lines",
                "and a sub line",
                "box",
                "changed",
            )],
            vec![],
        );
        let g = run(&s);
        let b = &g.boxes[0];
        assert!(b.title.lines.len() >= 2 && b.title.lines.len() <= 3);
        assert!(b.title.at.x >= b.rect.x + 13.99);
        assert!(b.title.rect().right() <= b.rect.right());
        let sub = b.sub.as_ref().expect("sub");
        assert!(sub.at.y >= b.title.rect().bottom());
        assert!(sub.rect().bottom() <= b.rect.bottom());
        assert_eq!(sub.style, TextStyle::SubStrong);
        assert_eq!(b.title.align, Align::Left);
        assert_eq!(b.change, Change::Changed);
    }

    #[test]
    fn a_word_with_no_spaces_cannot_stretch_a_box() {
        let word = "x".repeat(120);
        let s = spec(
            "flow",
            vec![node("a", &word, "box"), node("b", "B", "box")],
            vec![edge("a", "b", Some(&word))],
        );
        let g = run(&s);
        assert!(g.boxes[0].rect.w <= 240.0);
        assert!(g.boxes[0].title.lines.len() <= 3);
        let label = g.edges[0].label.as_ref().expect("label");
        assert!(label.lines.len() <= 4);
        assert!(
            label
                .lines
                .iter()
                .all(|l| Fonts.width(l, TextStyle::EdgeLabelMono) <= 164.5)
        );
    }

    #[test]
    fn graph_runs_top_to_bottom_with_72_px_between_ranks() {
        let s = spec(
            "graph",
            vec![
                node("cli", "cli", "box"),
                node("auth", "auth", "box"),
                node("config", "config", "box"),
            ],
            vec![
                edge("cli", "auth", None),
                edge("auth", "config", Some("reads refresh margin")),
            ],
        );
        let g = run(&s);
        let (cli, auth, config) = (
            rect_of(&g, "cli"),
            rect_of(&g, "auth"),
            rect_of(&g, "config"),
        );
        assert!(auth.y >= cli.bottom() + 72.0 - 0.01);
        assert!(config.y >= auth.bottom() + 72.0 - 0.01);
        let label = g.edges[1].label.as_ref().expect("label");
        assert!(label.lines.len() <= 2);
        let first = g.edges[0].points[0];
        assert!(
            (first.y - cli.bottom()).abs() < 1e-3,
            "an edge leaves the bottom side"
        );
    }

    #[test]
    fn a_cycle_is_laid_out_and_its_arrow_points_at_the_right_box() {
        let s = spec(
            "flow",
            vec![
                node("a", "A", "box"),
                node("b", "B", "box"),
                node("c", "C", "box"),
            ],
            vec![
                edge("a", "b", None),
                edge("b", "c", None),
                edge("c", "a", Some("again")),
            ],
        );
        let g = run(&s);
        let a = rect_of(&g, "a");
        let back = g
            .edges
            .iter()
            .find(|e| e.index == 2)
            .expect("the back edge");
        let end = back.points.last().expect("points");
        assert_eq!(
            *end,
            Point {
                x: a.centre().x,
                y: a.y
            },
            "the arrow comes down into the top of a"
        );
        let c = rect_of(&g, "c");
        assert_eq!(
            back.points[0],
            Point {
                x: c.centre().x,
                y: c.y
            },
            "over the row from the top of c"
        );
    }

    #[test]
    fn a_self_loop_has_three_segments_on_the_right_side() {
        let s = spec(
            "flow",
            vec![node("a", "Retry", "box")],
            vec![edge("a", "a", Some("again"))],
        );
        let g = run(&s);
        let e = &g.edges[0];
        assert_eq!(e.points.len(), 4);
        let a = rect_of(&g, "a");
        assert!(
            (e.points[0].x - a.right()).abs() < 1e-3 && (e.points[3].x - a.right()).abs() < 1e-3
        );
        assert!(e.points[1].x > a.right());
        assert!(e.label.is_some());
    }

    #[test]
    fn every_loop_on_a_small_box_returns_to_the_box() {
        let s = spec(
            "states",
            vec![node("start", "", "start"), node("a", "Ready", "pill")],
            vec![
                edge("start", "a", None),
                edge("start", "start", None),
                edge("start", "start", None),
                edge("a", "a", None),
                edge("a", "a", None),
                edge("a", "a", None),
                edge("a", "a", None),
                edge("a", "a", None),
                edge("a", "a", None),
                edge("a", "a", None),
            ],
        );
        let g = run(&s);
        for e in g.edges.iter().filter(|e| e.points.len() == 4) {
            let owner = if e.index < 3 { "start" } else { "a" };
            let r = rect_of(&g, owner);
            for p in [e.points[0], e.points[3]] {
                assert!(
                    p.y >= r.y - 1e-3 && p.y <= r.bottom() + 1e-3,
                    "{p:?} off {r:?}"
                );
            }
        }
    }

    #[test]
    fn an_edge_over_several_ranks_runs_through_a_lane_not_a_box() {
        let s = spec(
            "flow",
            vec![
                node("a", "A", "box"),
                node("b", "B", "box"),
                node("c", "C", "box"),
                node("d", "D", "box"),
            ],
            vec![
                edge("a", "b", None),
                edge("b", "c", None),
                edge("c", "d", None),
                edge("a", "d", Some("skips two ranks")),
            ],
        );
        let g = run(&s);
        let long = g.edges.iter().find(|e| e.index == 3).expect("long edge");
        assert!(
            long.points.len() >= 4,
            "it jogs around the boxes in between: {:?}",
            long.points
        );
    }

    #[test]
    fn parallel_edges_leave_through_different_ports() {
        let s = spec(
            "flow",
            vec![node("a", "A", "box"), node("b", "B", "box")],
            vec![
                edge("a", "b", Some("one")),
                edge("a", "b", Some("two")),
                edge("b", "a", None),
            ],
        );
        let g = run(&s);
        let ys: Vec<f32> = g.edges.iter().map(|e| e.points[0].y).collect();
        assert!(
            ys[0] != ys[1],
            "two routes out of one side never share a line: {ys:?}"
        );
    }

    #[test]
    fn a_hub_with_twenty_edges_grows_to_hold_its_ports() {
        let mut nodes = vec![node("hub", "Hub", "box")];
        let mut edges = Vec::new();
        for i in 0..20 {
            nodes.push(node(&format!("n{i}"), &format!("Leaf {i}"), "box"));
            edges.push(edge("hub", &format!("n{i}"), None));
            edges.push(edge(&format!("n{i}"), "hub", None));
        }
        let g = run(&spec("flow", nodes, edges));
        assert!(rect_of(&g, "hub").h >= 8.0 * 21.0 - 0.01);
    }

    #[test]
    fn disconnected_parts_and_unknown_ends_are_laid_out() {
        let s = spec(
            "flow",
            vec![
                node("a", "A", "box"),
                node("b", "B", "box"),
                node("c", "Alone", "box"),
            ],
            vec![
                edge("a", "b", None),
                edge("a", "ghost", None),
                edge("ghost", "b", None),
            ],
        );
        let g = run(&s);
        assert_eq!(g.edges.len(), 1, "an edge with an unknown end is not drawn");
        assert_eq!(g.boxes.len(), 3);
    }

    #[test]
    fn an_empty_spec_is_two_margins() {
        let g = run(&spec("flow", vec![], vec![]));
        assert_eq!(g.size, (48.0, 48.0));
        assert!(g.boxes.is_empty());
    }

    #[test]
    fn duplicate_node_ids_do_not_panic() {
        let s = spec(
            "flow",
            vec![node("a", "A", "box"), node("a", "A again", "box")],
            vec![edge("a", "a", None)],
        );
        let g = layout(&s, &Fonts);
        assert_eq!(g.boxes.len(), 2);
    }

    #[test]
    fn states_have_pills_and_a_start_dot() {
        let s = spec(
            "states",
            vec![
                node("start", "", "start"),
                node("fresh", "Fresh", "pill"),
                node_sub("expiring", "Expiring", "within 60 s · new", "pill", "new"),
            ],
            vec![
                edge("start", "fresh", None),
                edge("fresh", "expiring", Some("time passes")),
                edge("expiring", "fresh", Some("new token saved")),
            ],
        );
        let g = run(&s);
        let dot = g.start_dot.expect("a start dot");
        let start = rect_of(&g, "start");
        assert_eq!((start.w, start.h), (18.0, 18.0));
        assert_eq!(dot, start.centre());
        let fresh = &g.boxes[1];
        assert!(fresh.rect.h >= 56.0);
        assert_eq!(fresh.title.align, Align::Center);
        assert!(
            fresh.title.at.y > fresh.rect.y,
            "centred text sits in the middle"
        );
    }

    #[test]
    fn data_flow_circles_are_140_px_wide_and_tall() {
        let s = spec(
            "data_flow",
            vec![
                node_sub("client", "API client", "external", "external", "unchanged"),
                node_sub("refresh", "1  Refresh", "margin 60 s", "circle", "changed"),
                node_sub(
                    "file",
                    "D1  |  Token file",
                    "written atomically",
                    "store",
                    "unchanged",
                ),
            ],
            vec![
                edge("client", "refresh", Some("token request")),
                edge("refresh", "file", Some("write")),
            ],
        );
        let g = run(&s);
        let c = rect_of(&g, "refresh");
        assert_eq!((c.w, c.h), (140.0, 140.0));
        assert_eq!(g.boxes[1].title.align, Align::Center);
        assert_eq!(
            g.boxes[0].title.align,
            Align::Center,
            "an external node is centred in a data flow"
        );
    }

    #[test]
    fn anchored_boxes_are_flagged() {
        let mut s = flow_mockup();
        s.nodes[0].anchor = Some(clusia_core::diagram::CodeAnchor {
            path: "client/http.rs".into(),
            start_line: 1,
            end_line: 4,
        });
        let g = run(&s);
        assert!(g.boxes[0].anchored && !g.boxes[1].anchored);
    }

    #[test]
    fn the_same_spec_gives_the_same_geometry() {
        let s = random_graph("flow", 30, 60, 7);
        assert_eq!(layout(&s, &Fonts), layout(&s, &Fonts));
    }

    #[test]
    fn awkward_random_graphs_keep_every_invariant_inside_the_budget() {
        for (kind, seed) in [("flow", 1), ("graph", 2), ("data_flow", 3), ("states", 4)] {
            let s = random_graph(kind, 40, 80, seed);
            let started = Instant::now();
            let g = layout(&s, &Fonts);
            let took = started.elapsed();
            let budget = if cfg!(debug_assertions) { 200 } else { 20 };
            assert!(took.as_millis() <= budget, "{kind}: {took:?}");
            check(&s, &g);
        }
    }

    #[test]
    fn a_hub_with_every_edge_and_eighty_edges_between_two_boxes_stay_inside_the_budget() {
        let budget = if cfg!(debug_assertions) { 200 } else { 20 };
        let hub = |kind: &str, shape: &str| {
            let nodes: Vec<_> = (0..40)
                .map(|i| node(&format!("n{i}"), &format!("Part {i}"), shape))
                .collect();
            let edges: Vec<_> = (1..40)
                .flat_map(|i| {
                    let other = format!("n{i}");
                    [
                        edge("n0", &other, Some("reads")),
                        edge(&other, "n0", Some("writes")),
                    ]
                })
                .collect();
            spec(kind, nodes, edges)
        };
        let pair = spec(
            "flow",
            vec![node("a", "Client", "box"), node("b", "Server", "box")],
            (0..80)
                .map(|i| {
                    if i % 2 == 0 {
                        edge("a", "b", Some("call"))
                    } else {
                        edge("b", "a", None)
                    }
                })
                .collect(),
        );
        let one_way = |kind: &str, shape: &str| {
            let mut s = hub(kind, shape);
            s.edges.retain(|e| e.from == "n0");
            s
        };
        // A circle or a table keeps its size, so a hub of either with a route each way to every
        // other node crowds its ports until two routes share a line: those two are held to the
        // time only.
        for (name, s, valid) in [
            ("box hub", hub("flow", "box"), true),
            ("circle hub one way", one_way("data_flow", "circle"), true),
            (
                "table hub one way",
                one_way("entity_relationship", "table"),
                true,
            ),
            ("one pair", pair, true),
            ("circle hub", hub("data_flow", "circle"), false),
            ("table hub", hub("entity_relationship", "table"), false),
        ] {
            let started = Instant::now();
            let g = layout(&s, &Fonts);
            let took = started.elapsed();
            assert!(took.as_millis() <= budget, "{name}: {took:?}");
            if valid {
                check(&s, &g);
            }
        }
    }

    #[test]
    fn a_chain_of_forty_nodes_and_its_returns_stay_valid() {
        let nodes: Vec<_> = (0..40)
            .map(|i| node(&format!("n{i}"), &format!("Step {i}"), "box"))
            .collect();
        let mut edges: Vec<_> = (0..39)
            .map(|i| edge(&format!("n{i}"), &format!("n{}", i + 1), None))
            .collect();
        edges.push(edge("n39", "n0", Some("start over")));
        edges.push(edge("n0", "n39", None));
        run(&spec("flow", nodes, edges));
    }

    #[test]
    fn dense_random_graphs_never_put_two_routes_on_one_line() {
        for seed in 0..80u64 {
            for kind in ["flow", "graph", "states"] {
                let s = random_graph(kind, 12, 20, seed);
                check(&s, &layout(&s, &Fonts));
            }
        }
    }

    #[test]
    fn many_small_random_graphs_keep_every_invariant() {
        for seed in 0..80u64 {
            let n = 2 + (seed as usize % 13);
            let m = n + (seed as usize % (n + 3));
            for kind in ["flow", "graph"] {
                let s = random_graph(kind, n, m, seed);
                let g = layout(&s, &Fonts);
                check(&s, &g);
            }
        }
    }

    #[test]
    fn labels_stay_off_the_routes_of_other_edges() {
        let down = spec(
            "graph",
            vec![
                node("cli", "cli", "box"),
                node("auth", "auth", "box"),
                node("client", "client", "box"),
                node("config", "config", "box"),
            ],
            vec![
                edge("cli", "auth", None),
                edge("auth", "client", Some("new dependency")),
                edge("auth", "config", Some("reads refresh margin")),
            ],
        );
        let g = run(&down);
        assert!(
            g.edges.iter().filter(|e| e.label.is_some()).count() >= 1,
            "at least one of the two labels survives"
        );
        let across = spec(
            "flow",
            vec![
                node("a", "A", "box"),
                node("b", "B", "box"),
                node("c", "C", "box"),
                node("d", "D", "box"),
            ],
            vec![
                edge("a", "b", Some("first")),
                edge("a", "c", Some("second")),
                edge("a", "d", Some("third")),
                edge("b", "d", Some("fourth")),
                edge("d", "a", Some("back")),
            ],
        );
        let g = run(&across);
        assert!(g.edges.iter().any(|e| e.index == 0 && e.label.is_some()));
    }

    /// The edges of `s` that have a label in the spec but none in the geometry.
    fn dropped(s: &DiagramSpec, g: &Geometry) -> Vec<usize> {
        (0..s.edges.len())
            .filter(|&i| {
                s.edges[i].label.is_some()
                    && g.edges
                        .iter()
                        .find(|e| e.index == i)
                        .is_some_and(|e| e.label.is_none())
            })
            .collect()
    }

    #[test]
    fn the_mockup_fixtures_keep_every_edge_label() {
        for (name, s) in [
            ("flow", flow_mockup()),
            ("data flow", data_flow_mockup()),
            ("states", states_mockup()),
            ("graph", deps_mockup()),
        ] {
            let g = run(&s);
            assert_eq!(dropped(&s, &g), Vec::<usize>::new(), "{name}");
            assert_eq!(g.dropped_texts, 0, "{name}");
        }
    }

    #[test]
    fn sparse_tree_like_graphs_use_the_spine_and_keep_every_invariant() {
        let mut stacked = 0;
        for seed in 0..60u64 {
            for kind in ["flow", "data_flow", "states"] {
                let n = 4 + (seed as usize % 4);
                let s = sparse_graph(kind, n, seed);
                let g = layout(&s, &Fonts);
                check(&s, &g);
                let hangs = g
                    .boxes
                    .iter()
                    .any(|a| g.boxes.iter().any(|b| hangs_under(&b.rect, &a.rect)));
                stacked += usize::from(hangs);
            }
        }
        assert!(
            stacked >= 60,
            "only {stacked} of 180 diagrams hang a box 72 px under another"
        );
    }

    fn ranked(nodes: &[&str], edges: &[(&str, &str)]) -> DiagramSpec {
        spec(
            "graph",
            nodes.iter().map(|n| node(n, n, "box")).collect(),
            edges.iter().map(|(a, b)| edge(a, b, None)).collect(),
        )
    }

    #[test]
    fn ranked_layout_puts_each_node_on_its_rank_top_down() {
        let s = ranked(&["a", "b", "c", "d"], &[("a", "b"), ("a", "c"), ("b", "d")]);
        let g = layout_ranked(&s, &[0, 1, 1, 2], &Fonts);
        check(&s, &g);
        let (a, b, c, d) = (
            rect_of(&g, "a"),
            rect_of(&g, "b"),
            rect_of(&g, "c"),
            rect_of(&g, "d"),
        );
        assert!(
            (b.centre().y - c.centre().y).abs() < 1e-3,
            "one rank, one row"
        );
        assert!(b.y >= a.bottom() + 72.0 - 0.01 && d.y >= b.bottom() + 72.0 - 0.01);
        assert_eq!(g.edges.len(), 3);
        assert!(
            (g.edges[0].points[0].y - a.bottom()).abs() < 1e-3,
            "an edge leaves the bottom side"
        );
        for (i, b) in g.boxes.iter().enumerate() {
            assert_eq!(b.index, i, "boxes keep the spec's order");
        }
    }

    #[test]
    fn explicit_ranks_win_over_the_direction_of_the_edges() {
        let s = ranked(&["a", "b"], &[("b", "a")]);
        let g = layout_ranked(&s, &[0, 1], &Fonts);
        check(&s, &g);
        assert!(
            rect_of(&g, "a").bottom() < rect_of(&g, "b").y,
            "a is above b although b calls a"
        );
    }

    #[test]
    fn empty_ranks_leave_no_blank_row_and_short_ranks_do_not_panic() {
        let s = ranked(&["a", "b"], &[("a", "b")]);
        assert_eq!(
            layout_ranked(&s, &[0, 2], &Fonts),
            layout_ranked(&s, &[0, 1], &Fonts)
        );
        let three = ranked(&["a", "b", "c"], &[("a", "b")]);
        check(&three, &layout_ranked(&three, &[0, 1], &Fonts));
        assert_eq!(
            layout_ranked(&spec("graph", vec![], vec![]), &[], &Fonts).size,
            (48.0, 48.0)
        );
    }

    #[test]
    fn sixty_ranked_nodes_and_eighty_edges_are_valid_deterministic_and_inside_the_budget() {
        let names: Vec<String> = (0..60).map(|i| format!("n{i}")).collect();
        let ranks: Vec<usize> = (0..60).map(|i| i / 10).collect();
        let edges: Vec<(String, String)> = (0..80)
            .map(|k| {
                let a = (k * 7) % 50;
                let b = (a / 10 + 1) * 10 + (k * 3) % 10;
                (format!("n{a}"), format!("n{b}"))
            })
            .collect();
        let s = spec(
            "graph",
            names
                .iter()
                .map(|n| node(n, &format!("Function {n}"), "box"))
                .collect(),
            edges.iter().map(|(a, b)| edge(a, b, None)).collect(),
        );
        let started = Instant::now();
        let g = layout_ranked(&s, &ranks, &Fonts);
        let took = started.elapsed().as_millis();
        assert!(
            took <= if cfg!(debug_assertions) { 200 } else { 20 },
            "{took} ms"
        );
        check(&s, &g);
        assert_eq!(layout_ranked(&s, &ranks, &Fonts), g);
    }
}
