//! Propose phase. Reads the brief, asks the model for N proposals in
//! parallel, writes `proposals/p_001.json`, `proposals/p_002.json`, …
//!
//! When `SketchPhase` ran earlier in the same run, `ProposePhase`
//! reads the surviving sketches from `sketches/` and pairs the i-th
//! proposal with the i-th sketch. The pairing is best-effort: if there
//! are more proposals than sketches the extras get an empty
//! `source_sketch`; if there are fewer proposals than sketches the
//! trailing sketches are unused. The intent is **lineage**, not
//! selection — the proposal still stands on its own merits and the
//! gate/judge phases do not look at `source_sketch`.
//!
//! When `DecomposePhase` ran earlier in the same run (only `Mode::Deep`)
//! and the resulting `problem_graph.json` is non-trivial, the phase
//! populates `Proposal.source_nodes` with the ids of the graph nodes
//! whose text fingerprint is closest to the proposal text. This is the
//! Phase G limitation #2 follow-up (see `docs/v0.3-status.md`).

use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;

use crate::domain::{ProblemGraph, Proposal, Sketch};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::ranking::cluster::jaccard_distance;

/// Jaccard distance threshold for assigning a proposal to a DAG node.
/// `0.7` is consistent with the `CLUSTER_THRESHOLD` in `RankPhase`: two
/// texts with 70%+ shared vocabulary are considered to address the same
/// sub-problem. Lowering this widens the assignment.
const SOURCE_NODE_THRESHOLD: f32 = 0.7;

/// Propose phase. Generates `count` proposals concurrently, bounded
/// by `RunContext::parallelism` (default 4). The wall-clock cost of
/// this phase is `ceil(count / max_parallelism) * (model_latency)`,
/// not `count * (model_latency)`.
pub struct ProposePhase {
    /// Number of proposals to generate.
    pub count: u32,
}

impl ProposePhase {
    /// Read every `sketches/sk_*.json` and return their ids in
    /// alphabetical order (which matches the persistence order).
    /// Returns an empty vector when the directory is missing or
    /// `fast` mode skipped the sketch phase; the caller treats
    /// either case as "no source sketch".
    fn load_sketch_ids(ctx: &RunContext) -> Vec<String> {
        let dir = ctx.run_dir().sketches();
        let entries = match std::fs::read_dir(&dir) {
            Ok(it) => it,
            Err(_) => return Vec::new(),
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .filter(|p| {
                // sidecar .meta.json files are persisted next to each
                // artefact — keep only the primary .json files.
                p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| !s.ends_with(".meta.json"))
                    .unwrap_or(true)
            })
            .collect();
        paths.sort();
        paths
            .into_iter()
            .filter_map(|p| {
                let raw = std::fs::read_to_string(&p).ok()?;
                let sk: Sketch = serde_json::from_str(&raw).ok()?;
                if sk.id.is_empty() { None } else { Some(sk.id) }
            })
            .collect()
    }

    /// Read `problem_graph.json` from the run dir. Returns `None` when
    /// the sidecar is missing or the graph is trivial (Phase G
    /// limitation #1). This is the only consumer of `problem_graph.json`
    /// outside `DecomposePhase` / `SketchPhase` / the deliver surface.
    fn load_problem_graph(ctx: &RunContext) -> Option<ProblemGraph> {
        let path = ctx.run_dir().problem_graph();
        if !path.exists() {
            return None;
        }
        let raw = std::fs::read_to_string(&path).ok()?;
        let g: ProblemGraph = serde_json::from_str(&raw).ok()?;
        if g.should_decompose && !g.nodes.is_empty() {
            Some(g)
        } else {
            None
        }
    }

    /// Given a non-trivial graph and a freshly emitted proposal,
    /// compute the ids of the nodes that the proposal addresses. The
    /// assignment is based on Jaccard distance between the proposal's
    /// textual fingerprint (`summary + approach + tradeoffs + evidence`)
    /// and each node's fingerprint (`id + question + expected_output`).
    ///
    /// Returns an empty `Vec` when the graph has no nodes or when no
    /// node passes the threshold. The result is sorted by (distance,
    /// id) so the persisted JSON is stable across runs (idempotent
    /// writes).
    fn compute_source_nodes(graph: &ProblemGraph, proposal: &Proposal) -> Vec<String> {
        let prop_text = format!(
            "{} {} {} {}",
            proposal.summary,
            proposal.approach,
            proposal.tradeoffs.join(" "),
            proposal.evidence.join(" ")
        );
        let mut matched: Vec<(String, f32)> = graph
            .nodes
            .iter()
            .filter_map(|n| {
                let node_text = format!("{} {} {}", n.id, n.question, n.expected_output);
                let d = jaccard_distance(&prop_text, &node_text);
                if d <= SOURCE_NODE_THRESHOLD {
                    Some((n.id.clone(), d))
                } else {
                    None
                }
            })
            .collect();
        // Sort by (distance, id) so ties are deterministic and the
        // closest node wins when many tie.
        matched.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        matched.into_iter().map(|(id, _)| id).collect()
    }
}

#[async_trait]
impl Phase for ProposePhase {
    fn name(&self) -> &'static str {
        "propose"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        tracing::debug!(count = self.count, "propose: enter");
        let brief: serde_json::Value = read_json(&ctx.run_dir().brief())?;
        let user = serde_json::to_string(&brief).map_err(crate::Error::from)?;
        let system = system_prompt(Role::Propose).to_owned();
        let proposals_dir = ctx.run_dir().proposals();
        std::fs::create_dir_all(&proposals_dir)?;

        let count = self.count as usize;
        let sketch_ids = Self::load_sketch_ids(ctx);
        tracing::debug!(
            sketch_id_count = sketch_ids.len(),
            "propose: loaded sketch ids"
        );
        let problem_graph = Self::load_problem_graph(ctx);
        if let Some(g) = problem_graph.as_ref() {
            tracing::debug!(
                node_count = g.nodes.len(),
                should_decompose = g.should_decompose,
                "propose: problem graph available for source_nodes"
            );
        }
        let system_arc = std::sync::Arc::new(system);
        let user_arc = std::sync::Arc::new(user);

        let futures = (0..count).map(|i| {
            let id = format!("p_{i:03}");
            let user_with_id = format!("{}\n\nUse id=\"{id}\" in the output.", user_arc.as_str());
            let ctx = ctx.clone();
            let system_arc = std::sync::Arc::clone(&system_arc);
            let id_for_default = id.clone();
            let source_sketch = sketch_ids.get(i).cloned().unwrap_or_default();
            let graph_for_thread = problem_graph.clone();
            async move {
                let _permit = ctx.parallelism.acquire().await?;
                let mut proposal: Proposal = ctx
                    .call_with_retry_parse(
                        Role::Propose,
                        system_arc.as_str().to_owned(),
                        user_with_id,
                        "Proposal: {id, summary, approach, tradeoffs[], evidence[], artifacts[]{kind,language,source}}",
                        5,
                    )
                    .await?;
                tracing::trace!(
                    proposal_id = %id_for_default,
                    "propose: parsed proposal"
                );
                // PR-flake: when the model returns a parseable Proposal
                // whose `summary` or `approach` are empty (typically
                // because the upstream mock served the wrong fixture
                // for this role), surface a structured warning so the
                // cluster phase can see why the run produced N-1
                // proposals instead of N. We do NOT short-circuit:
                // `join_all` must keep all `count` slots, otherwise
                // the cluster phase would see a different cardinality
                // and synthesize against an incomplete set.
                if proposal.summary.trim().is_empty() || proposal.approach.trim().is_empty() {
                    let _ = ctx.telemetry.warn(
                        "phase.propose_dropped_empty",
                        "warn",
                        "propose parsed but produced empty summary or approach",
                        serde_json::json!({
                            "proposal_id_for_default": id_for_default,
                            "summary_len": proposal.summary.len(),
                            "approach_len": proposal.approach.len(),
                        }),
                        crate::telemetry::WarningContext {
                            phase: Some("propose".into()),
                            role: Some("propose".into()),
                            ..Default::default()
                        },
                    );
                }
                // Force `id` to the slot id. The on-disk file stem is
                // `<id>.json` (see below), and the canonical id used
                // by every downstream phase (`ClusterProposalsPhase`,
                // `CritiquePhase`, `JudgePhase`, `GatePhase`, …) is
                // that stem. The model's reported id is informational
                // only: when per-role mock fixtures cycle (e.g. 3
                // propose fixtures cycling for 7 propose calls) the
                // LLM-reported id collides across slots, which then
                // collapses multiple distinct proposals onto the same
                // critique / gate / evaluation files. Forcing the
                // slot id here keeps the on-disk name, the JSON
                // payload, and the downstream file keys consistent.
                proposal.id = id_for_default;
                proposal.source_sketch = source_sketch;
                // Phase H commit 3: populate Proposal.source_nodes from
                // the problem graph when it is non-trivial. Done here
                // (post-parse) rather than inside the proposal to keep
                // the model's contract unchanged.
                if let Some(graph) = graph_for_thread.as_ref() {
                    proposal.source_nodes = Self::compute_source_nodes(graph, &proposal);
                }
                Ok::<(String, Proposal), crate::error::Error>((id, proposal))
            }
        });

        let results = join_all(futures).await;
        let mut paths = Vec::with_capacity(count);
        let mut empty_warns = 0usize;
        for r in results {
            let (id, proposal) = match r {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = %e, "propose: future failed");
                    return Err(e);
                }
            };
            let path: PathBuf = proposals_dir.join(format!("{id}.json"));
            write_json(&path, &proposal)?;
            paths.push(path);
            if proposal.summary.trim().is_empty() || proposal.approach.trim().is_empty() {
                empty_warns += 1;
            }
        }
        tracing::info!(
            proposals_written = paths.len(),
            empty_proposals = empty_warns,
            "propose: phase complete"
        );
        Ok(PhaseOutput::Proposals(paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::GraphNode;

    fn fixture_graph() -> ProblemGraph {
        ProblemGraph {
            schema_version: "v1".into(),
            should_decompose: true,
            nodes: vec![
                GraphNode {
                    id: "n0".into(),
                    question: "design the data model for the rainbow".into(),
                    expected_output: "schema".into(),
                    constraints: vec![],
                    dependencies: vec![],
                    validation_method: Default::default(),
                },
                GraphNode {
                    id: "n1".into(),
                    question: "implement the rendering pipeline".into(),
                    expected_output: "code".into(),
                    constraints: vec![],
                    dependencies: vec!["n0".into()],
                    validation_method: Default::default(),
                },
                GraphNode {
                    id: "n2".into(),
                    question: "completely unrelated topic about cheese".into(),
                    expected_output: "essay".into(),
                    constraints: vec![],
                    dependencies: vec![],
                    validation_method: Default::default(),
                },
            ],
            integration_rules: vec![],
            critical_path: vec![],
            brief_blake3: "deadbeef".into(),
            created_unix: 0,
        }
    }

    fn fixture_proposal(text: &str) -> Proposal {
        Proposal {
            id: "p_test".into(),
            summary: text.into(),
            approach: text.into(),
            tradeoffs: vec![],
            evidence: vec![text.into()],
            artifacts: vec![],
            source_sketch: String::new(),
            source_nodes: vec![],
            replaced_by: None,
        }
    }

    #[test]
    fn source_nodes_populated_when_text_matches_node() {
        let graph = fixture_graph();
        let proposal =
            fixture_proposal("the rainbow rendering pipeline data model schema is canonical");
        let nodes = ProposePhase::compute_source_nodes(&graph, &proposal);
        // n0 (data model + rainbow) matches strongly. n1 (rendering
        // pipeline) shares 3 of 10 unique words → distance 0.7 which
        // is right on the threshold boundary; the test focuses on
        // the safe-assertion path (n0 must be present, n2 must not).
        assert!(nodes.contains(&"n0".to_string()), "got {nodes:?}");
        assert!(!nodes.contains(&"n2".to_string()), "got {nodes:?}");
    }

    #[test]
    fn source_nodes_empty_when_no_match() {
        let graph = fixture_graph();
        let proposal = fixture_proposal("quantum entanglement experiment");
        let nodes = ProposePhase::compute_source_nodes(&graph, &proposal);
        assert!(nodes.is_empty(), "got {nodes:?}");
    }

    #[test]
    fn source_nodes_picks_up_high_overlap_nodes() {
        // Craft a proposal that strongly matches both n0 and n1.
        let graph = ProblemGraph {
            schema_version: "v1".into(),
            should_decompose: true,
            nodes: vec![
                GraphNode {
                    id: "alpha".into(),
                    question: "render the rainbow with the rendering pipeline".into(),
                    expected_output: "code".into(),
                    constraints: vec![],
                    dependencies: vec![],
                    validation_method: Default::default(),
                },
                GraphNode {
                    id: "beta".into(),
                    question: "render the rainbow with the rendering pipeline".into(),
                    expected_output: "code".into(),
                    constraints: vec![],
                    dependencies: vec![],
                    validation_method: Default::default(),
                },
                GraphNode {
                    id: "gamma".into(),
                    question: "totally different topic about cheese".into(),
                    expected_output: "essay".into(),
                    constraints: vec![],
                    dependencies: vec![],
                    validation_method: Default::default(),
                },
            ],
            integration_rules: vec![],
            critical_path: vec![],
            brief_blake3: "deadbeef".into(),
            created_unix: 0,
        };
        let proposal =
            fixture_proposal("we render the rainbow with the rendering pipeline end-to-end");
        let nodes = ProposePhase::compute_source_nodes(&graph, &proposal);
        // alpha and beta should both match; gamma should not.
        assert!(nodes.contains(&"alpha".to_string()), "got {nodes:?}");
        assert!(nodes.contains(&"beta".to_string()), "got {nodes:?}");
        assert!(!nodes.contains(&"gamma".to_string()), "got {nodes:?}");
        // Stable order: tied distance, alpha < beta lexicographically.
        let pos_a = nodes.iter().position(|n| n == "alpha").unwrap();
        let pos_b = nodes.iter().position(|n| n == "beta").unwrap();
        assert!(pos_a < pos_b, "got {nodes:?}");
    }
}
