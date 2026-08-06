//! D.22.3: invalidate_downstream with DAG traversal.
//!
//! When a refinement invalidates a node, mark all downstream
//! nodes (in `ProblemGraph.topological_layers()` order) as stale
//! so they re-compute on the next phase.

use crate::domain::graph::ProblemGraph;
use std::collections::HashSet;

/// Return the root and every node reachable through downstream edges.
pub fn invalidate_downstream(graph: &ProblemGraph, root_node_id: &str) -> HashSet<String> {
    let mut invalidated = HashSet::new();
    invalidated.insert(root_node_id.to_string());
    let adj = graph.adjacency();
    let mut frontier = vec![root_node_id.to_string()];
    while let Some(node) = frontier.pop() {
        if let Some(children) = adj.get(&node) {
            for child in children {
                if invalidated.insert(child.clone()) {
                    frontier.push(child.clone());
                }
            }
        }
    }
    invalidated
}

#[cfg(test)]
mod tests {
    use super::invalidate_downstream;
    use crate::domain::graph::{GraphNode, ProblemGraph, ValidationMethod};
    fn graph(nodes: Vec<GraphNode>) -> ProblemGraph {
        ProblemGraph {
            schema_version: "v1".to_string(),
            should_decompose: true,
            nodes,
            ..ProblemGraph::default()
        }
    }
    fn node(id: &str, dependencies: &[&str]) -> GraphNode {
        GraphNode {
            id: id.to_string(),
            dependencies: dependencies.iter().map(|id| (*id).to_string()).collect(),
            validation_method: ValidationMethod::None,
            ..GraphNode::default()
        }
    }
    #[test]
    fn invalidate_downstream_marks_root_and_children() {
        let affected = invalidate_downstream(
            &graph(vec![
                node("root", &[]),
                node("child", &["root"]),
                node("grandchild", &["child"]),
                node("unrelated", &[]),
            ]),
            "root",
        );
        assert_eq!(affected.len(), 3);
        assert!(affected.contains("root"));
        assert!(affected.contains("child"));
        assert!(affected.contains("grandchild"));
        assert!(!affected.contains("unrelated"));
    }
    #[test]
    fn invalidate_downstream_handles_dag_cycle() {
        let affected = invalidate_downstream(
            &graph(vec![node("root", &["child"]), node("child", &["root"])]),
            "root",
        );
        assert_eq!(affected.len(), 2);
    }
}
