//! Pipeline phases. v0.2 ships a non-discovery pipeline with optional
//! sketches
//! (intake → clarify → route → sketch? → propose → gate → validate →
//! critique → repair → judge → rank → deliver) per         . The
//! sketch step is gated by `Mode::runs_sketches()`; `fast` skips it.
//! Phase D adds `cluster_proposals` + `synthesize` between critique
//! and judge, and an adversary branch inside `judge` (        ).

pub mod adversary;
pub mod budget;
pub mod cardinality;
pub mod clarify;
pub mod cluster_proposals;
pub mod critique;
#[cfg(feature = "dag")]
pub mod dag;
pub mod decompose;
pub mod deliver;
pub mod discover_cluster;
pub mod discover_contradict;
pub mod discover_dimensions;
pub mod discover_extract;
pub mod discover_facet;
pub mod discover_integrate;
pub mod discover_matrix;
pub mod discover_summary;
pub mod discover_tag;
pub mod gate;
pub mod intake;
pub mod judge;
pub mod phase;
pub mod pipe;
pub mod propose;
pub mod rank;
pub mod refine;
pub mod repair;
pub mod replace;
pub mod route;
pub mod sketch_phase;
pub mod synthesize;
pub mod util;
pub mod validate;

pub use adversary::{
    AdversaryPhase, PATTERN_ADVERSARY_SCHEMA_VERSION, PatternAdversaryReport,
    PatternAdversarySection, ProposalPatternVerdict,
};
pub use budget::{BudgetObserver, PressureLevel};
pub use clarify::ClarifyPhase;
pub use cluster_proposals::ClusterProposalsPhase;
pub use critique::CritiquePhase;
#[cfg(feature = "dag")]
pub use dag::{
    EdgeKind, PhaseGraph, PhaseId, build_dag_for_deep_mode, execute_dag, phase_node,
    topological_layers,
};
pub use decompose::DecomposePhase;
pub use deliver::DeliverPhase;
pub use discover_cluster::DiscoverClusterPhase;
pub use discover_contradict::DiscoverContradictPhase;
pub use discover_dimensions::DiscoverDimensionsPhase;
pub use discover_extract::DiscoverExtractPhase;
pub use discover_facet::DiscoverFacetPhase;
pub use discover_integrate::DiscoverIntegratePhase;
pub use discover_matrix::DiscoverMatrixPhase;
pub use discover_summary::DiscoverSummaryPhase;
pub use discover_tag::DiscoverTagPhase;
pub use gate::GatePhase;
pub use intake::IntakePhase;
pub use judge::JudgePhase;
pub use phase::{Phase, PhaseOutput, RunContext, resolve_temperature, temperature_for_role};
pub use pipe::{Pipeline, PipelineKind};
pub use propose::ProposePhase;
pub use rank::RankPhase;
pub use refine::{
    DROPPED_SENTINEL, NOOP_REASON_MERGE, NOOP_REASON_SPLIT, RefineContext, RefineDispatchPlan,
    dispatch_refine_action,
};
pub use repair::RepairPhase;
pub use route::RoutePhase;
pub use sketch_phase::SketchPhase;
pub use synthesize::SynthesizePhase;
pub use validate::ValidatePhase;

/// Build context for [`PhaseFactory::build`].
///
/// Each phase factory receives this context and pulls the values
/// it needs. The struct is intentionally narrow: only the values
/// that vary across modes (counts, feature flags) live here. The
/// static configuration lives in `cfg` so factories that need it
/// (e.g. `RepairPhase::from_config`) can pass it through.
pub struct BuildCtx<'a> {
    /// Resolved configuration. Shared across factories.
    pub cfg: &'a crate::config::Config,
    /// Active run mode. Factories use this to gate themselves (the
    /// `modes` filter on `PhaseFactory` is the coarse filter;
    /// `mode` is available for finer per-mode logic if a phase
    /// ever needs it).
    pub mode: crate::cli::Mode,
    /// Per-mode counts derived from `pipeline_shape`. Factories
    /// that need a count (e.g. `SketchPhase { count }`) read it
    /// from here.
    pub shape: BuildShape,
    /// `true` when the run opted into the replace-sources path
    /// (operator flag or `Mode::Deep` default).
    pub replace_sources_enabled: bool,
    /// `true` when the LLM-based adversary pass is enabled. The
    /// `AdversaryPhase` factory reads this.
    pub adversary_enabled: bool,
}

/// Per-mode counts derived from `pipeline_shape`. Lives in
/// `phases` (not `cli::run`) so phase modules can build their
/// factories without depending on `cli::run::PipelineShape`.
#[derive(Clone, Copy)]
pub struct BuildShape {
    /// Number of proposals to generate (consumed by `ProposePhase`).
    pub proposals: u32,
    /// Number of judges per proposal (consumed by `JudgePhase`).
    pub judges: u32,
    /// Number of sketches per problem cell (consumed by `SketchPhase`).
    pub sketches: u32,
    /// Number of critics per proposal (consumed by `CritiquePhase`).
    pub critics: u32,
}

/// Returns `true` when the named phase runs in `mode` according
/// to the linear pipeline rules that used to live inline in
/// `cli::run::build_pipeline_for_mode`. Centralised here so the
/// dispatcher can stay table-driven.
pub fn linear_phase_runs_in(name: &str, mode: crate::cli::Mode) -> bool {
    use crate::cli::Mode::*;
    match name {
        // Always run: intake / clarify / route / gate / critique /
        // repair / judge / adversary / rank / deliver.
        "intake" | "clarify" | "route" | "gate" | "critique" | "repair" | "judge" | "adversary"
        | "rank" | "deliver" => true,
        // Deep only.
        "decompose" => mode == Deep,
        // `runs_sketches` is true for everything except Fast and
        // Explore; mirror it here.
        "sketch" => matches!(mode, Standard | Deep | Batch),
        // Propose runs in every mode except Explore (Explore ends
        // at sketches per the comment in the legacy dispatcher).
        "propose" => mode != Explore,
        // Validate only runs when proposals exist (i.e. every mode
        // except Explore, but also excluding Fast — Fast stays
        // cheap because the structural checks live inside Gate).
        "validate" => matches!(mode, Standard | Deep | Batch),
        // Cluster + Synthesize run everywhere except Fast.
        "cluster_proposals" | "synthesize" => mode != Fast,
        other => {
            tracing::debug!(
                phase = other,
                "linear_phase_runs_in: unknown phase name (caller must check canonical order)"
            );
            false
        }
    }
}

/// Construct a `Box<dyn Phase>` for the named phase using the
/// given build context. Returns `None` for unknown names. The
/// caller (the linear pipeline dispatcher) is responsible for
/// filtering by `linear_phase_runs_in(name, mode)` before calling
/// here so per-mode gates stay declarative at the dispatch site.
pub fn build_linear_phase(name: &str, ctx: &BuildCtx<'_>) -> Option<Box<dyn Phase>> {
    use std::sync::Arc;
    let phase: Box<dyn Phase> = match name {
        "intake" => Box::new(IntakePhase),
        "clarify" => Box::new(ClarifyPhase),
        "route" => Box::new(RoutePhase),
        "decompose" => Box::new(DecomposePhase),
        "sketch" => Box::new(SketchPhase {
            count: ctx.shape.sketches,
        }),
        "propose" => Box::new(ProposePhase {
            count: ctx.shape.proposals,
        }),
        "validate" => Box::new(ValidatePhase::new()),
        "cluster_proposals" => Box::new(ClusterProposalsPhase::default()),
        "synthesize" => Box::new(SynthesizePhase::default()),
        "gate" => Box::new(GatePhase),
        "critique" => Box::new(CritiquePhase {
            critics_per_proposal: ctx.shape.critics,
        }),
        "repair" => Box::new(RepairPhase::from_config(ctx.cfg)),
        "judge" => Box::new(JudgePhase {
            judges: ctx.shape.judges,
            ..JudgePhase::default()
        }),
        "adversary" => Box::new(AdversaryPhase {
            enable: ctx.adversary_enabled,
        }),
        "rank" => Box::new(RankPhase {
            config: Arc::new(ctx.cfg.clone()),
            replace_sources_enabled: ctx.replace_sources_enabled,
            stability_enabled: ctx.cfg.stability.enabled,
        }),
        "deliver" => Box::new(DeliverPhase),
        _ => return None,
    };
    Some(phase)
}
