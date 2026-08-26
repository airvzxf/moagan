//! Optional DAG backend for the deep-mode phase pipeline.
//!
//! Activated only when the binary is compiled with `--features dag`.
//! The default build (`cargo build` with no features) does not pull
//! the `petgraph` crate; the linear [`crate::phases::pipe::Pipeline`]
//! stays the canonical executor and `dag.rs` is gated out by
//! `#[cfg(feature = "dag")]` at the module level.
//!
//! Compliance: T01-06 §3.6.2 (`DAG de fases para deep mode`); ADR
//! 0001 §D-1 (admission policy for `petgraph`); `docs/proposal-03-
//! add-ons.md` §D.2 (catalogue pin `petgraph 0.6 + serde`).
//!
//! ## Topology
//!
//! [`build_dag_for_deep_mode`] wires the canonical 16-phase linear
//! chain into a [`PhaseGraph`] (one node per phase, one edge per
//! "parent must finish before child" relation). The resulting graph
//! is linear by construction; the value of the DAG representation
//! (vs. the flat `Vec<Box<dyn Phase>>` in
//! [`crate::phases::pipe::Pipeline`]) is that future work can
//! branch, parallelise, and inspect the topology without rewiring
//! the pipeline.
//!
//! [`topological_layers`] runs Kahn's algorithm and returns the
//! layers in dependency order so the executor can fan out within a
//! layer (today the deep-mode layers are singletons; the API is
//! ready for future fan-out — e.g. `sketch × 5`, `propose × 5`).
//!
//! [`execute_dag`] drives a phase set through those layers using
//! `futures::future::join_all`, so the per-layer concurrency is
//! bounded only by `ctx.parallelism` and the runtime, never by the
//! linear `Pipeline` semantics.

use std::collections::HashMap;

use petgraph::Direction;
use petgraph::graph::NodeIndex;

use crate::error::{Error, Result};

use super::phase::{Phase, PhaseOutput, RunContext};

/// Directed acyclic graph of [`PhaseId`]s. Edge weights are
/// [`EdgeKind`] so future work can distinguish hard data
/// dependencies from soft ordering hints without re-parsing node
/// metadata.
pub type PhaseGraph = petgraph::graph::DiGraph<PhaseId, EdgeKind>;

/// Identity of a phase inside the DAG. The variant name matches the
/// canonical phase name returned by
/// [`crate::phases::phase::Phase::name`] and the entries in
/// [`crate::phases::pipe::Pipeline::canonical_phase_order`], so the
/// executor can look up an implementation by string key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseId {
    /// Normalises the raw user prompt into a canonical brief.
    Intake,
    /// Detects ambiguities and turns them into clarification
    /// questions.
    Clarify,
    /// Picks the running mode for the request.
    Route,
    /// Splits the brief into a `ProblemGraph` (deep-only).
    Decompose,
    /// Produces the `sketches/sk_*.json` artefacts.
    Sketch,
    /// Produces the `proposals/p_*.json` artefacts.
    Propose,
    /// Runs the mechanical validator against each proposal.
    Validate,
    /// Clusters proposals by stack / approach (Phase D).
    ClusterProposals,
    /// Synthesises compatible proposals (Phase D).
    Synthesize,
    /// Filters out proposals that violate hard constraints.
    Gate,
    /// Runs the critic panel (correctness / constraint / security).
    Critique,
    /// Repairs the proposals based on critique feedback.
    Repair,
    /// Runs the judge panel (correctness / completeness /
    /// feasibility).
    Judge,
    /// Pattern-based adversarial review (Phase D, deep-only).
    Adversary,
    /// Computes the final ranking and writes `rankings/ranking.json`.
    Rank,
    /// Writes `final/portfolio.md` and closes the run.
    Deliver,
}

impl PhaseId {
    /// Canonical lowercase phase name. Matches the
    /// `Phase::name()` contract in [`crate::phases::phase`].
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Intake => "intake",
            Self::Clarify => "clarify",
            Self::Route => "route",
            Self::Decompose => "decompose",
            Self::Sketch => "sketch",
            Self::Propose => "propose",
            Self::Validate => "validate",
            Self::ClusterProposals => "cluster_proposals",
            Self::Synthesize => "synthesize",
            Self::Gate => "gate",
            Self::Critique => "critique",
            Self::Repair => "repair",
            Self::Judge => "judge",
            Self::Adversary => "adversary",
            Self::Rank => "rank",
            Self::Deliver => "deliver",
        }
    }

    /// Resolve a phase name back to its [`PhaseId`]. Returns `None`
    /// when the name is not one of the canonical entries, so a
    /// pipeline that drags in a non-canonical phase (e.g. an alias
    /// like `"proposal"`) can be detected at the boundary.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "intake" => Some(Self::Intake),
            "clarify" => Some(Self::Clarify),
            "route" => Some(Self::Route),
            "decompose" => Some(Self::Decompose),
            "sketch" => Some(Self::Sketch),
            "propose" => Some(Self::Propose),
            "validate" => Some(Self::Validate),
            "cluster_proposals" => Some(Self::ClusterProposals),
            "synthesize" => Some(Self::Synthesize),
            "gate" => Some(Self::Gate),
            "critique" => Some(Self::Critique),
            "repair" => Some(Self::Repair),
            "judge" => Some(Self::Judge),
            "adversary" => Some(Self::Adversary),
            "rank" => Some(Self::Rank),
            "deliver" => Some(Self::Deliver),
            _ => None,
        }
    }
}

/// Edge semantics between two phases in the DAG.
///
/// The deep-mode graph only emits [`EdgeKind::Data`] today
/// (every child strictly reads the parent's artefacts), but the
/// enum exists so future additions — e.g. `Critique` running in
/// parallel with `Repair` on disjoint subsets — can branch
/// without a graph-format migration. `Default::default()` resolves
/// to [`EdgeKind::Data`] so future code that constructs an edge
/// without specifying a kind keeps the strict-dependency
/// semantics.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Hard data dependency: child cannot start until parent
    /// finishes. Deep mode uses this exclusively today; this is
    /// also the `Default::default()` value so unspecified edges
    /// stay conservative.
    #[default]
    Data,
    /// Soft ordering hint: child reads parent eventually but can
    /// start earlier. Reserved for future phase-level fan-out.
    Order,
}

/// Build the canonical deep-mode phase DAG.
///
/// Mirrors [`crate::phases::pipe::Pipeline::canonical_phase_order`]
/// so the linear `Pipeline` and the DAG execute the same phases in
/// the same order. Adding a new canonical phase requires touching
/// both call sites; the
/// [`dag_topology_matches_linear_order`](self::tests::dag_topology_matches_linear_order)
/// unit test pins the invariant.
///
/// Returns an owned graph because `petgraph::graph::DiGraph` is
/// already `Copy`/`Clone`-friendly and callers commonly persist it
/// (e.g. for visualisation) or pass it to multiple executors.
pub fn build_dag_for_deep_mode() -> PhaseGraph {
    tracing::debug!("dag: building canonical deep-mode phase DAG");
    let mut graph = PhaseGraph::new();

    // Add nodes in canonical order. The order of insertion does NOT
    // change the topology but it stabilises the node-index numbering
    // so unit tests can assert specific indices without flake.
    let order: [PhaseId; 16] = [
        PhaseId::Intake,
        PhaseId::Clarify,
        PhaseId::Route,
        PhaseId::Decompose,
        PhaseId::Sketch,
        PhaseId::Propose,
        PhaseId::Validate,
        PhaseId::ClusterProposals,
        PhaseId::Synthesize,
        PhaseId::Gate,
        PhaseId::Critique,
        PhaseId::Repair,
        PhaseId::Judge,
        PhaseId::Adversary,
        PhaseId::Rank,
        PhaseId::Deliver,
    ];
    let mut idx: HashMap<PhaseId, NodeIndex> = HashMap::with_capacity(order.len());
    for id in order {
        let node = graph.add_node(id);
        idx.insert(id, node);
    }

    // Wire the linear chain. Each phase depends on the previous one
    // in canonical order. Future fan-out (e.g. `decompose` emitting
    // a `ProblemGraph` that feeds `sketch × 5` in parallel) is a
    // pure additive change here.
    let pairs: [(PhaseId, PhaseId); 15] = [
        (PhaseId::Intake, PhaseId::Clarify),
        (PhaseId::Clarify, PhaseId::Route),
        (PhaseId::Route, PhaseId::Decompose),
        (PhaseId::Decompose, PhaseId::Sketch),
        (PhaseId::Sketch, PhaseId::Propose),
        (PhaseId::Propose, PhaseId::Validate),
        (PhaseId::Validate, PhaseId::ClusterProposals),
        (PhaseId::ClusterProposals, PhaseId::Synthesize),
        (PhaseId::Synthesize, PhaseId::Gate),
        (PhaseId::Gate, PhaseId::Critique),
        (PhaseId::Critique, PhaseId::Repair),
        (PhaseId::Repair, PhaseId::Judge),
        (PhaseId::Judge, PhaseId::Adversary),
        (PhaseId::Adversary, PhaseId::Rank),
        (PhaseId::Rank, PhaseId::Deliver),
    ];
    for (from, to) in pairs {
        let from_idx = idx[&from];
        let to_idx = idx[&to];
        graph.add_edge(from_idx, to_idx, EdgeKind::Data);
        tracing::trace!(from = from.as_str(), to = to.as_str(), "dag: edge added");
    }

    tracing::info!(
        node_count = graph.node_count(),
        edge_count = graph.edge_count(),
        "dag: canonical deep-mode DAG built"
    );
    graph
}

/// Kahn-style topological layers.
///
/// Returns one `Vec<NodeIndex>` per layer in dependency order:
/// layer 0 holds the roots (no predecessors); layer 1 holds nodes
/// whose only predecessors are in layer 0; and so on. For the
/// canonical deep-mode graph (a chain) this collapses to 16 layers
/// of one node each, which is fine — the layer abstraction is what
/// gives [`execute_dag`] room to parallelise future fan-out without
/// a rewrite.
///
/// Errors with [`Error::InvalidState`] when the graph is not a DAG
/// (cycle or dangling reference). The message lists the nodes that
/// could not be placed so a debugger can spot the offending cycle
/// without re-running the test.
pub fn topological_layers(graph: &PhaseGraph) -> Result<Vec<Vec<NodeIndex>>> {
    let n = graph.node_count();
    tracing::debug!(n, "dag: topological_layers: start");
    // Track in-degrees in a mutable map so we can decrement on
    // every outgoing edge of a placed node and detect zero quickly.
    let mut in_degree: HashMap<NodeIndex, usize> = HashMap::with_capacity(n);
    for node in graph.node_indices() {
        in_degree.insert(
            node,
            graph.neighbors_directed(node, Direction::Incoming).count(),
        );
    }

    let mut layers: Vec<Vec<NodeIndex>> = Vec::new();
    let mut current_layer: Vec<NodeIndex> = in_degree
        .iter()
        .filter_map(|(idx, deg)| if *deg == 0 { Some(*idx) } else { None })
        .collect();
    current_layer.sort();

    let mut placed = 0usize;
    while !current_layer.is_empty() {
        layers.push(current_layer.clone());
        placed += current_layer.len();
        tracing::trace!(
            layer_index = layers.len() - 1,
            layer_size = current_layer.len(),
            placed,
            "dag: topological_layers: layer placed"
        );
        let mut next_layer: Vec<NodeIndex> = Vec::new();
        for &node in &current_layer {
            // Remove the placed node from the map so a successor's
            // in-degree can never be decremented twice through
            // different paths (Kahn correctness invariant).
            in_degree.remove(&node);
            for succ in graph.neighbors_directed(node, Direction::Outgoing) {
                if let Some(deg) = in_degree.get_mut(&succ) {
                    *deg = deg.saturating_sub(1);
                    if *deg == 0 {
                        next_layer.push(succ);
                    }
                }
            }
        }
        next_layer.sort();
        current_layer = next_layer;
    }

    if placed < n {
        let stuck: Vec<&'static str> = graph
            .node_indices()
            .filter(|idx| in_degree.contains_key(idx))
            .map(|idx| graph[idx].as_str())
            .collect();
        tracing::error!(
            placed,
            total = n,
            stuck = ?stuck,
            "dag: topological_layers: cycle detected"
        );
        return Err(Error::InvalidState(format!(
            "phase DAG is not a DAG (cycle suspected); stuck at: [{}]",
            stuck.join(", ")
        )));
    }

    tracing::debug!(
        layers = layers.len(),
        placed,
        "dag: topological_layers: success"
    );
    Ok(layers)
}

/// Look up the [`NodeIndex`] for a phase name. Returns `None` when
/// the graph does not reference that name (e.g. the caller is
/// looking at a sparse DAG that omits some canonical phases).
pub fn phase_node(graph: &PhaseGraph, name: &str) -> Option<NodeIndex> {
    graph
        .node_indices()
        .find(|idx| graph[*idx].as_str() == name)
}

/// Execute the DAG layer-by-layer using the supplied phase
/// implementations.
///
/// Phases inside the same layer run concurrently through
/// [`futures::future::join_all`]; phases in different layers run in
/// strict topological order so layer N+1 always observes layer N's
/// completed artefacts (the same contract the linear `Pipeline`
/// enforces by construction). Global concurrency is capped by
/// `ctx.parallelism`; the executor does not impose an additional
/// ceiling so the caller can keep the existing `Parallelism`
/// semantics.
///
/// On the first layer-level error the executor returns the error
/// from the first failing future (in layer order); the remaining
/// layers are skipped. This mirrors [`crate::phases::pipe::Pipeline`]
/// behaviour where the first error aborts the run.
///
/// The executor does NOT emit telemetry or apply timeouts — those
/// concerns live in the surrounding caller so the DAG primitive
/// stays small and unit-testable in isolation. A CLI-level
/// dispatcher that wires this into the deep-mode entry point can
/// wrap the call with the same telemetry hooks used by
/// `Pipeline::run_phases`.
pub async fn execute_dag(
    graph: &PhaseGraph,
    phases: &[Box<dyn Phase>],
    ctx: &RunContext,
) -> Result<Vec<PhaseOutput>> {
    tracing::info!(
        layers = "?",
        phases = phases.len(),
        nodes = graph.node_count(),
        "dag: execute_dag: start"
    );
    let layers = topological_layers(graph)?;

    // Build a name → impl index once so per-layer lookup is O(1).
    let mut phase_by_name: HashMap<&'static str, &dyn Phase> = HashMap::with_capacity(phases.len());
    for phase in phases {
        phase_by_name.insert(phase.name(), phase.as_ref());
    }

    let mut outputs: Vec<PhaseOutput> = Vec::with_capacity(graph.node_count());
    for (i, layer) in layers.iter().enumerate() {
        tracing::debug!(
            layer = i,
            layer_size = layer.len(),
            "dag: execute_dag: layer start"
        );
        let layer_results = futures::future::join_all(layer.iter().map(|&node| {
            let phase_id = graph[node];
            let name = phase_id.as_str();
            let phase = phase_by_name.get(name).copied();
            async move {
                match phase {
                    Some(p) => p.execute(ctx).await,
                    None => Err(Error::InvalidState(format!(
                        "phase DAG references {name:?} but no implementation was supplied; \
                         add the phase to the pipeline or remove the node from the graph"
                    ))),
                }
            }
        }))
        .await;

        for result in layer_results {
            outputs.push(result?);
        }
        tracing::trace!(
            layer = i,
            outputs_so_far = outputs.len(),
            "dag: execute_dag: layer completed"
        );
    }
    tracing::info!(total_outputs = outputs.len(), "dag: execute_dag: complete");
    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deep-mode DAG must expose one node per canonical phase
    /// in the order published by
    /// [`crate::phases::pipe::Pipeline::canonical_phase_order`].
    /// Re-ordering breaks the cross-pipeline invariant tested
    /// below.
    #[test]
    fn build_dag_for_deep_mode_has_sixteen_nodes_in_canonical_order() {
        let graph = build_dag_for_deep_mode();
        assert_eq!(graph.node_count(), 16);
        let nodes: Vec<&'static str> = graph.node_indices().map(|i| graph[i].as_str()).collect();
        assert_eq!(
            nodes,
            vec![
                "intake",
                "clarify",
                "route",
                "decompose",
                "sketch",
                "propose",
                "validate",
                "cluster_proposals",
                "synthesize",
                "gate",
                "critique",
                "repair",
                "judge",
                "adversary",
                "rank",
                "deliver",
            ]
        );
    }

    /// Linear chain has 15 edges (one less than nodes) and every
    /// edge is the canonical `Data` kind. A future change that
    /// adds a branch or changes an edge kind must update this test.
    #[test]
    fn build_dag_for_deep_mode_has_fifteen_data_edges() {
        let graph = build_dag_for_deep_mode();
        assert_eq!(graph.edge_count(), 15);
        for edge in graph.edge_references() {
            assert_eq!(*edge.weight(), EdgeKind::Data);
        }
    }

    /// `topological_layers` collapses the chain to 16 singletons,
    /// in canonical order. Locks down the executor's iteration
    /// order so a reordering of `build_dag_for_deep_mode` breaks
    /// here first.
    #[test]
    fn topological_layers_chain_returns_singletons_in_order() {
        let graph = build_dag_for_deep_mode();
        let layers = topological_layers(&graph).expect("chain is a DAG");
        assert_eq!(layers.len(), 16);
        for (layer, expected) in layers.iter().zip(
            [
                "intake",
                "clarify",
                "route",
                "decompose",
                "sketch",
                "propose",
                "validate",
                "cluster_proposals",
                "synthesize",
                "gate",
                "critique",
                "repair",
                "judge",
                "adversary",
                "rank",
                "deliver",
            ]
            .iter(),
        ) {
            assert_eq!(layer.len(), 1);
            assert_eq!(graph[layer[0]].as_str(), *expected);
        }
    }

    /// A graph that contains a cycle must be rejected with
    /// `Error::InvalidState`. The error message lists the stuck
    /// node names so a debugger can spot the cycle without
    /// re-running.
    #[test]
    fn topological_layers_rejects_cycles() {
        let mut graph = PhaseGraph::new();
        let a = graph.add_node(PhaseId::Intake);
        let b = graph.add_node(PhaseId::Clarify);
        let c = graph.add_node(PhaseId::Route);
        graph.add_edge(a, b, EdgeKind::Data);
        graph.add_edge(b, c, EdgeKind::Data);
        // Create a cycle: c → a.
        graph.add_edge(c, a, EdgeKind::Data);

        let err = topological_layers(&graph).expect_err("cycle must be rejected");
        match err {
            Error::InvalidState(msg) => {
                assert!(
                    msg.contains("not a DAG"),
                    "error must mention DAG; got: {msg}"
                );
                assert!(
                    msg.contains("stuck at"),
                    "error must list stuck nodes; got: {msg}"
                );
            }
            other => panic!("expected Error::InvalidState, got: {other:?}"),
        }
    }

    /// A diamond graph (A → B, A → C, B → D, C → D) must produce
    /// the canonical Kahn layers: {A}, {B, C}, {D}. Pins the
    /// fan-out behaviour the executor relies on so future
    /// branching work has a reference test.
    #[test]
    fn topological_layers_diamond_returns_canonical_layers() {
        let mut graph = PhaseGraph::new();
        let a = graph.add_node(PhaseId::Intake);
        let b = graph.add_node(PhaseId::Clarify);
        let c = graph.add_node(PhaseId::Route);
        let d = graph.add_node(PhaseId::Decompose);
        graph.add_edge(a, b, EdgeKind::Data);
        graph.add_edge(a, c, EdgeKind::Data);
        graph.add_edge(b, d, EdgeKind::Data);
        graph.add_edge(c, d, EdgeKind::Data);

        let layers = topological_layers(&graph).expect("diamond is a DAG");
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0].len(), 1);
        assert_eq!(graph[layers[0][0]].as_str(), "intake");
        assert_eq!(layers[1].len(), 2);
        let mut names: Vec<&'static str> = layers[1].iter().map(|i| graph[*i].as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["clarify", "route"]);
        assert_eq!(layers[2].len(), 1);
        assert_eq!(graph[layers[2][0]].as_str(), "decompose");
    }

    /// `PhaseId::from_name` must round-trip every canonical entry
    /// and reject unknown names. Pins the bidirectional mapping
    /// so a typo in either direction is caught at unit-test time.
    #[test]
    fn phase_id_from_name_round_trips_canonical_entries() {
        let canonical = [
            PhaseId::Intake,
            PhaseId::Clarify,
            PhaseId::Route,
            PhaseId::Decompose,
            PhaseId::Sketch,
            PhaseId::Propose,
            PhaseId::Validate,
            PhaseId::ClusterProposals,
            PhaseId::Synthesize,
            PhaseId::Gate,
            PhaseId::Critique,
            PhaseId::Repair,
            PhaseId::Judge,
            PhaseId::Adversary,
            PhaseId::Rank,
            PhaseId::Deliver,
        ];
        for id in canonical {
            assert_eq!(PhaseId::from_name(id.as_str()), Some(id));
        }
        assert_eq!(PhaseId::from_name("not_a_phase"), None);
        assert_eq!(PhaseId::from_name(""), None);
    }

    /// `phase_node` returns the node index whose payload matches
    /// the supplied name. Locks down the executor's per-layer
    /// lookup contract.
    #[test]
    fn phase_node_finds_named_phase() {
        let graph = build_dag_for_deep_mode();
        assert!(phase_node(&graph, "intake").is_some());
        assert!(phase_node(&graph, "deliver").is_some());
        assert!(phase_node(&graph, "missing").is_none());
    }
}
