//! Which nodes of a left-to-right diagram share a row and which hang below another node.
//!
//! The spine is the longest path of the acyclic graph, cut where the row would grow wider than
//! `MAX_ROW`. Every other node joins the column of its highest placed neighbour, below it, once
//! all of its neighbours are placed. A diagram that does not break down this way has no plan
//! and keeps the plain layered placement.

use std::cmp::Reverse;

use crate::layered::NodeSize;

/// The widest a spine may grow, boxes and the 96 px gaps between them together.
const MAX_ROW: f32 = 1000.0;
const ROW_GAP: f32 = 96.0;

pub(crate) struct Plan {
    /// The column of each node: its index on the spine, or that of the spine node above it.
    pub(crate) layer: Vec<usize>,
    /// Where the node stands in its column: 0 for the spine node, then 1, 2, … downwards.
    pub(crate) depth: Vec<usize>,
}

/// A plan for the nodes `sizes` joined by the acyclic edges `dag`, whose ranks are `rank`.
///
/// `None` when the diagram has no branch and is no hub with spokes, when some node is neither
/// on the spine nor a branch, or when two nodes of one column are joined without being
/// neighbours in it.
pub(crate) fn plan(sizes: &[NodeSize], dag: &[(usize, usize)], rank: &[usize]) -> Option<Plan> {
    let n = sizes.len();
    if dag.is_empty() {
        return None;
    }
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut nbrs: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(u, v) in dag {
        out[u].push(v);
        for (a, b) in [(u, v), (v, u)] {
            if !nbrs[a].contains(&b) {
                nbrs[a].push(b);
            }
        }
    }
    // Nodes in the order of falling rank, so every successor comes before its predecessors.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&v| Reverse(rank[v]));
    let mut longest = vec![1usize; n];
    for &u in &order {
        for &v in &out[u] {
            longest[u] = longest[u].max(longest[v] + 1);
        }
    }
    let first = (0..n).max_by_key(|&v| (longest[v], Reverse(v)))?;
    let mut spine = vec![first];
    while let Some(&last) = spine.last() {
        match out[last].iter().find(|&&v| longest[v] + 1 == longest[last]) {
            Some(&v) => spine.push(v),
            None => break,
        }
    }
    // A hub with spokes has a path of two nodes: the hub goes in the middle, the first spoke on
    // its left and the next one that joins nothing else on its right.
    let mut star = false;
    if let [hub, spoke] = spine[..]
        && let Some(&other) = nbrs[hub]
            .iter()
            .find(|&&w| w != spoke && nbrs[w].len() == 1)
    {
        spine = vec![spoke, hub, other];
        star = true;
    }
    let mut width = sizes[first].w;
    let mut keep = 1;
    for &v in &spine[1..] {
        width += ROW_GAP + sizes[v].w;
        if width > MAX_ROW {
            break;
        }
        keep += 1;
    }
    spine.truncate(keep);

    let mut layer = vec![usize::MAX; n];
    let mut depth = vec![0usize; n];
    let mut height = vec![1usize; spine.len()];
    for (i, &v) in spine.iter().enumerate() {
        layer[v] = i;
    }
    loop {
        let mut grew = false;
        for v in 0..n {
            if layer[v] != usize::MAX
                || nbrs[v].is_empty()
                || nbrs[v].iter().any(|&w| layer[w] == usize::MAX)
            {
                continue;
            }
            let anchor = nbrs[v]
                .iter()
                .copied()
                .max_by_key(|&w| (layer[w], depth[w], Reverse(w)))?;
            layer[v] = layer[anchor];
            depth[v] = height[layer[v]];
            height[layer[v]] += 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    if layer.contains(&usize::MAX) || (!star && depth.iter().all(|&d| d == 0)) {
        return None;
    }
    let joined_apart = dag
        .iter()
        .any(|&(u, v)| layer[u] == layer[v] && depth[u].abs_diff(depth[v]) != 1);
    if joined_apart {
        return None;
    }
    Some(Plan { layer, depth })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sized(n: usize, w: f32) -> Vec<NodeSize> {
        vec![
            NodeSize {
                w,
                h: 60.0,
                fixed: false
            };
            n
        ]
    }

    fn ranks_of(n: usize, dag: &[(usize, usize)]) -> Vec<usize> {
        let mut rank = vec![0usize; n];
        for _ in 0..n {
            for &(u, v) in dag {
                rank[v] = rank[v].max(rank[u] + 1);
            }
        }
        rank
    }

    fn plan_of(n: usize, w: f32, dag: &[(usize, usize)]) -> Option<Plan> {
        plan(&sized(n, w), dag, &ranks_of(n, dag))
    }

    #[test]
    fn a_leaf_of_the_middle_hangs_under_its_neighbour() {
        // 0 -> 1 -> 2, and 1 -> 3.
        let p = plan_of(4, 200.0, &[(0, 1), (1, 2), (1, 3)]).expect("plan");
        assert_eq!(p.layer, vec![0, 1, 2, 1]);
        assert_eq!(p.depth, vec![0, 0, 0, 1]);
    }

    #[test]
    fn a_spine_wider_than_the_row_leaves_its_tail_below() {
        // Four 200 px boxes need 1088 px, so the last one hangs under the third.
        let p = plan_of(4, 200.0, &[(0, 1), (1, 2), (2, 3)]).expect("plan");
        assert_eq!(p.layer, vec![0, 1, 2, 2]);
        assert_eq!(p.depth, vec![0, 0, 0, 1]);
    }

    #[test]
    fn a_node_joined_to_two_columns_hangs_under_the_later_one() {
        // 0 -> 1 -> 2 -> 3 with a shortcut 1 -> 3: the spine is cut after three boxes, and 3 is
        // joined to the columns of 1 and 2.
        let p = plan_of(4, 200.0, &[(0, 1), (1, 2), (1, 3), (2, 3)]).expect("plan");
        assert_eq!(p.layer, vec![0, 1, 2, 2]);
        assert_eq!(p.depth, vec![0, 0, 0, 1]);
    }

    #[test]
    fn a_hub_with_two_spokes_sits_between_them() {
        // 0 -> 1 and 0 -> 2: the hub 0 goes in the middle, 1 on its left and 2 on its right.
        let p = plan_of(3, 200.0, &[(0, 1), (0, 2)]).expect("a plan");
        assert_eq!(p.layer, vec![1, 0, 2]);
        assert_eq!(p.depth, vec![0, 0, 0]);
    }

    #[test]
    fn a_plain_chain_has_no_plan() {
        assert!(plan_of(3, 200.0, &[(0, 1), (1, 2)]).is_none());
    }

    #[test]
    fn a_node_with_no_neighbour_has_no_plan() {
        assert!(plan_of(4, 200.0, &[(0, 1), (1, 2)]).is_none());
    }

    #[test]
    fn two_nodes_of_a_column_that_are_not_next_to_each_other_have_no_plan() {
        // 3 and 4 both hang under 1, so 4 stands two places below the node it is joined to.
        assert!(plan_of(5, 200.0, &[(0, 1), (1, 2), (1, 3), (1, 4)]).is_none());
    }
}
