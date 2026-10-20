//! The call subgraph around one function: callers above, callees below, capped per level and
//! overall, with a subtitle per node written from the index alone.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::changes::{ChangeKind, ChangedFunction};
use crate::index::{CallIndex, Function, FunctionId};
use crate::resolve::{Resolved, Target};
use crate::{FIND_MAX, MAX_DEPTH, MAX_GRAPH_NODES, PER_LEVEL};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Changed,
    Signature,
    New,
    Moved,
    Untouched,
    Outside,
    More,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    /// The function id, `outside:<dependency>::<name>` or `more:<level>`.
    pub key: String,
    pub label: String,
    pub sub: String,
    pub kind: NodeKind,
    /// Negative above the centre (callers), positive below it (callees).
    pub level: i8,
    pub function: Option<FunctionId>,
    pub path: Option<String>,
    pub line: Option<u32>,
    /// Filled by the caller from the file's text; see `signature_text`.
    pub signature: String,
    /// For a `More` node, how many nodes it stands for.
    pub more: u32,
    pub ambiguous: Vec<FunctionId>,
    /// Callers of the function in the whole index; 0 for an outside or `More` node.
    pub callers: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub line: u32,
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallGraph {
    pub center: String,
    pub depth: u8,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Calls between visible nodes that are not drawn: they skip a level, go back or recurse.
    pub hidden_edges: u32,
    /// Labels from the top caller down to the centre.
    pub breadcrumb: Vec<String>,
    /// Changed functions of the whole pull request, not only the visible ones.
    pub changed: u32,
    /// Untouched functions that call or are called by a changed one.
    pub touched: u32,
    pub cut: bool,
}

fn is_changed(kind: ChangeKind) -> bool {
    matches!(
        kind,
        ChangeKind::Changed | ChangeKind::Signature | ChangeKind::New | ChangeKind::Moved
    )
}

fn plural(n: u32, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

fn is_test(function: &Function) -> bool {
    function.path.split('/').any(|part| part == "tests") || function.name.starts_with("test")
}

fn empty(index: &CallIndex, center: &FunctionId, depth: u8) -> CallGraph {
    CallGraph {
        center: center.0.clone(),
        depth,
        nodes: Vec::new(),
        edges: Vec::new(),
        hidden_edges: 0,
        breadcrumb: Vec::new(),
        changed: 0,
        touched: 0,
        cut: index.cut,
    }
}

/// A node still to be placed: a local function or an outside call.
#[derive(Clone, PartialEq, Eq)]
struct Candidate {
    key: String,
    outside: bool,
}

/// The graph around `center`, `depth` levels each way (clamped to `1..=MAX_DEPTH`). A level
/// shows `PER_LEVEL` nodes, plus `PER_LEVEL` for every time its signed number is in `expand`,
/// and the whole graph `MAX_GRAPH_NODES`; the rest of a level is one `More` node.
pub fn subgraph(
    index: &CallIndex,
    resolved: &Resolved,
    changed: &[ChangedFunction],
    center: &FunctionId,
    depth: u8,
    expand: &[i8],
) -> CallGraph {
    let depth = depth.clamp(1, MAX_DEPTH);
    let functions: HashMap<&str, &Function> = index
        .functions
        .iter()
        .map(|f| (f.id.0.as_str(), f))
        .collect();
    if !functions.contains_key(center.0.as_str()) {
        return empty(index, center, depth);
    }
    let changes: HashMap<&str, &ChangedFunction> = changed
        .iter()
        .filter(|c| c.kind != ChangeKind::Removed)
        .map(|c| (c.id.0.as_str(), c))
        .collect();
    let kind_of = |key: &str| changes.get(key).map(|c| c.kind);
    let is_changed_key = |key: &str| kind_of(key).is_some_and(is_changed);

    // Every resolved edge by key: the smallest call line per (from, to), who calls whom, and
    // the outside nodes' dependency and name.
    let mut lines: BTreeMap<(&str, String), u32> = BTreeMap::new();
    let mut callers: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    let mut callees: HashMap<&str, BTreeSet<String>> = HashMap::new();
    let mut outside: HashMap<String, (&str, &str)> = HashMap::new();
    for (from, target, line) in &resolved.edges {
        let from = from.0.as_str();
        let targets: Vec<String> = match target {
            Target::Local { ids } => ids.iter().map(|id| id.0.clone()).collect(),
            Target::Outside { dependency, name } => {
                let key = format!("outside:{dependency}::{name}");
                outside.insert(key.clone(), (dependency.as_str(), name.as_str()));
                vec![key]
            }
        };
        for to in targets {
            let slot = lines.entry((from, to.clone())).or_insert(*line);
            *slot = (*slot).min(*line);
            callees.entry(from).or_default().insert(to);
        }
    }
    for (from, target, _) in &resolved.edges {
        if let Target::Local { ids } = target {
            for id in ids {
                callers
                    .entry(id.0.as_str())
                    .or_default()
                    .insert(from.0.as_str());
            }
        }
    }
    let caller_count = |key: &str| callers.get(key).map_or(0, BTreeSet::len) as u32;

    let order = |cands: &mut Vec<Candidate>| {
        cands.sort_by(|a, b| {
            a.outside
                .cmp(&b.outside)
                .then_with(|| {
                    if a.outside {
                        std::cmp::Ordering::Equal
                    } else {
                        (!is_changed_key(&a.key))
                            .cmp(&!is_changed_key(&b.key))
                            .then_with(|| caller_count(&b.key).cmp(&caller_count(&a.key)))
                    }
                })
                .then_with(|| a.key.cmp(&b.key))
        });
    };

    // The level walk: up 1, down 1, up 2, down 2 …
    let mut placed: HashSet<String> = HashSet::from([center.0.clone()]);
    let mut visible: BTreeMap<i8, Vec<String>> = BTreeMap::from([(0, vec![center.0.clone()])]);
    let mut more_nodes: BTreeMap<i8, u32> = BTreeMap::new();
    let mut total = 1usize;
    let steps = (1..=depth as i8).flat_map(|k| [-k, k]);
    for level in steps {
        let before = level - level.signum();
        let mut cands: Vec<Candidate> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for key in visible.get(&before).into_iter().flatten() {
            let next: Vec<(String, bool)> = if level < 0 {
                callers
                    .get(key.as_str())
                    .into_iter()
                    .flatten()
                    .map(|k| (k.to_string(), false))
                    .collect()
            } else {
                callees
                    .get(key.as_str())
                    .into_iter()
                    .flatten()
                    .map(|k| (k.clone(), k.starts_with("outside:")))
                    .collect()
            };
            for (key, is_outside) in next {
                if !placed.contains(&key) && seen.insert(key.clone()) {
                    cands.push(Candidate {
                        key,
                        outside: is_outside,
                    });
                }
            }
        }
        order(&mut cands);
        let avail = MAX_GRAPH_NODES.saturating_sub(total);
        if avail == 0 || cands.is_empty() {
            continue;
        }
        let limit = PER_LEVEL * (1 + expand.iter().filter(|&&e| e == level).count());
        let shown = if cands.len() <= limit && cands.len() <= avail {
            cands.len()
        } else {
            limit.min(avail - 1)
        };
        let keys: Vec<String> = cands[..shown].iter().map(|c| c.key.clone()).collect();
        placed.extend(keys.iter().cloned());
        total += shown;
        if shown < cands.len() {
            more_nodes.insert(level, (cands.len() - shown) as u32);
            total += 1;
        }
        visible.insert(level, keys);
    }

    // The visible nodes by level, with the `More` node after a level's nodes.
    let mut level_of: HashMap<&str, i8> = HashMap::new();
    for (level, keys) in &visible {
        for key in keys {
            level_of.insert(key.as_str(), *level);
        }
    }

    // Ambiguity: the other candidates of every ambiguous call between two visible nodes.
    let mut ambiguous: HashMap<&str, BTreeSet<FunctionId>> = HashMap::new();
    for (from, target, _) in &resolved.edges {
        if let Target::Local { ids } = target
            && ids.len() > 1
            && level_of.contains_key(from.0.as_str())
        {
            for id in ids {
                if level_of.contains_key(id.0.as_str()) {
                    ambiguous
                        .entry(id.0.as_str())
                        .or_default()
                        .extend(ids.iter().filter(|other| *other != id).cloned());
                }
            }
        }
    }

    let mut nodes: Vec<GraphNode> = Vec::new();
    for (level, keys) in &visible {
        for key in keys {
            let node = match functions.get(key.as_str()) {
                Some(function) => {
                    let callers = caller_count(key);
                    let change = changes.get(key.as_str());
                    let (kind, sub) = match change {
                        Some(c) => match c.kind {
                            ChangeKind::Changed => (
                                NodeKind::Changed,
                                format!("+{} −{} · body changed", c.added, c.removed),
                            ),
                            ChangeKind::Signature => (
                                NodeKind::Signature,
                                format!(
                                    "signature changed · {}",
                                    plural(callers, "caller", "callers")
                                ),
                            ),
                            ChangeKind::Moved => (
                                NodeKind::Moved,
                                format!("moved · +{} −{}", c.added, c.removed),
                            ),
                            _ if is_test(function) => (NodeKind::New, "new test".to_string()),
                            _ => (NodeKind::New, format!("new · +{}", c.added)),
                        },
                        None => (
                            NodeKind::Untouched,
                            format!("not touched · {}", plural(callers, "caller", "callers")),
                        ),
                    };
                    GraphNode {
                        key: key.clone(),
                        label: function.qualified.clone(),
                        sub,
                        kind,
                        level: *level,
                        function: Some(function.id.clone()),
                        path: Some(function.path.clone()),
                        line: Some(function.start_line),
                        signature: String::new(),
                        more: 0,
                        ambiguous: ambiguous
                            .get(key.as_str())
                            .map(|set| set.iter().cloned().collect())
                            .unwrap_or_default(),
                        callers,
                    }
                }
                None => {
                    let (dependency, name) = outside.get(key).copied().unwrap_or(("", key));
                    GraphNode {
                        key: key.clone(),
                        label: name.to_string(),
                        sub: format!("{dependency} · outside this repo"),
                        kind: NodeKind::Outside,
                        level: *level,
                        function: None,
                        path: None,
                        line: None,
                        signature: String::new(),
                        more: 0,
                        ambiguous: Vec::new(),
                        callers: 0,
                    }
                }
            };
            nodes.push(node);
        }
        if let Some(&hidden) = more_nodes.get(level) {
            nodes.push(GraphNode {
                key: format!("more:{level}"),
                label: if *level < 0 {
                    format!("+{hidden} more callers")
                } else {
                    format!("+{hidden} more calls")
                },
                sub: String::new(),
                kind: NodeKind::More,
                level: *level,
                function: None,
                path: None,
                line: None,
                signature: String::new(),
                more: hidden,
                ambiguous: Vec::new(),
                callers: 0,
            });
        }
    }
    // A `More` node of a level that placed no node of its own cannot exist (it needs a
    // visible candidate), so every level with a `More` node was walked above.

    let mut edges: Vec<(i8, GraphEdge)> = Vec::new();
    let mut hidden_edges = 0u32;
    for ((from, to), line) in &lines {
        let (Some(&from_level), Some(&to_level)) = (level_of.get(from), level_of.get(to.as_str()))
        else {
            continue;
        };
        if to_level == from_level + 1 {
            edges.push((
                from_level,
                GraphEdge {
                    from: (*from).to_string(),
                    to: to.clone(),
                    line: *line,
                    changed: is_changed_key(from) || is_changed_key(to),
                },
            ));
        } else {
            hidden_edges += 1;
        }
    }
    edges.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.from.cmp(&b.1.from))
            .then_with(|| a.1.to.cmp(&b.1.to))
    });
    let edges: Vec<GraphEdge> = edges.into_iter().map(|(_, e)| e).collect();

    // The longest chain of visible callers, one per level, that ends at the centre; the first
    // caller in level order wins a tie. Worked out from the top level down, so each node keeps
    // only the best chain above it.
    let mut above: HashMap<&str, Vec<&str>> = HashMap::new();
    for level in -(depth as i8)..=0 {
        for key in visible.get(&level).into_iter().flatten() {
            let mut best: Option<Vec<&str>> = None;
            for caller in visible.get(&(level - 1)).into_iter().flatten() {
                let calls_key = callees
                    .get(caller.as_str())
                    .is_some_and(|set| set.contains(key));
                let Some(chain) = above.get(caller.as_str()).filter(|_| calls_key) else {
                    continue;
                };
                if best.as_ref().is_none_or(|b| chain.len() + 1 > b.len()) {
                    let mut longer = chain.clone();
                    longer.push(caller.as_str());
                    best = Some(longer);
                }
            }
            above.insert(key.as_str(), best.unwrap_or_default());
        }
    }
    let mut breadcrumb: Vec<String> = above
        .get(center.0.as_str())
        .into_iter()
        .flatten()
        .filter_map(|k| functions.get(k).map(|f| f.qualified.clone()))
        .collect();
    breadcrumb.push(functions[center.0.as_str()].qualified.clone());

    let mut touched: HashSet<&str> = HashSet::new();
    for (from, target, _) in &resolved.edges {
        if let Target::Local { ids } = target {
            for id in ids {
                match (is_changed_key(&from.0), is_changed_key(&id.0)) {
                    (true, false) => {
                        touched.insert(id.0.as_str());
                    }
                    (false, true) => {
                        touched.insert(from.0.as_str());
                    }
                    _ => {}
                }
            }
        }
    }

    CallGraph {
        center: center.0.clone(),
        depth,
        nodes,
        edges,
        hidden_edges,
        breadcrumb,
        changed: changes.values().filter(|c| is_changed(c.kind)).count() as u32,
        touched: touched.len() as u32,
        cut: index.cut,
    }
}

/// Functions whose qualified name matches `query`, case-insensitively: an exact qualified
/// name first, then an exact bare name, then a substring of the qualified name; by id within a
/// rank, at most `FIND_MAX`.
pub fn find(index: &CallIndex, query: &str) -> Vec<FunctionId> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(u8, &FunctionId)> = index
        .functions
        .iter()
        .filter_map(|f| {
            let qualified = f.qualified.to_lowercase();
            let rank = if qualified == query {
                0
            } else if f.name.to_lowercase() == query {
                1
            } else if qualified.contains(&query) {
                2
            } else {
                return None;
            };
            Some((rank, &f.id))
        })
        .collect();
    hits.sort();
    hits.into_iter()
        .take(FIND_MAX)
        .map(|(_, id)| id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{cpu_timed, generated_repo, index_of, index_with};
    use crate::{MAX_GRAPH_NODES, resolve};

    const API: &str = "pub fn handler() {\n    auth_middleware();\n}\n\npub fn auth_middleware() {\n    token();\n}\n\npub fn token() {\n    refresh();\n    helper();\n    tokio::spawn(1);\n}\n\npub fn refresh() {\n    save();\n    std::fs::read(\"p\");\n}\n\npub fn save() {}\n\npub fn helper() {}\n";

    fn id(v: &str) -> FunctionId {
        FunctionId(v.to_string())
    }

    fn changed(i: &str, kind: ChangeKind, added: u32, removed: u32) -> ChangedFunction {
        ChangedFunction {
            id: id(i),
            kind,
            added,
            removed,
            moved_from: None,
        }
    }

    fn graph(
        files: &[(&str, &str)],
        deps: &[&str],
        changes: &[ChangedFunction],
        center: &str,
        depth: u8,
        expand: &[i8],
    ) -> CallGraph {
        let index = index_with(files, deps);
        subgraph(
            &index,
            &resolve(&index),
            changes,
            &id(center),
            depth,
            expand,
        )
    }

    fn api_graph(depth: u8) -> CallGraph {
        let changes = [
            changed("src/api.rs#refresh#15", ChangeKind::Changed, 4, 1),
            changed("src/api.rs#save#20", ChangeKind::New, 1, 0),
        ];
        graph(
            &[("src/api.rs", API)],
            &["tokio"],
            &changes,
            "src/api.rs#refresh#15",
            depth,
            &[],
        )
    }

    fn nodes_of(g: &CallGraph) -> Vec<(&str, &str, &str, NodeKind, i8)> {
        g.nodes
            .iter()
            .map(|n| {
                (
                    n.key.as_str(),
                    n.label.as_str(),
                    n.sub.as_str(),
                    n.kind,
                    n.level,
                )
            })
            .collect()
    }

    fn edges_of(g: &CallGraph) -> Vec<(&str, &str, u32, bool)> {
        g.edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str(), e.line, e.changed))
            .collect()
    }

    #[test]
    fn a_changed_function_with_callers_above_and_callees_below() {
        let g = api_graph(3);
        assert_eq!(
            nodes_of(&g),
            vec![
                (
                    "src/api.rs#handler#1",
                    "handler",
                    "not touched · 0 callers",
                    NodeKind::Untouched,
                    -3
                ),
                (
                    "src/api.rs#auth_middleware#5",
                    "auth_middleware",
                    "not touched · 1 caller",
                    NodeKind::Untouched,
                    -2
                ),
                (
                    "src/api.rs#token#9",
                    "token",
                    "not touched · 1 caller",
                    NodeKind::Untouched,
                    -1
                ),
                (
                    "src/api.rs#refresh#15",
                    "refresh",
                    "+4 −1 · body changed",
                    NodeKind::Changed,
                    0
                ),
                ("src/api.rs#save#20", "save", "new · +1", NodeKind::New, 1),
                (
                    "outside:std::read",
                    "read",
                    "std · outside this repo",
                    NodeKind::Outside,
                    1
                ),
            ]
        );
        assert_eq!(
            edges_of(&g),
            vec![
                (
                    "src/api.rs#handler#1",
                    "src/api.rs#auth_middleware#5",
                    2,
                    false
                ),
                (
                    "src/api.rs#auth_middleware#5",
                    "src/api.rs#token#9",
                    6,
                    false
                ),
                ("src/api.rs#token#9", "src/api.rs#refresh#15", 10, true),
                ("src/api.rs#refresh#15", "outside:std::read", 17, true),
                ("src/api.rs#refresh#15", "src/api.rs#save#20", 16, true),
            ]
        );
        assert_eq!(
            (
                g.center.as_str(),
                g.depth,
                g.hidden_edges,
                g.changed,
                g.touched,
                g.cut
            ),
            ("src/api.rs#refresh#15", 3, 0, 2, 1, false)
        );
        assert_eq!(
            g.breadcrumb,
            vec!["handler", "auth_middleware", "token", "refresh"]
        );
        let callers: Vec<u32> = g.nodes.iter().map(|n| n.callers).collect();
        assert_eq!(
            callers,
            vec![0, 1, 1, 1, 1, 0],
            "callers of each node in the whole index; 0 for the outside node"
        );
        let centre = g.nodes.iter().find(|n| n.level == 0).expect("centre");
        assert_eq!(
            (
                centre.path.as_deref(),
                centre.line,
                centre.signature.as_str(),
                centre.more
            ),
            (Some("src/api.rs"), Some(15), "", 0)
        );
        assert_eq!(centre.function, Some(id("src/api.rs#refresh#15")));
        assert!(centre.ambiguous.is_empty());
        let out = g
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Outside)
            .expect("outside");
        assert_eq!(
            (out.function.clone(), out.path.clone(), out.line),
            (None, None, None)
        );
    }

    #[test]
    fn depth_one_shows_one_level_each_way_and_depth_is_clamped() {
        let g = api_graph(1);
        let keys: Vec<&str> = g.nodes.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "src/api.rs#token#9",
                "src/api.rs#refresh#15",
                "src/api.rs#save#20",
                "outside:std::read"
            ]
        );
        assert_eq!(g.edges.len(), 3);
        assert_eq!(g.breadcrumb, vec!["token", "refresh"]);
        assert_eq!(api_graph(9).depth, 5);
        assert_eq!(api_graph(0).depth, 1);
    }

    #[test]
    fn the_breadcrumb_follows_the_longest_visible_chain_of_callers() {
        const SRC: &str = "pub fn race() {\n    refresh();\n}\n\npub fn handler() {\n    token();\n}\n\npub fn token() {\n    refresh();\n}\n\npub fn refresh() {}\n";
        let changes = [
            changed("src/a.rs#refresh#13", ChangeKind::Changed, 1, 1),
            changed("src/a.rs#race#1", ChangeKind::New, 3, 0),
        ];
        let g = graph(
            &[("src/a.rs", SRC)],
            &[],
            &changes,
            "src/a.rs#refresh#13",
            3,
            &[],
        );
        assert_eq!(
            g.breadcrumb,
            vec!["handler", "token", "refresh"],
            "the changed caller that nothing calls is a dead end"
        );
    }

    #[test]
    fn an_unknown_centre_gives_an_empty_graph() {
        let g = graph(&[("src/api.rs", API)], &[], &[], "nope#x#1", 3, &[]);
        assert_eq!(
            (
                g.center.as_str(),
                g.nodes.len(),
                g.edges.len(),
                g.breadcrumb.len(),
                g.changed,
                g.touched
            ),
            ("nope#x#1", 0, 0, 0, 0, 0)
        );
    }

    fn hub_source(leaves: usize) -> String {
        let mut s = String::from("pub fn hub() {\n");
        for i in 0..leaves {
            s.push_str(&format!("    leaf_{i:02}();\n"));
        }
        s.push_str("}\n");
        for i in 0..leaves {
            s.push_str(&format!("pub fn leaf_{i:02}() {{}}\n"));
        }
        s
    }

    #[test]
    fn a_level_shows_eight_changed_first_and_one_more_node_that_expands_by_eight() {
        let src = hub_source(12);
        let changes = [changed("src/hub.rs#leaf_11#26", ChangeKind::Changed, 1, 0)];
        let g = graph(
            &[("src/hub.rs", src.as_str())],
            &[],
            &changes,
            "src/hub.rs#hub#1",
            1,
            &[],
        );
        let shown: Vec<&str> = g
            .nodes
            .iter()
            .filter(|n| n.level == 1 && n.kind != NodeKind::More)
            .map(|n| n.label.as_str())
            .collect();
        assert_eq!(
            shown,
            vec![
                "leaf_11", "leaf_00", "leaf_01", "leaf_02", "leaf_03", "leaf_04", "leaf_05",
                "leaf_06"
            ]
        );
        let more = g
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::More)
            .expect("more");
        assert_eq!(
            (
                more.key.as_str(),
                more.label.as_str(),
                more.more,
                more.level,
                more.sub.as_str()
            ),
            ("more:1", "+4 more calls", 4, 1, "")
        );
        assert_eq!(g.edges.len(), 8, "the More node has no edge");
        assert_eq!(more.callers, 0);

        for expand in [vec![1], vec![1, 1]] {
            let g = graph(
                &[("src/hub.rs", src.as_str())],
                &[],
                &changes,
                "src/hub.rs#hub#1",
                1,
                &expand,
            );
            assert_eq!(g.nodes.len(), 13);
            assert!(g.nodes.iter().all(|n| n.kind != NodeKind::More));
        }
        let other = graph(
            &[("src/hub.rs", src.as_str())],
            &[],
            &changes,
            "src/hub.rs#hub#1",
            1,
            &[2],
        );
        assert!(other.nodes.iter().any(|n| n.kind == NodeKind::More));
    }

    #[test]
    fn callers_overflow_into_a_more_callers_node() {
        let mut src = String::from("pub fn sink() {}\n");
        for i in 0..9 {
            src.push_str(&format!("pub fn c_{i}() {{ sink(); }}\n"));
        }
        let g = graph(
            &[("src/sink.rs", src.as_str())],
            &[],
            &[],
            "src/sink.rs#sink#1",
            1,
            &[],
        );
        let more = g
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::More)
            .expect("more");
        assert_eq!(
            (
                more.key.as_str(),
                more.label.as_str(),
                more.more,
                more.level
            ),
            ("more:-1", "+1 more callers", 1, -1)
        );
        assert_eq!(g.nodes.iter().filter(|n| n.level == -1).count(), 9);
        let centre = g.nodes.iter().find(|n| n.level == 0).expect("centre");
        assert_eq!(
            centre.callers, 9,
            "all nine callers count, not only the eight shown"
        );
    }

    fn ladder_source() -> String {
        let mut s = String::from("pub fn c() {\n");
        for i in 0..10 {
            s.push_str(&format!("    dn1_{i}();\n"));
        }
        s.push_str("}\n");
        for k in 1..=5 {
            for i in 0..10 {
                let callee = if k == 1 {
                    "c".to_string()
                } else {
                    format!("up{}_0", k - 1)
                };
                s.push_str(&format!("pub fn up{k}_{i}() {{\n    {callee}();\n}}\n"));
            }
        }
        for k in 1..=5 {
            for i in 0..10 {
                if i == 0 && k < 5 {
                    let calls: String = (0..10)
                        .map(|j| format!("    dn{}_{j}();\n", k + 1))
                        .collect();
                    s.push_str(&format!("pub fn dn{k}_{i}() {{\n{calls}}}\n"));
                } else {
                    s.push_str(&format!("pub fn dn{k}_{i}() {{}}\n"));
                }
            }
        }
        s
    }

    #[test]
    fn the_whole_graph_stops_at_sixty_nodes_and_never_draws_a_skipped_level() {
        let src = ladder_source();
        let index = index_of(&[("src/ladder.rs", src.as_str())]);
        let centre = index
            .functions
            .iter()
            .find(|f| f.qualified == "c")
            .expect("c")
            .id
            .clone();
        let g = subgraph(&index, &resolve(&index), &[], &centre, 5, &[]);
        assert_eq!(g.nodes.len(), MAX_GRAPH_NODES);
        let at = |level: i8| g.nodes.iter().filter(|n| n.level == level).count();
        assert_eq!((at(-1), at(1), at(-3), at(3)), (9, 9, 9, 9));
        assert_eq!(at(-4), 5);
        let more = g
            .nodes
            .iter()
            .find(|n| n.level == -4 && n.kind == NodeKind::More)
            .expect("more");
        assert_eq!(more.more, 6);
        assert!(
            g.nodes
                .iter()
                .all(|n| n.level.abs() <= 4 && n.level != 4 && n.level != 5)
        );
        assert_eq!(g.hidden_edges, 0);
        let first = g.nodes.iter().find(|n| n.level == -1).expect("level -1");
        assert_eq!(first.label, "up1_0", "the most called caller comes first");
    }

    #[test]
    fn a_call_that_goes_back_up_is_hidden_and_counted() {
        let g = graph(
            &[(
                "src/m.rs",
                "pub fn a() {\n    b();\n}\npub fn b() {\n    a();\n}\n",
            )],
            &[],
            &[],
            "src/m.rs#a#1",
            2,
            &[],
        );
        let keys: Vec<(&str, i8)> = g
            .nodes
            .iter()
            .map(|n| (n.label.as_str(), n.level))
            .collect();
        assert_eq!(keys, vec![("b", -1), ("a", 0)]);
        assert_eq!(
            edges_of(&g),
            vec![("src/m.rs#b#4", "src/m.rs#a#1", 5, false)]
        );
        assert_eq!(g.hidden_edges, 1);
    }

    #[test]
    fn a_recursive_function_is_one_node_and_one_hidden_edge() {
        let g = graph(
            &[("src/r.rs", "pub fn r() {\n    r();\n}\n")],
            &[],
            &[],
            "src/r.rs#r#1",
            3,
            &[],
        );
        assert_eq!((g.nodes.len(), g.edges.len(), g.hidden_edges), (1, 0, 1));
    }

    #[test]
    fn an_ambiguous_call_links_every_candidate_and_names_the_others() {
        let g = graph(
            &[
                ("src/a.rs", "pub fn dup() {}\n"),
                ("src/b.rs", "pub fn dup() {}\n"),
                ("src/main.rs", "pub fn run() {\n    dup();\n}\n"),
            ],
            &[],
            &[],
            "src/main.rs#run#1",
            1,
            &[],
        );
        assert_eq!(g.edges.len(), 2);
        let a = g
            .nodes
            .iter()
            .find(|n| n.key == "src/a.rs#dup#1")
            .expect("a");
        let b = g
            .nodes
            .iter()
            .find(|n| n.key == "src/b.rs#dup#1")
            .expect("b");
        assert_eq!(a.ambiguous, vec![id("src/b.rs#dup#1")]);
        assert_eq!(b.ambiguous, vec![id("src/a.rs#dup#1")]);
    }

    #[test]
    fn subtitles_for_moved_signature_and_new_test_functions() {
        let files = [
            ("src/lib.rs", "pub fn refresh() {}\n"),
            ("tests/api.rs", "fn test_refresh() {\n    refresh();\n}\n"),
        ];
        let centre_sub = |kind: ChangeKind, added: u32, removed: u32| {
            let g = graph(
                &files,
                &[],
                &[changed("src/lib.rs#refresh#1", kind, added, removed)],
                "src/lib.rs#refresh#1",
                1,
                &[],
            );
            g.nodes
                .iter()
                .find(|n| n.level == 0)
                .expect("centre")
                .sub
                .clone()
        };
        assert_eq!(centre_sub(ChangeKind::Moved, 6, 1), "moved · +6 −1");
        assert_eq!(
            centre_sub(ChangeKind::Signature, 2, 1),
            "signature changed · 1 caller"
        );
        let g = graph(
            &files,
            &[],
            &[changed(
                "tests/api.rs#test_refresh#1",
                ChangeKind::New,
                2,
                0,
            )],
            "src/lib.rs#refresh#1",
            1,
            &[],
        );
        let caller = g.nodes.iter().find(|n| n.level == -1).expect("caller");
        assert_eq!(
            (caller.sub.as_str(), caller.kind),
            ("new test", NodeKind::New)
        );
    }

    #[test]
    fn five_thousand_functions_give_a_graph_inside_the_budget() {
        let repo = generated_repo(500, 10);
        let files: Vec<(&str, &str)> = repo.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
        let index = index_of(&files);
        let resolved = resolve(&index);
        let centre = index.functions[0].id.clone();
        let (g, took) = cpu_timed(|| subgraph(&index, &resolved, &[], &centre, 5, &[]));
        assert!(took.as_secs() < 2, "{took:?}");
        assert!(g.nodes.len() <= MAX_GRAPH_NODES);
        assert_eq!(subgraph(&index, &resolved, &[], &centre, 5, &[]), g);
    }

    fn find_ids(query: &str) -> Vec<String> {
        let index = index_of(&[
            ("src/a.rs", "pub fn refresh() {}\n"),
            ("src/b.rs", "impl TokenStore { pub fn refresh(&self) {} }\n"),
            ("src/c.rs", "impl Cache { pub fn refresh_all(&self) {} }\n"),
            ("src/d.rs", "pub fn other() {}\n"),
        ]);
        find(&index, query).into_iter().map(|i| i.0).collect()
    }

    #[test]
    fn find_ranks_exact_then_same_name_then_substring() {
        assert_eq!(
            find_ids("REFRESH"),
            vec![
                "src/a.rs#refresh#1",
                "src/b.rs#TokenStore::refresh#1",
                "src/c.rs#Cache::refresh_all#1"
            ]
        );
        assert_eq!(
            find_ids("tokenstore::refr"),
            vec!["src/b.rs#TokenStore::refresh#1"]
        );
        assert!(find_ids("zzz").is_empty());
        assert!(find_ids("  ").is_empty());
    }

    #[test]
    fn find_stops_at_twenty() {
        let src: String = (0..30)
            .map(|i| format!("pub fn f_{i:02}() {{}}\n"))
            .collect();
        let index = index_of(&[("src/f.rs", src.as_str())]);
        let hits = find(&index, "f_");
        assert_eq!(hits.len(), crate::FIND_MAX);
        assert_eq!(hits[0].0, "src/f.rs#f_00#1");
    }
}
