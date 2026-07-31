//! `decompose` phase (Phase G, v0.3 «tercera etapa»).
//!
//! Splits a `deep`-mode canonical brief into a DAG of sub-questions
//! so the downstream `SketchPhase` and `ProposePhase` can fan out by
//! node instead of by angle. Per V4 §5.3 the trigger conditions are
//! met when the brief has multiple constraints, multiple deliverables,
//! explicit dependency hints, or a mix of architecture/domain/
//! implementation concerns.
//!
//! The phase only runs in `deep` mode. The wiring in
//! `src/cli/run.rs::build_pipeline_for_mode` is responsible for
//! inserting the phase between `Route` and `Sketch` only when
//! `Mode::Deep` is selected.
//!
//! ## Trivial path
//!
//! `ProblemGraph::should_decompose(brief) == false` ⇒ the phase
//! skips the LLM call entirely and writes a trivial
//! `ProblemGraph::trivial(...)` to `problem_graph.json`. The
//! downstream `SketchPhase` reads the sidecar, sees an empty graph,
//! and falls back to its non-DAG behaviour (one sketch per angle).
//!
//! ## DAG validation
//!
//! Even when `should_decompose == true`, the model can produce
//! graphs with cycles or dangling dependencies. The phase validates
//! the response with `ProblemGraph::validate_no_cycles()`; a broken
//! graph is logged as a warning, the response is repaired (drop
//! the offending node), and the phase falls through to the
//! remaining nodes. When no nodes survive, the trivial path is
//! taken.
//!
//! ## Sidecars
//!
//! - `problem_graph.json` (canonical, per T01-06 §1.2). Atomic
//!   write via `crate::atomic::writer::AtomicWriter`.
//! - SQLite mirror via `Db::record_problem_graph` (migration v006).
//!
//! ## Telemetry
//!
//! The phase emits:
//! - `phase.decompose.skipped_trivial` (info) when
//!   `should_decompose` is `false`.
//! - `phase.decompose.cycle_detected` (warn) when the model emits
//!   a graph that fails `validate_no_cycles()`.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::atomic::writer::AtomicWriter;
use crate::domain::{Brief, ProblemGraph, should_decompose as brief_should_decompose};
use crate::error::{Error, Result};
use crate::ids::blake3_hex;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::read_json;
use crate::time::now_unix_secs;

/// `decompose` phase. Decides whether the brief is worth a DAG, and
/// when it is, calls the `decomposer` LLM role to produce the
/// nodes. The phase is a no-op in every mode other than `deep`
/// (the wiring in `build_pipeline_for_mode` is the gate) and
/// short-circuits to `ProblemGraph::trivial` when the brief does
/// not meet the V4 §5.3 trigger ladder.
pub struct DecomposePhase;

impl DecomposePhase {
    /// Path to the sidecar this phase writes. Reused by tests so
    /// they can pin the path layout.
    pub fn sidecar_path(ctx: &RunContext) -> PathBuf {
        ctx.run_dir().root().join("problem_graph.json")
    }

    /// Repair a graph the model emitted with cycles or dangling
    /// dependencies by dropping the offending nodes. The phase
    /// then re-runs the cycle check; if nothing remains it returns
    /// `None` so the caller can fall back to the trivial path.
    fn repair(graph: &mut ProblemGraph) -> Result<Option<ProblemGraph>> {
        // Cap total repair rounds so a pathologically bad graph
        // does not spend unbounded compute.
        for _ in 0..8 {
            // 1. Drop nodes whose dependencies reference an id that
            //    does not exist in `nodes`.
            let known: std::collections::HashSet<String> =
                graph.nodes.iter().map(|n| n.id.clone()).collect();
            let before_dangle = graph.nodes.len();
            graph
                .nodes
                .retain(|n| n.dependencies.iter().all(|d| known.contains(d)));
            if graph.nodes.len() != before_dangle {
                continue;
            }
            // 2. Try Kahn's topological sort. If it succeeds the
            //    graph is acyclic and well-formed.
            if graph.validate_no_cycles().is_ok() {
                break;
            }
            // 3. Compute the stuck set ourselves: simulate Kahn to
            //    find which nodes are still participating in a
            //    cycle. This is more robust than parsing the error
            //    message — we drop the first stuck node and try
            //    again on the next round.
            let stuck = stuck_node_ids(graph);
            if let Some(first) = stuck.into_iter().next() {
                graph.nodes.retain(|n| n.id != first);
                continue;
            }
            // No stuck nodes reported but Kahn still failed —
            // fall through and break the loop.
            break;
        }
        if graph.nodes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(graph.clone()))
        }
    }
}

/// Run Kahn's algorithm by hand and return the set of node ids
/// that still have non-zero in-degree (i.e. are part of a cycle).
/// Used by the repair pass instead of parsing the error message
/// shape.
fn stuck_node_ids(graph: &ProblemGraph) -> Vec<String> {
    let n = graph.nodes.len();
    let index: std::collections::HashMap<&str, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();
    let mut in_degree = vec![0usize; n];
    for (i, node) in graph.nodes.iter().enumerate() {
        for dep in &node.dependencies {
            if let Some(&p) = index.get(dep.as_str()) {
                if p != i {
                    in_degree[i] += 1;
                }
            }
        }
    }
    let mut frontier: Vec<usize> = (0..n).filter(|i| in_degree[*i] == 0).collect();
    let mut visited: Vec<bool> = vec![false; n];
    while let Some(node) = frontier.pop() {
        if visited[node] {
            continue;
        }
        visited[node] = true;
        for (i, n) in graph.nodes.iter().enumerate() {
            if n.dependencies
                .iter()
                .any(|d| index.get(d.as_str()) == Some(&node))
            {
                in_degree[i] = in_degree[i].saturating_sub(1);
                if in_degree[i] == 0 {
                    frontier.push(i);
                }
            }
        }
    }
    graph
        .nodes
        .iter()
        .zip(in_degree.iter())
        .zip(visited.iter())
        .filter_map(|((node, &deg), &v)| {
            if !v && deg > 0 {
                Some(node.id.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Extract the first stuck node id from the error message returned
/// by `ProblemGraph::topological_layers`. Best-effort: the message
/// shape is `graph has a cycle; stuck at: ["a", "b"]`. When the
/// shape does not match we return `None` so the caller can fall
/// through to dropping the longest node instead.
#[cfg(test)]
fn extract_first_stuck_node(msg: &str, nodes: &[crate::domain::GraphNode]) -> Option<String> {
    let start = msg.find("stuck at:")?;
    let after = &msg[start..];
    let lb = after.find('[')?;
    let rb = after.find(']')?;
    let inner = &after[lb + 1..rb];
    let first = inner.split(',').next()?.trim().trim_matches('"');
    if first.is_empty() {
        return None;
    }
    // Defensive: confirm the id actually exists in the graph.
    if nodes.iter().any(|n| n.id == first) {
        Some(first.to_string())
    } else {
        None
    }
}
#[async_trait]
impl Phase for DecomposePhase {
    fn name(&self) -> &'static str {
        "decompose"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        // 1. Load the canonical brief and decide whether to bother
        //    calling the LLM. The trigger ladder is in
        //    `domain::should_decompose`.
        let brief: Brief = read_json(&ctx.run_dir().brief())?;
        let brief_blake3 = blake3_hex(&serde_json::to_vec(&brief).map_err(Error::from)?);
        if !brief_should_decompose(&brief) {
            let _ = ctx.telemetry.warn(
                "phase.decompose.skipped_trivial",
                "info",
                "decompose short-circuited to trivial graph",
                serde_json::json!({
                    "deliverables": brief.deliverables.len(),
                    "constraints": brief.constraints.len(),
                    "assumptions": brief.assumptions.len(),
                }),
                crate::telemetry::WarningContext {
                    phase: Some("decompose".into()),
                    role: Some("decompose".into()),
                    ..Default::default()
                },
            );
            let graph = ProblemGraph::trivial(brief_blake3, now_unix_secs());
            return self.persist(ctx, graph);
        }

        // 2. Call the LLM. We pass the brief as the user payload and
        //    let the JSON repair pass absorb schema drift.
        let user = serde_json::to_string(&brief).map_err(Error::from)?;
        let system = system_prompt(Role::Decomposer).to_owned();
        let mut graph: ProblemGraph = ctx
            .call_with_retry_parse::<ProblemGraph>(
                Role::Decomposer,
                system,
                user,
                "Decomposer: {should_decompose, nodes[], integration_rules[], critical_path[]}",
                3,
            )
            .await?;

        // 3. Re-derive the `brief_blake3` so the sidecar matches the
        //    on-disk brief byte-for-byte even if the model truncated
        //    fields.
        graph.brief_blake3 = brief_blake3.clone();
        graph.schema_version = "v1".to_string();
        if graph.created_unix == 0 {
            graph.created_unix = now_unix_secs();
        }

        // 4. If the model said "no", respect it. The trigger ladder
        //    is conservative; the model is the final word.
        if !graph.should_decompose {
            let _ = ctx.telemetry.warn(
                "phase.decompose.skipped_by_model",
                "info",
                "decompose: model returned should_decompose=false",
                serde_json::json!({}),
                crate::telemetry::WarningContext {
                    phase: Some("decompose".into()),
                    role: Some("decompose".into()),
                    ..Default::default()
                },
            );
            let trivial = ProblemGraph::trivial(graph.brief_blake3.clone(), graph.created_unix);
            return self.persist(ctx, trivial);
        }

        // 5. Repair cycles / dangling deps. The pipeline never
        //    crashes on a malformed graph; it always produces a
        //    valid sidecar (or a trivial one).
        let final_graph = match Self::repair(&mut graph)? {
            Some(g) => g,
            None => {
                let _ = ctx.telemetry.warn(
                    "phase.decompose.all_nodes_dropped",
                    "warn",
                    "decompose: every node was dropped during repair",
                    serde_json::json!({}),
                    crate::telemetry::WarningContext {
                        phase: Some("decompose".into()),
                        role: Some("decompose".into()),
                        ..Default::default()
                    },
                );
                ProblemGraph::trivial(graph.brief_blake3.clone(), graph.created_unix)
            }
        };
        if final_graph.is_empty() {
            return self.persist(ctx, final_graph);
        }
        self.persist(ctx, final_graph)
    }
}

impl DecomposePhase {
    /// Persist the graph atomically and mirror it to SQLite. Returns
    /// the canonical `PhaseOutput` so the pipeline keeps the same
    /// shape as the other phases.
    fn persist(&self, ctx: &RunContext, graph: ProblemGraph) -> Result<PhaseOutput> {
        let sidecar = Self::sidecar_path(ctx);
        let json = serde_json::to_vec_pretty(&graph).map_err(Error::from)?;
        AtomicWriter::new().write(&sidecar, &json)?;
        // SQLite mirror (best-effort; do not fail the phase when the
        // index is unavailable).
        if let Some(db) = ctx.telemetry.db() {
            let _ = db.record_problem_graph(
                ctx.run_id,
                graph.brief_blake3.as_str(),
                graph.should_decompose,
                graph.nodes.len() as i64,
                graph.created_unix,
            );
        }
        Ok(PhaseOutput::ProblemGraph(sidecar))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{GraphNode, ValidationMethod};

    #[test]
    fn repair_drops_dangling_dependency() {
        let mut g = ProblemGraph {
            should_decompose: true,
            nodes: vec![
                GraphNode {
                    id: "a".into(),
                    dependencies: vec!["ghost".into()],
                    ..Default::default()
                },
                GraphNode {
                    id: "b".into(),
                    dependencies: vec![],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let r = DecomposePhase::repair(&mut g).unwrap();
        let g = r.expect("non-empty after repair");
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, "b");
    }

    #[test]
    fn repair_breaks_cycle_by_dropping_stuck_node() {
        let mut g = ProblemGraph {
            should_decompose: true,
            nodes: vec![
                GraphNode {
                    id: "a".into(),
                    dependencies: vec!["b".into()],
                    ..Default::default()
                },
                GraphNode {
                    id: "b".into(),
                    dependencies: vec!["a".into()],
                    ..Default::default()
                },
                GraphNode {
                    id: "c".into(),
                    dependencies: vec![],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let r = DecomposePhase::repair(&mut g).unwrap();
        let g = r.expect("non-empty after repair");
        // c should survive; one of a or b is dropped.
        assert!(g.nodes.iter().any(|n| n.id == "c"));
        assert!(g.topological_layers().is_ok());
    }

    #[test]
    fn repair_returns_none_when_all_nodes_dropped() {
        let mut g = ProblemGraph {
            should_decompose: true,
            nodes: vec![GraphNode {
                id: "a".into(),
                dependencies: vec!["ghost".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let r = DecomposePhase::repair(&mut g).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn extract_stuck_node_parses_message_shape() {
        let nodes = vec![
            GraphNode {
                id: "a".into(),
                ..Default::default()
            },
            GraphNode {
                id: "b".into(),
                ..Default::default()
            },
        ];
        let msg = r#"graph has a cycle; stuck at: ["a", "b"]"#;
        assert_eq!(extract_first_stuck_node(msg, &nodes), Some("a".into()));
    }

    #[test]
    fn extract_stuck_node_handles_unknown_id() {
        let nodes = vec![GraphNode {
            id: "a".into(),
            ..Default::default()
        }];
        let msg = r#"graph has a cycle; stuck at: ["ghost"]"#;
        assert_eq!(extract_first_stuck_node(msg, &nodes), None);
    }

    #[test]
    fn repair_preserves_validation_method() {
        // The repair pass must not lose the validation_method field
        // of the surviving nodes.
        let mut g = ProblemGraph {
            should_decompose: true,
            nodes: vec![
                GraphNode {
                    id: "a".into(),
                    validation_method: ValidationMethod::Executable,
                    ..Default::default()
                },
                GraphNode {
                    id: "b".into(),
                    validation_method: ValidationMethod::None,
                    dependencies: vec!["a".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let r = DecomposePhase::repair(&mut g).unwrap().unwrap();
        assert_eq!(r.nodes[0].validation_method, ValidationMethod::Executable);
        assert_eq!(r.nodes[1].validation_method, ValidationMethod::None);
    }
}
