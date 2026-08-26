//! J#5: lineage graph view. Renders the parent/child run DAG
//! as a simple JSON adjacency list for the dashboard.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Stable identifier for a single run, used as a node label in
/// the lineage graph.
pub type RunId = String;

/// Adjacency-list view of the parent/child run DAG for the
/// dashboard. `nodes` lists each run id exactly once;
/// `edges` lists `(parent, child)` pairs in input order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineageGraph {
    /// Distinct run ids referenced by the graph.
    pub nodes: Vec<RunId>,
    /// `(parent, child)` edges in the order they were observed.
    pub edges: Vec<(RunId, RunId)>,
}

impl LineageGraph {
    /// Empty graph with no nodes and no edges.
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }
    /// Build the graph from a flat list of `(parent, child)`
    /// pairs. Nodes are deduplicated in first-seen order.
    pub fn from_pairs(pairs: &[(String, String)]) -> Self {
        tracing::trace!(pair_count = pairs.len(), "LineageGraph::from_pairs: enter");
        let mut nodes: Vec<RunId> = Vec::new();
        let mut edges: Vec<(RunId, RunId)> = Vec::new();
        let mut seen: HashMap<String, ()> = HashMap::new();
        for (parent, child) in pairs {
            if !seen.contains_key(parent) {
                seen.insert(parent.clone(), ());
                nodes.push(parent.clone());
            }
            if !seen.contains_key(child) {
                seen.insert(child.clone(), ());
                nodes.push(child.clone());
            }
            edges.push((parent.clone(), child.clone()));
        }
        tracing::trace!(
            nodes = nodes.len(),
            edges = edges.len(),
            "LineageGraph::from_pairs: ok"
        );
        Self { nodes, edges }
    }
    /// Total node count.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    /// Total edge count.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
    /// True if the graph has no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lineage_graph_from_pairs_deduplicates_nodes() {
        let g = LineageGraph::from_pairs(&[
            ("a".into(), "b".into()),
            ("b".into(), "c".into()),
            ("a".into(), "b".into()),
        ]);
        assert_eq!(g.nodes, vec!["a", "b", "c"]);
        assert_eq!(g.edges.len(), 3);
    }

    #[test]
    fn lineage_graph_empty_has_no_nodes() {
        let g = LineageGraph::empty();
        assert!(g.nodes.is_empty());
        assert!(g.edges.is_empty());
        assert!(g.is_empty());
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn lineage_graph_node_and_edge_count() {
        let g = LineageGraph::from_pairs(&[("p1".into(), "c1".into()), ("p1".into(), "c2".into())]);
        assert_eq!(g.node_count(), 3);
        assert_eq!(g.edge_count(), 2);
    }
}
