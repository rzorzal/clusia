//! Boxes with rows: the entity tables of an entity–relationship diagram and the classes of a
//! class diagram. A header of 34 px holds the name, and 16 px more the sub text under it; every
//! row is 23 px. A hairline separates the header from the rows, and every row of an entity from
//! the one before; a class has one only where its spec divides the rows.

use clusia_core::diagram::{DiagramKind, Node};

use crate::geometry::{Align, Point, Rect, RowGeo, TextLines};
use crate::measure::{Measure, TextStyle};
use crate::text;

pub(crate) const WIDTH: f32 = 230.0;
const HEADER: f32 = 34.0;
/// The header of a box with a sub text: the name, the sub text under it and the same padding.
const HEADER_SUB: f32 = 50.0;
/// Space between the name and the sub text under it.
const SUB_GAP: f32 = 2.0;
const ROW: f32 = 23.0;
const PAD: f32 = 12.0;
/// Room kept at the right of a row for its `PK` or `FK` tag.
const TAG: f32 = 24.0;
const MIN_HEIGHT: f32 = 40.0;

/// The size and the fitted text of one table or class.
pub(crate) struct Table {
    pub w: f32,
    pub h: f32,
    title: Vec<String>,
    title_style: TextStyle,
    sub: Vec<String>,
    sub_style: TextStyle,
    row_style: TextStyle,
    rows: Vec<Vec<String>>,
    /// A hairline above every row, not only where the spec divides them.
    ruled: bool,
}

pub(crate) fn table(kind: DiagramKind, node: &Node, measure: &dyn Measure) -> Table {
    let classes = kind == DiagramKind::Classes;
    let title_style = if classes {
        TextStyle::TitleMono
    } else {
        TextStyle::Title
    };
    let sub_style = if node.change == clusia_core::diagram::Change::Unchanged {
        TextStyle::Sub
    } else {
        TextStyle::SubStrong
    };
    let row_style = TextStyle::RowMono;
    let inner = WIDTH - 2.0 * PAD;
    let sub = text::wrap(
        measure,
        node.sub.as_deref().unwrap_or(""),
        sub_style,
        inner,
        1,
    );
    let title = text::wrap(measure, &node.label, title_style, inner, 1);
    let header = if sub.is_empty() { HEADER } else { HEADER_SUB };
    let rows = node
        .rows
        .iter()
        .map(|r| {
            let room = if r.key.is_some() { inner - TAG } else { inner };
            text::wrap(measure, &r.text, row_style, room, 1)
        })
        .collect();
    Table {
        w: WIDTH,
        h: (header + ROW * node.rows.len() as f32).max(MIN_HEIGHT),
        title,
        title_style,
        sub,
        sub_style,
        row_style,
        rows,
        ruled: !classes,
    }
}

impl Table {
    /// The header text and the rows inside `rect`.
    pub(crate) fn geometry(
        &self,
        node: &Node,
        rect: Rect,
        measure: &dyn Measure,
    ) -> (TextLines, Option<TextLines>, Vec<RowGeo>) {
        let inner = rect.w - 2.0 * PAD;
        let title_h = measure.line_height(self.title_style);
        let sub_h = measure.line_height(self.sub_style);
        let header = if self.sub.is_empty() {
            HEADER
        } else {
            HEADER_SUB
        };
        // The name, and the sub text under it, are centred in the header as one block.
        let block_h = if self.sub.is_empty() {
            title_h
        } else {
            title_h + SUB_GAP + sub_h
        };
        let top = rect.y + (header - block_h) / 2.0;
        let title = text::block(
            measure,
            self.title.clone(),
            self.title_style,
            Point {
                x: rect.x + PAD,
                y: top,
            },
            Some(inner),
            Align::Left,
        );
        let sub = (!self.sub.is_empty()).then(|| {
            text::block(
                measure,
                self.sub.clone(),
                self.sub_style,
                Point {
                    x: rect.x + PAD,
                    y: top + title_h + SUB_GAP,
                },
                Some(inner),
                Align::Left,
            )
        });
        let row_h = measure.line_height(self.row_style);
        let rows = node
            .rows
            .iter()
            .zip(&self.rows)
            .enumerate()
            .map(|(i, (row, lines))| {
                let cell = Rect {
                    x: rect.x,
                    y: rect.y + header + ROW * i as f32,
                    w: rect.w,
                    h: ROW,
                };
                RowGeo {
                    text: text::block(
                        measure,
                        lines.clone(),
                        self.row_style,
                        Point {
                            x: cell.x + PAD,
                            y: cell.y + (ROW - row_h) / 2.0,
                        },
                        Some(inner),
                        Align::Left,
                    ),
                    key: row.key,
                    change: row.change,
                    rect: cell,
                    divider_above: i == 0 || self.ruled || node.divider_after == Some(i as u32),
                }
            })
            .collect();
        (title, sub, rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::measure::Fonts;
    use crate::testkit::{assert_no_text_clash, check, edge, spec};
    use crate::{MARGIN, layout};
    use clusia_core::diagram::{Change, RowKey};
    use serde_json::{Value, json};

    fn row(text: &str, key: Option<&str>) -> Value {
        json!({ "text": text, "key": key, "change": null })
    }

    fn table_node(id: &str, label: &str, shape: &str, rows: Vec<Value>) -> Value {
        let mut n = crate::testkit::node(id, label, shape);
        n["rows"] = Value::Array(rows);
        n
    }

    /// The Entity–relationship mockup: an account has tokens, a token records refresh attempts
    /// and is saved as a file.
    fn er() -> clusia_core::diagram::DiagramSpec {
        let mut token = table_node(
            "token",
            "Token",
            "table",
            vec![
                row("id", Some("pk")),
                row("access_token", None),
                row("account_id", Some("fk")),
            ],
        );
        token["sub"] = json!("changed");
        token["change"] = json!("changed");
        let mut has = edge("account", "token", Some("has"));
        has["ends"] = json!(["1", "0..N"]);
        let mut records = edge("token", "attempt", Some("records"));
        records["ends"] = json!(["1", "0..N"]);
        let mut saved = edge("token", "file", Some("saved as"));
        saved["ends"] = json!(["1", "1"]);
        spec(
            "entity_relationship",
            vec![
                table_node(
                    "account",
                    "Account",
                    "table",
                    vec![row("id", Some("pk")), row("login", None)],
                ),
                token,
                table_node(
                    "attempt",
                    "RefreshAttempt",
                    "table",
                    vec![row("id", Some("pk")), row("token_id", Some("fk"))],
                ),
                table_node("file", "TokenFile", "table", vec![row("path", None)]),
            ],
            vec![has, records, saved],
        )
    }

    /// The Classes mockup: a token store between the token it returns and its client.
    fn classes() -> clusia_core::diagram::DiagramSpec {
        let class = |id: &str, label: &str, rows: &[&str]| {
            table_node(
                id,
                label,
                "class",
                rows.iter().map(|r| row(r, None)).collect(),
            )
        };
        spec(
            "classes",
            vec![
                class("token", "Token", &["access: String", "is_fresh(margin)"]),
                class(
                    "store",
                    "TokenStore",
                    &["path: PathBuf", "token() -> Token"],
                ),
                class("client", "HttpClient", &["base: Url", "send(&Request)"]),
            ],
            vec![
                edge("store", "token", Some("returns")),
                edge("store", "client", Some("owns 1")),
            ],
        )
    }

    #[test]
    fn a_table_is_230_wide_with_a_34_px_header_and_23_px_rows() {
        let s = er();
        let g = layout(&s, &Fonts);
        check(&s, &g);
        let account = g.boxes.iter().find(|b| b.node == "account").expect("box");
        assert_eq!(
            account.rect.h,
            34.0 + 2.0 * 23.0,
            "no sub text: a 34 px header"
        );
        let token = g.boxes.iter().find(|b| b.node == "token").expect("token");
        assert_eq!(token.rect.w, 230.0);
        assert_eq!(
            token.rect.h,
            50.0 + 3.0 * 23.0,
            "the sub text takes a line under the name"
        );
        assert_eq!(token.rows.len(), 3);
        assert_eq!(token.rows[1].rect.y, token.rect.y + 50.0 + 23.0);
        assert_eq!(token.rows[0].key, Some(RowKey::Pk));
        assert_eq!(token.rows[2].key, Some(RowKey::Fk));
        assert!(
            token.rows.iter().all(|r| r.divider_above),
            "a hairline above every row of an entity"
        );
        assert_eq!(token.title.lines, vec!["Token".to_string()]);
        let sub = token.sub.as_ref().expect("sub text");
        assert_eq!(sub.at.x, token.title.at.x, "under the name");
        assert!(sub.at.y >= token.title.rect().bottom());
        assert!(sub.rect().bottom() <= token.rows[0].rect.y);
        assert_eq!(token.change, Change::Changed);
        assert_eq!(token.title.style, TextStyle::Title);
        assert_eq!(token.rows[0].text.style, TextStyle::RowMono);
    }

    #[test]
    fn cardinalities_sit_at_the_ends_and_there_are_no_arrowheads() {
        let s = er();
        let g = layout(&s, &Fonts);
        let has = &g.edges[0];
        assert_eq!(has.arrow, crate::Arrow::None);
        let near_from = has.ends[0].as_ref().expect("from end");
        let near_to = has.ends[1].as_ref().expect("to end");
        assert_eq!(near_from.lines, vec!["1".to_string()]);
        assert_eq!(near_to.lines, vec!["0..N".to_string()]);
        assert_eq!(near_from.style, TextStyle::Card);
        let account = g.boxes.iter().find(|b| b.node == "account").expect("box");
        let token = g.boxes.iter().find(|b| b.node == "token").expect("box");
        assert!(near_from.at.x >= account.rect.right());
        assert!(near_to.rect().right() <= token.rect.x);
    }

    #[test]
    fn a_class_has_a_divider_after_the_rows_it_names() {
        let mut store = table_node(
            "store",
            "TokenStore",
            "class",
            vec![
                row("path: PathBuf", None),
                row("client: HttpClient", None),
                row("token() -> Token", None),
                row("save(&Token)", None),
            ],
        );
        store["divider_after"] = json!(2);
        let s = spec(
            "classes",
            vec![
                table_node("token", "Token", "class", vec![row("access: String", None)]),
                store,
            ],
            vec![edge("store", "token", Some("returns"))],
        );
        let g = layout(&s, &Fonts);
        check(&s, &g);
        let b = g.boxes.iter().find(|b| b.node == "store").expect("store");
        let dividers: Vec<bool> = b.rows.iter().map(|r| r.divider_above).collect();
        assert_eq!(dividers, vec![true, false, true, false]);
        assert_eq!(b.title.style, TextStyle::TitleMono);
        assert_eq!(b.rows[0].text.style, TextStyle::RowMono);
        assert_eq!(g.edges[0].arrow, crate::Arrow::End);
    }

    #[test]
    fn long_rows_and_names_are_cut_to_one_line() {
        let long = "a_very_long_field_name_that_goes_on: HashMap<String, Vec<Option<Token>>>";
        let s = spec(
            "classes",
            vec![table_node(
                "t",
                &"Name".repeat(40),
                "class",
                vec![row(long, None), row(long, Some("pk"))],
            )],
            vec![],
        );
        let g = layout(&s, &Fonts);
        check(&s, &g);
        let b = &g.boxes[0];
        assert_eq!(b.title.lines.len(), 1);
        assert!(b.title.lines[0].ends_with('…'));
        for r in &b.rows {
            assert_eq!(r.text.lines.len(), 1);
            assert!(r.text.lines[0].ends_with('…'));
            assert!(r.text.rect().right() <= r.rect.right());
        }
    }

    #[test]
    fn a_table_without_rows_keeps_a_minimum_height_and_starts_at_the_margin() {
        let s = spec(
            "entity_relationship",
            vec![table_node("t", "Empty", "table", vec![])],
            vec![],
        );
        let g = layout(&s, &Fonts);
        assert_eq!(g.boxes[0].rect.h, 40.0);
        assert_eq!((g.boxes[0].rect.x, g.boxes[0].rect.y), (MARGIN, MARGIN));
        assert!(g.boxes[0].rows.is_empty());
    }

    #[test]
    fn many_tables_and_relationships_keep_every_invariant() {
        for seed in 0..30u64 {
            let n = 3 + seed as usize % 9;
            let nodes: Vec<Value> = (0..n)
                .map(|i| {
                    let rows = (0..(1 + i % 5))
                        .map(|r| row(&format!("field_{r}"), None))
                        .collect();
                    table_node(&format!("n{i}"), &format!("Entity {i}"), "table", rows)
                })
                .collect();
            let edges: Vec<Value> = (0..n + 2)
                .map(|k| {
                    let (a, b) = ((k * 7 + seed as usize) % n, (k * 3 + 1) % n);
                    let mut e = edge(&format!("n{a}"), &format!("n{b}"), Some("relates"));
                    e["ends"] = json!(["1", "0..N"]);
                    e
                })
                .collect();
            let s = spec("entity_relationship", nodes, edges);
            check(&s, &layout(&s, &Fonts));
        }
    }

    #[test]
    fn the_er_mockup_hangs_the_file_under_the_token() {
        let s = er();
        let g = layout(&s, &Fonts);
        check(&s, &g);
        assert_no_text_clash(&g);
        let rect = |id: &str| g.boxes.iter().find(|b| b.node == id).expect("box").rect;
        let (account, token, attempt, file) = (
            rect("account"),
            rect("token"),
            rect("attempt"),
            rect("file"),
        );
        assert!(account.right() < token.x && token.right() < attempt.x);
        assert_eq!(
            (account.y, token.y),
            (token.y, attempt.y),
            "one row, tops level"
        );
        assert_eq!(file.x, token.x);
        assert!((file.y - token.bottom() - 72.0).abs() < 0.01);
        for e in &g.edges {
            assert!(e.label.is_some(), "edge {} keeps its label", e.index);
            assert!(
                e.ends.iter().all(Option::is_some),
                "edge {} keeps both cardinalities",
                e.index
            );
        }
        let saved = &g.edges[2];
        assert_eq!(saved.points.len(), 2, "straight down the column");
        assert_eq!(saved.points[0].x, saved.points[1].x);
        assert_eq!(saved.points[0].y, token.bottom());
        assert_eq!(saved.points[1].y, file.y);
        let (label, from, to) = (
            saved.label.as_ref().expect("label"),
            saved.ends[0].as_ref().expect("from"),
            saved.ends[1].as_ref().expect("to"),
        );
        assert!(from.at.y >= token.bottom() && to.rect().bottom() <= file.y);
        assert!(label.at.y >= from.rect().bottom() && label.rect().bottom() <= to.at.y);
    }

    #[test]
    fn a_label_between_two_tables_sits_under_the_line_and_clear_of_both_cardinalities() {
        let s = er();
        let g = layout(&s, &Fonts);
        for e in g.edges.iter().take(2) {
            let label = e.label.as_ref().expect("label");
            assert!(label.at.y > e.points[0].y, "under the line");
            let last = e.points[e.points.len() - 1].y;
            for (end, line) in e.ends.iter().flatten().zip([e.points[0].y, last]) {
                assert!(end.rect().bottom() <= line, "above the line");
                assert!(!label.rect().intersects(&end.rect()));
            }
        }
    }

    #[test]
    fn the_classes_mockup_is_one_row_of_three() {
        let s = classes();
        let g = layout(&s, &Fonts);
        check(&s, &g);
        assert_no_text_clash(&g);
        let labels: Vec<&str> = g
            .edges
            .iter()
            .map(|e| e.label.as_ref().expect("label").lines[0].as_str())
            .collect();
        assert_eq!(labels, vec!["returns", "owns 1"]);
        let top = g.boxes[0].rect.y;
        assert!(g.boxes.iter().all(|b| b.rect.y == top));
        assert!(g.boxes.windows(2).all(|w| w[0].rect.right() < w[1].rect.x));
    }

    #[test]
    fn tables_at_the_caps_keep_the_invariants_inside_the_budget() {
        for (kind, shape) in [("entity_relationship", "table"), ("classes", "class")] {
            let nodes: Vec<Value> = (0..40)
                .map(|i| {
                    let rows = (0..16).map(|r| row(&format!("field_{r}"), None)).collect();
                    table_node(&format!("n{i}"), &format!("Entity {i}"), shape, rows)
                })
                .collect();
            let edges: Vec<Value> = (0..80)
                .map(|k| {
                    let (a, b) = ((k * 7 + 3) % 40, (k * 11 + 5) % 40);
                    let mut e = edge(&format!("n{a}"), &format!("n{b}"), Some("relates"));
                    e["ends"] = json!(["1", "0..N"]);
                    e
                })
                .collect();
            let s = spec(kind, nodes, edges);
            let (g, took) = crate::testkit::cpu_timed(|| layout(&s, &Fonts));
            assert!(
                took.as_millis() <= crate::testkit::BUDGET_MS,
                "{kind}: {took:?}"
            );
            check(&s, &g);
            assert_eq!(layout(&s, &Fonts), g);
        }
    }
}
