//! Helpers for the layout tests: specs from JSON and the invariants every kind must keep.

use clusia_core::diagram::DiagramSpec;
use serde_json::{Value, json};

use crate::{EdgeGeo, Geometry, MARGIN, Point, Rect, TextLines};

/// A node as JSON with every field spelled out.
pub(crate) fn node(id: &str, label: &str, shape: &str) -> Value {
    json!({
        "id": id, "label": label, "sub": null, "shape": shape, "rows": [],
        "divider_after": null, "anchor": null, "change": "unchanged",
        "note": null, "history": null
    })
}

pub(crate) fn node_sub(id: &str, label: &str, sub: &str, shape: &str, change: &str) -> Value {
    let mut v = node(id, label, shape);
    v["sub"] = json!(sub);
    v["change"] = json!(change);
    v
}

/// An edge as JSON with every field spelled out.
pub(crate) fn edge(from: &str, to: &str, label: Option<&str>) -> Value {
    json!({
        "from": from, "to": to, "label": label, "style": "solid",
        "ends": [null, null], "change": "unchanged", "history": null
    })
}

pub(crate) fn spec(kind: &str, nodes: Vec<Value>, edges: Vec<Value>) -> DiagramSpec {
    serde_json::from_value(json!({
        "key": "k", "kind": kind, "title": "A title", "caption": "",
        "nodes": nodes, "edges": edges, "groups": [], "messages": []
    }))
    .expect("a spec")
}

/// The five boxes of the Flow mockup.
pub(crate) fn flow_mockup() -> DiagramSpec {
    spec(
        "flow",
        vec![
            node_sub(
                "client",
                "HTTP client",
                "client/http.rs",
                "box",
                "unchanged",
            ),
            node_sub(
                "refresh",
                "TokenStore::refresh",
                "+9 −2 · 1 draft comment",
                "box",
                "changed",
            ),
            node_sub(
                "oauth",
                "OAuth token endpoint",
                "github.com · external",
                "external",
                "unchanged",
            ),
            node_sub("lock", "refresh_lock", "Mutex<()> · new", "box", "new"),
            node_sub(
                "save",
                "TokenStore::save",
                "Risk: not atomic",
                "box",
                "risk",
            ),
        ],
        vec![
            edge("client", "refresh", Some("token()")),
            edge("refresh", "oauth", Some("exchange()")),
            edge("refresh", "lock", Some("new")),
            edge("oauth", "save", None),
        ],
    )
}

/// The six modules of the Graph mockup.
pub(crate) fn deps_mockup() -> DiagramSpec {
    spec(
        "graph",
        vec![
            node_sub("cli", "cli", "unchanged", "box", "unchanged"),
            node_sub(
                "handlers",
                "clusiad::handlers",
                "unchanged",
                "box",
                "unchanged",
            ),
            node_sub("auth", "auth", "+53 −13 · 3 files", "box", "changed"),
            node_sub("client", "client", "+28 −9 · retries", "box", "changed"),
            node_sub("config", "config", "+6 · token margin", "box", "changed"),
            node_sub(
                "tracing",
                "tracing",
                "external crate",
                "external",
                "unchanged",
            ),
        ],
        vec![
            edge("cli", "auth", None),
            edge("handlers", "auth", None),
            edge("handlers", "client", None),
            edge("auth", "client", Some("new dependency")),
            edge("auth", "config", Some("reads refresh margin")),
            edge("client", "tracing", Some("logs requests")),
        ],
    )
}

/// The five nodes of the Data flow mockup: two circles in a row between two external parties,
/// and the file they read and write.
pub(crate) fn data_flow_mockup() -> DiagramSpec {
    spec(
        "data_flow",
        vec![
            node_sub("client", "API client", "external", "external", "unchanged"),
            node_sub("refresh", "1  Refresh", "margin 60 s", "circle", "changed"),
            node_sub("exchange", "2  Exchange", "retries", "circle", "changed"),
            node_sub(
                "github",
                "GitHub OAuth",
                "external",
                "external",
                "unchanged",
            ),
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
            edge("refresh", "exchange", Some("refresh token")),
            edge("exchange", "github", Some("grant request")),
            edge("github", "exchange", Some("access + refresh token")),
            edge("exchange", "file", Some("write (temp file + rename)")),
            edge("file", "refresh", Some("read")),
        ],
    )
}

/// The start dot and four states of the States mockup.
pub(crate) fn states_mockup() -> DiagramSpec {
    spec(
        "states",
        vec![
            node("start", "", "start"),
            node("fresh", "Fresh", "pill"),
            node_sub("expiring", "Expiring", "within 60 s · new", "pill", "new"),
            node_sub(
                "refreshing",
                "Refreshing",
                "one caller, others wait",
                "pill",
                "changed",
            ),
            node_sub("failed", "Failed", "retries: max 3", "pill", "risk"),
        ],
        vec![
            edge("start", "fresh", None),
            edge("fresh", "expiring", Some("time passes")),
            edge("expiring", "refreshing", Some("first caller locks")),
            edge("refreshing", "fresh", Some("new token saved")),
            edge("refreshing", "failed", Some("exchange failed")),
            edge("failed", "expiring", Some("retry")),
        ],
    )
}

/// A graph of `n` nodes and `m` edges from a fixed pseudo-random sequence.
pub(crate) fn random_graph(kind: &str, n: usize, m: usize, seed: u64) -> DiagramSpec {
    let mut state = seed;
    let mut next = move |bound: usize| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((state >> 33) as usize) % bound
    };
    let nodes: Vec<Value> = (0..n)
        .map(|i| node(&format!("n{i}"), &format!("Module number {i}"), "box"))
        .collect();
    let edges: Vec<Value> = (0..m)
        .map(|_| {
            let (a, b) = (next(n), next(n));
            edge(
                &format!("n{a}"),
                &format!("n{b}"),
                Some("calls the other module"),
            )
        })
        .collect();
    spec(kind, nodes, edges)
}

/// A tree of `n` nodes hung from earlier ones, with a few extra edges, from a fixed
/// pseudo-random sequence.
pub(crate) fn sparse_graph(kind: &str, n: usize, seed: u64) -> DiagramSpec {
    let mut state = seed;
    let mut next = move |bound: usize| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((state >> 33) as usize) % bound
    };
    let nodes: Vec<Value> = (0..n)
        .map(|i| node(&format!("n{i}"), &format!("Module number {i}"), "box"))
        .collect();
    let mut edges: Vec<Value> = Vec::new();
    for i in 1..n {
        let parent = next(i);
        let (a, b) = if next(4) == 0 {
            (i, parent)
        } else {
            (parent, i)
        };
        edges.push(edge(
            &format!("n{a}"),
            &format!("n{b}"),
            Some("calls the other module"),
        ));
    }
    for _ in 0..next(3) {
        let (a, b) = (next(n), next(n));
        edges.push(edge(&format!("n{a}"), &format!("n{b}"), Some("extra")));
    }
    spec(kind, nodes, edges)
}

fn is_axis_aligned(a: Point, b: Point) -> bool {
    (a.x - b.x).abs() < 1e-3 || (a.y - b.y).abs() < 1e-3
}

/// Whether the axis-aligned segment `a`–`b` touches the closed rectangle `r`.
fn touches(a: Point, b: Point, r: Rect) -> bool {
    a.x.max(b.x) >= r.x
        && a.x.min(b.x) <= r.right()
        && a.y.max(b.y) >= r.y
        && a.y.min(b.y) <= r.bottom()
}

/// How far two collinear runs overlap, or 0 when they are not on one line.
fn overlap(a: (Point, Point), b: (Point, Point)) -> f32 {
    let (ax, bx) = ((a.0.x - a.1.x).abs() < 1e-3, (b.0.x - b.1.x).abs() < 1e-3);
    let (ay, by) = ((a.0.y - a.1.y).abs() < 1e-3, (b.0.y - b.1.y).abs() < 1e-3);
    let span = |p: f32, q: f32, r: f32, s: f32| p.max(q).min(r.max(s)) - p.min(q).max(r.min(s));
    if ax && bx && (a.0.x - b.0.x).abs() < 1.0 {
        span(a.0.y, a.1.y, b.0.y, b.1.y)
    } else if ay && by && (a.0.y - b.0.y).abs() < 1.0 {
        span(a.0.x, a.1.x, b.0.x, b.1.x)
    } else {
        0.0
    }
}

/// Whether two edges have runs on one line that overlap by more than 2 px.
fn share_a_run(a: &EdgeGeo, b: &EdgeGeo) -> bool {
    a.points.windows(2).any(|s| {
        b.points
            .windows(2)
            .any(|t| overlap((s[0], s[1]), (t[0], t[1])) > 2.0)
    })
}

/// Panics when two texts that belong to edges or messages overlap: labels and cardinalities of
/// every edge, and the labels of every message.
pub(crate) fn assert_no_text_clash(geometry: &Geometry) {
    let mut texts: Vec<(String, Rect)> = Vec::new();
    for e in &geometry.edges {
        for t in e.label.iter().chain(e.ends.iter().flatten()) {
            texts.push((format!("edge {}: {:?}", e.index, t.lines), t.rect()));
        }
    }
    for m in &geometry.messages {
        texts.push((
            format!("message {}: {:?}", m.index, m.label.lines),
            m.label.rect(),
        ));
    }
    for (i, (name, rect)) in texts.iter().enumerate() {
        for (other, r) in &texts[i + 1..] {
            assert!(!rect.intersects(r), "{name} overlaps {other}");
        }
    }
}

/// Panics when `geometry` breaks an invariant every kind keeps: finite numbers, boxes apart,
/// routes sideways or up and down only and clear of other boxes, labels off boxes and off the
/// other texts of their edge, no two edges along one line, and everything inside `size` and starting at the
/// margin.
pub(crate) fn check(spec: &DiagramSpec, geometry: &Geometry) {
    let finite = |v: f32| assert!(v.is_finite(), "not finite: {v}");
    for b in &geometry.boxes {
        [b.rect.x, b.rect.y, b.rect.w, b.rect.h]
            .into_iter()
            .for_each(finite);
    }
    for (i, a) in geometry.boxes.iter().enumerate() {
        for b in &geometry.boxes[i + 1..] {
            assert!(
                !a.rect.inflate(4.0).intersects(&b.rect.inflate(4.0)),
                "boxes {} and {} are closer than 8 px",
                a.node,
                b.node
            );
        }
    }
    for e in &geometry.edges {
        assert!(
            e.points.len() >= 2,
            "edge {} has {} points",
            e.index,
            e.points.len()
        );
        let edge = &spec.edges[e.index];
        for p in &e.points {
            finite(p.x);
            finite(p.y);
        }
        for w in e.points.windows(2) {
            assert!(
                is_axis_aligned(w[0], w[1]),
                "edge {} has a slanted segment",
                e.index
            );
            for b in &geometry.boxes {
                let own = b.node == edge.from || b.node == edge.to;
                let zone = if own {
                    b.rect.inflate(-0.5)
                } else {
                    b.rect.inflate(1.0)
                };
                assert!(
                    !touches(w[0], w[1], zone),
                    "edge {} passes through box {}",
                    e.index,
                    b.node
                );
            }
        }
        let labels: Vec<&TextLines> = e.label.iter().chain(e.ends.iter().flatten()).collect();
        for l in &labels {
            for b in &geometry.boxes {
                assert!(
                    !l.rect().intersects(&b.rect),
                    "a label of edge {} overlaps box {}",
                    e.index,
                    b.node
                );
            }
        }
        for (i, a) in labels.iter().enumerate() {
            for b in &labels[i + 1..] {
                assert!(
                    !a.rect().intersects(&b.rect()),
                    "two texts of edge {} overlap: {:?} and {:?}",
                    e.index,
                    a.lines,
                    b.lines
                );
            }
        }
    }
    for e in &geometry.edges {
        for l in e.label.iter().chain(e.ends.iter().flatten()) {
            let zone = l.rect().inflate(1.0);
            for other in geometry.edges.iter().filter(|o| o.index != e.index) {
                for w in other.points.windows(2) {
                    assert!(
                        !touches(w[0], w[1], zone),
                        "a label of edge {} ({:?}) touches the route of edge {}: {:?}",
                        e.index,
                        l.lines,
                        other.index,
                        other.points
                    );
                }
            }
        }
    }
    for (i, a) in geometry.edges.iter().enumerate() {
        for b in &geometry.edges[i + 1..] {
            assert!(
                !share_a_run(a, b),
                "edges {} and {} run along one line: {:?} and {:?}",
                a.index,
                b.index,
                a.points,
                b.points
            );
        }
    }
    let Some(bounds) = geometry.bounds() else {
        assert_eq!(geometry.size, (2.0 * MARGIN, 2.0 * MARGIN));
        return;
    };
    assert!(
        (bounds.x - MARGIN).abs() < 1e-3 && (bounds.y - MARGIN).abs() < 1e-3,
        "{bounds:?}"
    );
    assert!((bounds.right() + MARGIN - geometry.size.0).abs() < 1e-3);
    assert!((bounds.bottom() + MARGIN - geometry.size.1).abs() < 1e-3);
}
