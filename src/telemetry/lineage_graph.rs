//! J#5: lineage graph view. Renders the parent/child run DAG
//! as a simple JSON adjacency list for the dashboard.

use serde::Serialize;
use std::collections::HashMap;

pub type RunId = String;

#[derive(Debug, Clone, Serialize)]
pub struct LineageGraph {
    pub nodes: Vec<RunId>,
    pub edges: Vec<(RunId, RunId)>,
}

impl LineageGraph {
    pub fn empty() -> Self {
        Self { nodes: Vec::new(), edges: Vec::new() }
    }
    pub fn from_pairs(pairs: &[(String, String)]) -> Self {
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
        Self { nodes, edges }
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
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
    fn lineage_graph_to_json_round_trip() {
        let g = LineageGraph::from_pairs(&[("x".into(), "y".into())]);
        let json = g.to_json();
        assert!(json.contains("\"x\""));
        assert!(json.contains("\"y\""));
    }

    #[test]
    fn lineage_graph_empty_has_no_nodes() {
        let g = LineageGraph::empty();
        assert!(g.nodes.is_empty());
        assert!(g.edges.is_empty());
    }
}
