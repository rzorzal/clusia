//! Sequence diagrams: participants in a row, a lifeline under each, and the messages top to
//! bottom in spec order.

use std::collections::HashMap;

use clusia_core::diagram::DiagramSpec;

use crate::geometry::{
    Activation, Align, BoxGeo, Geometry, Lifeline, MessageGeo, Point, Rect, TextLines,
};
use crate::layout::{box_text, card};
use crate::measure::{Measure, TextStyle};
use crate::text;

/// The least distance between two lifelines.
const MIN_PITCH: f32 = 160.0;
/// Clear space between two participant boxes.
const BOX_GAP: f32 = 40.0;
/// Space kept each side of a message label along its arrow.
const LABEL_PAD: f32 = 16.0;
const LABEL_WIDTH: f32 = 300.0;
const LABEL_LINES: usize = 2;
/// Space under the participants before the first label, and above each later arrow.
const ARROW_LEAD: f32 = 21.0;
const FIRST_LEAD: f32 = 32.0;
/// Space between the last arrow and the end of the lifelines.
const TAIL: f32 = 40.0;
/// How far a message to the sender itself reaches to the right.
const SELF_REACH: f32 = 28.0;
const ACTIVATION_WIDTH: f32 = 14.0;
const ACTIVATION_PAD: f32 = 10.0;

/// A message that can be drawn: both ends are participants.
struct Item {
    index: usize,
    from: usize,
    to: usize,
    label: Vec<String>,
    label_w: f32,
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
    let items: Vec<Item> = spec
        .messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let from = *index.get(m.from.as_str())?;
            let to = *index.get(m.to.as_str())?;
            let label = text::wrap(
                measure,
                &m.label,
                TextStyle::EdgeLabelMono,
                LABEL_WIDTH,
                LABEL_LINES,
            );
            let label_w = text::widest(measure, &label, TextStyle::EdgeLabelMono);
            Some(Item {
                index: i,
                from,
                to,
                label,
                label_w,
            })
        })
        .collect();

    // pitch[i] is the distance from lifeline i to lifeline i + 1.
    let mut pitch: Vec<f32> = (0..n.saturating_sub(1))
        .map(|i| MIN_PITCH.max((cards[i].w + cards[i + 1].w) / 2.0 + BOX_GAP))
        .collect();
    // Short arrows first: widening a span never undoes a narrower one that was already met.
    let mut by_span: Vec<&Item> = items.iter().collect();
    by_span.sort_by_key(|m| (m.from.abs_diff(m.to), m.index));
    for m in by_span {
        let (lo, hi) = (m.from.min(m.to), m.from.max(m.to));
        let (need, gaps) = if lo == hi {
            (m.label_w + 8.0 + LABEL_PAD, lo..(lo + 1).min(pitch.len()))
        } else {
            (m.label_w + LABEL_PAD, lo..hi)
        };
        if gaps.is_empty() {
            continue;
        }
        let have: f32 = pitch[gaps.clone()].iter().sum();
        if have < need {
            let extra = (need - have) / gaps.len() as f32;
            for p in &mut pitch[gaps] {
                *p += extra;
            }
        }
    }
    let mut centre = vec![cards[0].w / 2.0; n];
    for i in 1..n {
        centre[i] = centre[i - 1] + pitch[i - 1];
    }

    let rects: Vec<Rect> = cards
        .iter()
        .zip(&centre)
        .map(|(c, &x)| Rect {
            x: x - c.w / 2.0,
            y: 0.0,
            w: c.w,
            h: c.h,
        })
        .collect();
    let tallest = rects.iter().map(Rect::bottom).fold(0.0, f32::max);

    // Arrow heights: each row is its label's lines plus a fixed lead.
    let line = measure.line_height(TextStyle::EdgeLabelMono);
    let mut ys: Vec<f32> = Vec::with_capacity(items.len());
    let mut y = tallest;
    for (k, m) in items.iter().enumerate() {
        let lead = if k == 0 { FIRST_LEAD } else { ARROW_LEAD };
        y += lead + m.label.len().max(1) as f32 * line;
        ys.push(y);
    }
    let last = ys.last().copied().unwrap_or(tallest + TAIL);
    let bottom = last + TAIL;

    for (i, (node, c)) in spec.nodes.iter().zip(&cards).enumerate() {
        let rect = rects[i];
        let (title, sub) = box_text(c, rect, measure);
        geometry.boxes.push(BoxGeo {
            node: node.id.clone(),
            index: i,
            rect,
            shape: node.shape,
            title,
            sub,
            rows: Vec::new(),
            change: node.change,
            history: node.history,
            anchored: node.anchor.is_some(),
        });
        geometry.lifelines.push(Lifeline {
            node: node.id.clone(),
            x: centre[i],
            top: rect.bottom(),
            bottom,
        });
    }

    for (m, &y) in items.iter().zip(&ys) {
        let message = &spec.messages[m.index];
        let from_x = centre[m.from];
        let (to_x, label_x) = if m.from == m.to {
            (from_x + SELF_REACH, from_x + 8.0)
        } else {
            let to_x = centre[m.to];
            (to_x, (from_x + to_x) / 2.0 - m.label_w / 2.0)
        };
        let height = m.label.len() as f32 * line;
        geometry.messages.push(MessageGeo {
            index: m.index,
            y,
            from_x,
            to_x,
            label: TextLines {
                lines: m.label.clone(),
                style: TextStyle::EdgeLabelMono,
                at: Point {
                    x: label_x,
                    y: y - 4.0 - height,
                },
                width: m.label_w,
                height,
                align: Align::Left,
            },
            reply: message.reply,
            change: message.change,
            history: message.history,
        });
    }

    // A participant that is called and then calls on is active from the first call it gets to
    // the last message it takes part in.
    for (i, node) in spec.nodes.iter().enumerate() {
        let called = items
            .iter()
            .zip(&ys)
            .find(|(m, _)| m.to == i && m.from != i && !spec.messages[m.index].reply)
            .map(|(_, &y)| y);
        let Some(first) = called else { continue };
        let calls_on = items
            .iter()
            .zip(&ys)
            .any(|(m, &y)| m.from == i && m.to != i && !spec.messages[m.index].reply && y > first);
        if !calls_on {
            continue;
        }
        let last = items
            .iter()
            .zip(&ys)
            .filter(|(m, _)| m.from == i || m.to == i)
            .map(|(_, &y)| y)
            .fold(first, f32::max);
        geometry.activations.push(Activation {
            node: node.id.clone(),
            rect: Rect {
                x: centre[i] - ACTIVATION_WIDTH / 2.0,
                y: first - ACTIVATION_PAD,
                w: ACTIVATION_WIDTH,
                h: last - first + 2.0 * ACTIVATION_PAD,
            },
        });
    }
    geometry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::measure::Fonts;
    use crate::testkit::{assert_no_text_clash, check, node, spec};
    use crate::{MARGIN, layout};
    use clusia_core::diagram::Change;
    use serde_json::{Value, json};

    fn message(from: &str, to: &str, label: &str, reply: bool) -> Value {
        json!({ "from": from, "to": to, "label": label, "reply": reply, "change": "unchanged", "history": null })
    }

    fn with_messages(nodes: Vec<Value>, messages: Vec<Value>) -> DiagramSpec {
        let mut v = serde_json::to_value(spec("sequence", nodes, vec![])).expect("json");
        v["messages"] = Value::Array(messages);
        serde_json::from_value(v).expect("spec")
    }

    /// The Sequence mockup: two requests, the token store, its lock and the endpoint.
    fn mockup() -> DiagramSpec {
        with_messages(
            vec![
                node("a", "Request A", "actor"),
                node("b", "Request B", "actor"),
                node("store", "TokenStore", "box"),
                node("lock", "refresh_lock", "box"),
                node("oauth", "OAuth endpoint", "box"),
            ],
            vec![
                message("a", "store", "token()", false),
                message("store", "lock", "lock()", false),
                message("b", "store", "token()", false),
                message("store", "lock", "lock() · waits for A", false),
                message("store", "oauth", "exchange(refresh)", false),
                message("oauth", "store", "new token", true),
                message("store", "a", "Ok(token)", true),
                message("store", "b", "Ok(same token) · no second exchange", true),
            ],
        )
    }

    fn assert_sequence(s: &DiagramSpec, g: &Geometry) {
        check(s, g);
        assert_eq!(g.lifelines.len(), g.boxes.len());
        for (b, l) in g.boxes.iter().zip(&g.lifelines) {
            assert!(
                (l.x - b.rect.centre().x).abs() < 1e-3,
                "a lifeline runs under its box"
            );
            assert!((l.top - b.rect.bottom()).abs() < 1e-3);
        }
        let mut last_y = f32::NEG_INFINITY;
        for m in &g.messages {
            assert!(m.y > last_y, "messages go down in order");
            last_y = m.y;
            let lines = g.lifelines.iter().map(|l| (l.top, l.bottom));
            for (top, bottom) in lines {
                assert!(m.y > top && m.y < bottom);
            }
            for b in &g.boxes {
                assert!(
                    !m.label.rect().intersects(&b.rect),
                    "a label never overlaps a participant"
                );
            }
            if m.from_x != m.to_x {
                let (lo, hi) = (m.from_x.min(m.to_x), m.from_x.max(m.to_x));
                let l = m.label.rect();
                assert!(
                    l.x >= lo && l.right() <= hi,
                    "a label stays between its lifelines"
                );
            }
        }
        for a in &g.activations {
            let line = g
                .lifelines
                .iter()
                .find(|l| l.node == a.node)
                .expect("lifeline");
            assert!(a.rect.y >= line.top && a.rect.bottom() <= line.bottom);
            assert!((a.rect.centre().x - line.x).abs() < 1e-3);
        }
    }

    #[test]
    fn the_mockup_has_eight_messages_five_lifelines_and_one_activation() {
        let s = mockup();
        let g = layout(&s, &Fonts);
        assert_sequence(&s, &g);
        assert_eq!(g.messages.len(), 8);
        assert_eq!(g.lifelines.len(), 5);
        assert!(g.edges.is_empty());
        assert_eq!(g.activations.len(), 1);
        assert_eq!(g.activations[0].node, "store");
        let replies: Vec<bool> = g.messages.iter().map(|m| m.reply).collect();
        assert_eq!(
            replies,
            vec![false, false, false, false, false, true, true, true]
        );
        assert_eq!((g.boxes[0].rect.x, g.boxes[0].rect.y), (MARGIN, MARGIN));
        for m in &g.messages {
            assert!(
                !m.label.lines.is_empty(),
                "message {} keeps its label",
                m.index
            );
        }
        let first_gap = g.messages[1].y - g.messages[0].y;
        assert_eq!(first_gap, 35.0, "one-line labels are 35 px apart");
        assert_eq!(g.boxes[0].title.align, Align::Center);
        assert_no_text_clash(&g);
    }

    #[test]
    fn a_reply_label_and_a_two_word_participant_keep_to_one_line() {
        let s = with_messages(
            vec![
                node("b", "Request B", "box"),
                node("store", "TokenStore", "box"),
                node("oauth", "OAuth endpoint", "box"),
            ],
            vec![message(
                "store",
                "b",
                "Ok(same token) · no second exchange",
                true,
            )],
        );
        let g = layout(&s, &Fonts);
        assert_sequence(&s, &g);
        assert_eq!(g.messages[0].label.lines.len(), 1);
        let oauth = g.boxes.iter().find(|b| b.node == "oauth").expect("box");
        assert_eq!(oauth.title.lines, vec!["OAuth endpoint".to_string()]);
    }

    #[test]
    fn a_long_label_widens_the_columns_it_spans() {
        let long = "refresh the token and wait for the other caller to finish";
        let s = with_messages(
            vec![node("a", "A", "box"), node("b", "B", "box")],
            vec![message("a", "b", long, false)],
        );
        let g = layout(&s, &Fonts);
        assert_sequence(&s, &g);
        let m = &g.messages[0];
        assert!(m.to_x - m.from_x >= m.label.width + 16.0 - 0.01);
        assert!(m.label.lines.len() <= 2);
    }

    #[test]
    fn a_message_to_the_sender_reaches_right_and_keeps_its_label_clear() {
        let s = with_messages(
            vec![node("a", "A", "box"), node("b", "B", "box")],
            vec![
                message("a", "a", "validate the token before use", false),
                message("a", "b", "go", false),
            ],
        );
        let g = layout(&s, &Fonts);
        check(&s, &g);
        let m = &g.messages[0];
        assert_eq!(m.to_x, m.from_x + 28.0);
        assert!(m.label.at.x >= m.from_x + 8.0 - 0.01);
        assert!(
            m.label.rect().right() <= g.lifelines[1].x,
            "the label stops before the next lifeline"
        );
    }

    #[test]
    fn unknown_participants_drop_the_message_and_keep_the_rest() {
        let s = with_messages(
            vec![node("a", "A", "box"), node("b", "B", "box")],
            vec![
                message("a", "ghost", "lost", false),
                message("a", "b", "kept", false),
            ],
        );
        let g = layout(&s, &Fonts);
        assert_eq!(g.messages.len(), 1);
        assert_eq!(
            g.messages[0].index, 1,
            "the message keeps its position in the spec"
        );
    }

    #[test]
    fn no_messages_still_draws_participants_with_lifelines() {
        let s = with_messages(vec![node("a", "A", "box"), node("b", "B", "box")], vec![]);
        let g = layout(&s, &Fonts);
        check(&s, &g);
        assert!(g.messages.is_empty());
        assert!(g.lifelines.iter().all(|l| l.bottom > l.top));
        let empty = with_messages(vec![], vec![]);
        assert_eq!(layout(&empty, &Fonts).size, (48.0, 48.0));
    }

    #[test]
    fn change_and_history_marks_travel_with_the_message() {
        let mut v = serde_json::to_value(mockup()).expect("json");
        v["messages"][0]["change"] = json!("new");
        v["messages"][0]["history"] = json!("added");
        let s: DiagramSpec = serde_json::from_value(v).expect("spec");
        let g = layout(&s, &Fonts);
        assert_eq!(g.messages[0].change, Change::New);
        assert_eq!(
            g.messages[0].history,
            Some(clusia_core::diagram::History::Added)
        );
    }

    #[test]
    fn eight_participants_and_sixty_messages_keep_the_invariants_inside_the_budget() {
        let nodes: Vec<Value> = (0..8)
            .map(|i| node(&format!("p{i}"), &format!("Participant {i}"), "box"))
            .collect();
        let messages: Vec<Value> = (0..60)
            .map(|k| {
                let (a, b) = (k % 8, (k * 3 + 1) % 8);
                message(
                    &format!("p{a}"),
                    &format!("p{b}"),
                    &format!("call number {k} with a longish label"),
                    k % 4 == 3,
                )
            })
            .collect();
        let s = with_messages(nodes, messages);
        let started = std::time::Instant::now();
        let g = layout(&s, &Fonts);
        let budget = if cfg!(debug_assertions) { 200 } else { 20 };
        assert!(started.elapsed().as_millis() <= budget);
        assert_sequence(&s, &g);
        assert_eq!(layout(&s, &Fonts), g, "deterministic");
    }
}
