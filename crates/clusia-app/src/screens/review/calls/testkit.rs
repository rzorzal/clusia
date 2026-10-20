//! A call graph with every kind of node, for the tests of this module.

use clusia_calls::FunctionId;
use clusia_calls::{CallGraph, GraphEdge, GraphNode, NodeKind};

pub const AUTH: &str = "src/auth/middleware.rs#auth_middleware#5";
pub const TOKEN: &str = "src/auth/store.rs#TokenStore::token#20";
pub const RACE: &str = "tests/refresh.rs#tests::refresh_race#9";
pub const NEW: &str = "src/auth/store.rs#TokenStore::new#5";
pub const REFRESH: &str = "src/auth/refresh.rs#TokenStore::refresh#38";
pub const EXCHANGE: &str = "src/http/client.rs#HttpClient::exchange#12";
pub const SAVE: &str = "src/auth/store.rs#TokenStore::save#60";
pub const LOCK: &str = "outside:tokio::Mutex::lock";
pub const MORE: &str = "more:1";

pub fn node(key: &str, label: &str, sub: &str, kind: NodeKind, level: i8) -> GraphNode {
    let local = !matches!(kind, NodeKind::Outside | NodeKind::More);
    let mut parts = key.splitn(3, '#');
    let path = parts.next().unwrap_or("").to_string();
    let line = parts.nth(1).and_then(|l| l.parse().ok());
    GraphNode {
        key: key.into(),
        label: label.into(),
        sub: sub.into(),
        kind,
        level,
        function: local.then(|| FunctionId(key.into())),
        path: local.then_some(path),
        line: if local { line } else { None },
        signature: if local {
            format!("fn {label}()")
        } else {
            String::new()
        },
        more: 0,
        callers: 0,
        ambiguous: Vec::new(),
    }
}

pub fn edge(from: &str, to: &str, line: u32, changed: bool) -> GraphEdge {
    GraphEdge {
        from: from.into(),
        to: to.into(),
        line,
        changed,
    }
}

/// Two levels of callers, the changed centre, and three kinds of callee, a `+2 more` and an
/// outside node. Nodes (9) are in the order below; edges (8) are listed with the one that
/// skips a rank (`AUTH → REFRESH`) third.
pub fn sample() -> CallGraph {
    let mut more = node(MORE, "+2 more calls", "", NodeKind::More, 1);
    more.more = 2;
    let mut g = CallGraph {
        center: REFRESH.into(),
        depth: 3,
        nodes: vec![
            node(
                AUTH,
                "auth_middleware",
                "not touched · 4 callers",
                NodeKind::Untouched,
                -2,
            ),
            node(
                TOKEN,
                "TokenStore::token",
                "not touched · 4 callers",
                NodeKind::Untouched,
                -1,
            ),
            node(RACE, "tests::refresh_race", "new test", NodeKind::New, -1),
            node(
                NEW,
                "TokenStore::new",
                "signature changed · 4 callers",
                NodeKind::Signature,
                -1,
            ),
            node(
                REFRESH,
                "TokenStore::refresh",
                "+9 −2 · body changed",
                NodeKind::Changed,
                0,
            ),
            node(
                EXCHANGE,
                "HttpClient::exchange",
                "+4 −1 · body changed",
                NodeKind::Changed,
                1,
            ),
            node(
                SAVE,
                "TokenStore::save",
                "moved · +6 −1",
                NodeKind::Moved,
                1,
            ),
            node(
                LOCK,
                "Mutex::lock",
                "tokio · outside this repo",
                NodeKind::Outside,
                1,
            ),
            more,
        ],
        edges: vec![
            edge(AUTH, TOKEN, 30, false),
            edge(TOKEN, REFRESH, 25, false),
            edge(AUTH, REFRESH, 31, false),
            edge(RACE, REFRESH, 12, true),
            edge(NEW, REFRESH, 7, false),
            edge(REFRESH, EXCHANGE, 40, true),
            edge(REFRESH, SAVE, 41, true),
            edge(REFRESH, LOCK, 42, false),
        ],
        hidden_edges: 1,
        breadcrumb: vec![
            "auth_middleware".into(),
            "TokenStore::token".into(),
            "TokenStore::refresh".into(),
        ],
        changed: 4,
        touched: 21,
        cut: false,
    };
    // Callers in the whole index: the centre has 9, `token` 4, the signature change 4.
    for (key, callers) in [(REFRESH, 9), (TOKEN, 4), (NEW, 4)] {
        g.nodes.iter_mut().find(|n| n.key == key).unwrap().callers = callers;
    }
    g
}
