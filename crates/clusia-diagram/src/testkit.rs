//! Helpers for the layout tests: specs from JSON and the invariants every kind must keep.

use clusia_core::diagram::{DiagramKind, DiagramSpec, Shape};
use serde_json::{Value, json};

use crate::geometry::{ActorEnd, share_past_trunk, touches};
use crate::{Arrow, EdgeGeo, Geometry, MARGIN, Point, Rect, TextLines};

/// The calling thread's CPU time. Budgets read this instead of the wall clock so a loaded
/// machine, where the thread waits for a CPU, does not fail work that is itself fast.
pub(crate) fn thread_cpu_time() -> std::time::Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the whole call.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    assert_eq!(rc, 0, "the thread CPU clock is unavailable");
    std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// The CPU time one layout may take, in milliseconds.
pub(crate) const BUDGET_MS: u128 = if cfg!(debug_assertions) { 200 } else { 20 };

/// The CPU time `work` took on this thread, with what it returned.
pub(crate) fn cpu_timed<T>(work: impl FnOnce() -> T) -> (T, std::time::Duration) {
    let started = thread_cpu_time();
    let out = work();
    (out, thread_cpu_time() - started)
}

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

/// A chain of `n - leaves` nodes with `leaves` more joined to nodes of it, from a fixed
/// pseudo-random sequence: the shape that has a spine with branches.
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
    let leaves = 1 + next(3);
    let chain = n.saturating_sub(leaves).max(2);
    let mut edges: Vec<Value> = (1..chain)
        .map(|i| {
            edge(
                &format!("n{}", i - 1),
                &format!("n{i}"),
                Some("calls the other module"),
            )
        })
        .collect();
    for leaf in chain..n {
        let parent = next(chain);
        let (a, b) = if next(3) == 0 {
            (leaf, parent)
        } else {
            (parent, leaf)
        };
        edges.push(edge(&format!("n{a}"), &format!("n{b}"), Some("leaf")));
    }
    spec(kind, nodes, edges)
}

fn is_axis_aligned(a: Point, b: Point) -> bool {
    (a.x - b.x).abs() < 1e-3 || (a.y - b.y).abs() < 1e-3
}

/// The actor end of edge `e` in a use case diagram, as `share_past_trunk` takes it.
fn actor_end(spec: &DiagramSpec, e: &EdgeGeo) -> ActorEnd {
    if spec.kind != DiagramKind::UseCases || e.arrow != Arrow::None {
        return None;
    }
    let actor = |id: &str| {
        spec.nodes
            .iter()
            .position(|n| n.id == id && matches!(n.shape, Shape::Actor | Shape::External))
    };
    let edge = &spec.edges[e.index];
    match (actor(&edge.from), actor(&edge.to)) {
        (Some(a), None) => Some((a, true)),
        (None, Some(a)) => Some((a, false)),
        _ => None,
    }
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
/// routes sideways or up and down only and clear of other boxes, labels off boxes, off the
/// other texts of their edge and off every route (their own may only run beside them), no two
/// edges along one line (but two links of one actor on its trunk), and everything inside
/// `size` and starting at the margin.
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
            // Beside its own run a label may touch it, but no run of its own route may cut it.
            let inside = l.rect().inflate(-0.5);
            for w in e.points.windows(2) {
                assert!(
                    !touches(w[0], w[1], inside),
                    "a label of edge {} ({:?}) is crossed by its own route: {:?}",
                    e.index,
                    l.lines,
                    e.points
                );
            }
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
            let ends = (actor_end(spec, a), actor_end(spec, b));
            assert!(
                !share_past_trunk(&a.points, &b.points, ends),
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

#[cfg(test)]
mod clock_tests {
    use super::*;
    use std::time::Duration;

    /// Busy work that stops once the thread has had `ms` of CPU, however long the wall clock
    /// takes to give it; the wall bound only keeps a dead clock from hanging the test.
    fn spin(ms: u64) {
        let wall = std::time::Instant::now();
        let started = thread_cpu_time();
        let mut x = 0u64;
        while thread_cpu_time() - started < Duration::from_millis(ms)
            && wall.elapsed() < Duration::from_secs(30)
        {
            x = std::hint::black_box(x.wrapping_add(1));
        }
    }

    #[test]
    fn cpu_time_grows_with_busy_work() {
        let ((), took) = cpu_timed(|| spin(40));
        assert!(took >= Duration::from_millis(40), "{took:?}");
    }

    #[test]
    fn cpu_time_barely_moves_during_a_sleep() {
        let ((), took) = cpu_timed(|| std::thread::sleep(Duration::from_millis(100)));
        assert!(took < Duration::from_millis(20), "{took:?}");
    }

    #[test]
    fn cpu_time_never_goes_backwards() {
        let a = thread_cpu_time();
        spin(5);
        assert!(thread_cpu_time() >= a);
    }
}
