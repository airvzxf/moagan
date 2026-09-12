//! Pipeline phases. The linear pipeline is:
//! (intake → clarify → route → sketch? → propose → gate → validate →
//! critique → repair → judge → rank → deliver). The sketch step is
//! gated by `Mode::runs_sketches()`; `fast` skips it. Phase D adds
//! `cluster_proposals` + `synthesize` between critique and judge,
//! plus an adversary branch inside `judge`. The discovery pipeline
//! lives in `src/phases/discover_*.rs` and is wired through the
//! `moagan discover` subcommand.

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
///
/// The per-mode contract is pinned by the unit tests at the
/// bottom of this module (`linear_phase_runs_in_*`) and by the
/// integration test `explore_mode_pipeline_terminates_at_sketches`
/// in `tests/integration_mvp.rs`. Any change to this match must
/// keep both green.
pub fn linear_phase_runs_in(name: &str, mode: crate::cli::Mode) -> bool {
    use crate::cli::Mode::*;
    match name {
        // Pre-sketch phases always run (every mode produces an
        // intake brief, clarification round-trip, and routing
        // decision before any other work).
        "intake" | "clarify" | "route" => true,
        // Heavy-mode-only structural decomposition. Standard /
        // Batch / Explore / Fast skip this phase.
        "decompose" => mode == Deep,
        // Sketch fans out the explore map; mirrors
        // `Mode::runs_sketches()` so the dispatcher agrees with
        // the spec. `explore` runs 12 sketches and ends there;
        // the other non-Fast modes use sketches as input to the
        // proposal fan-out.
        "sketch" => mode != Fast,
        // Propose requires a proposal-count > 0; `explore` is
        // sketches-only by spec (see
        // `cli::run::pipeline_shape` docstring).
        "propose" => mode != Explore,
        // Validate only runs when proposals exist AND the mode
        // has the budget for the extra sandbox invocation; `fast`
        // keeps Gate self-contained and `explore` ends at
        // sketches.
        "validate" => matches!(mode, Standard | Deep | Batch),
        // Cluster + Synthesize require proposals to operate on.
        // `fast` skips Phase D entirely; `explore` has no
        // proposals (Propose is gated off above). Both Phase D
        // pair arms therefore skip.
        "cluster_proposals" | "synthesize" => matches!(mode, Standard | Deep | Batch),
        // Gate / critique / repair / judge / adversary / rank /
        // deliver all need a populated `proposals/` tree and a
        // ranking with a non-empty winner. `explore` ends at
        // sketches so all of these are skipped; the other modes
        // (fast / standard / deep / batch) run them in order.
        // This restores the pre-PR-#850 early-return guard at
        // `cli::run::build_pipeline_for_mode` (commit 5929527
        // refactored the dispatch table but lost the `if mode ==
        // Explore { return; }` short-circuit — see issue #881).
        "gate" | "critique" | "repair" | "judge" | "adversary" | "rank" | "deliver" => {
            mode != Explore
        }
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

#[cfg(test)]
mod linear_phase_runs_in_tests {
    //! Regression guard for issue #881.
    //!
    //! PR #850 (commit `5929527`) refactored the linear pipeline
    //! dispatcher from a hand-written push chain in
    //! `cli::run::build_pipeline_for_mode` into a table-driven
    //! match in `linear_phase_runs_in`. The refactor lost the
    //! `if mode == Mode::Explore { return pipeline; }` early
    //! return that lived in the legacy dispatcher — and silently
    //! dropped `sketch` from the explore path — so the explore
    //! pipeline began running `cluster_proposals → synthesize →
    //! gate → critique → repair → judge → adversary → rank →
    //! deliver` on an empty `proposals/` directory. The empty
    //! `Ranking::default()` written by `rank.rs` then cascaded
    //! into `deliver.rs` reading the literal path
    //! `proposals/.json` (empty `format!("{}.json", "")`),
    //! surfacing as `Error::Io(NotFound)` with exit code 8 and
    //! failing the post-release-validation `e2e-network-explore`
    //! smoke tests on every release tag since v0.17.0.
    //!
    //! The matrix below pins the per-mode contract. Any future
    //! refactor of `linear_phase_runs_in` must keep these
    //! expectations intact or this test will fail at `cargo
    //! test`, blocking the regression at T1 rather than at
    //! release-time T3.
    use super::linear_phase_runs_in;
    use crate::cli::Mode;

    const ALL_PHASES: &[&str] = &[
        "intake",
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
    ];
    // Note: `clarify` and `route` always run (omitted here for
    // brevity; covered separately below).

    fn assert_runs(mode: Mode, expected: &[&str]) {
        for &name in ALL_PHASES {
            let got = linear_phase_runs_in(name, mode);
            let want = expected.contains(&name);
            assert_eq!(
                got, want,
                "linear_phase_runs_in({name:?}, {mode:?}): expected {want}, got {got}"
            );
        }
        // Pre-sketch phases always run for every mode.
        assert!(linear_phase_runs_in("clarify", mode));
        assert!(linear_phase_runs_in("route", mode));
    }

    #[test]
    fn fast_skips_sketch_propose_validate_phase_d() {
        // Per `cli::run::pipeline_shape` docstring: fast = 3
        // proposals, 1 judge, no sketch, no synthesis.
        assert_runs(
            Mode::Fast,
            &[
                "intake",
                "propose",
                "gate",
                "critique",
                "repair",
                "judge",
                "adversary",
                "rank",
                "deliver",
            ],
        );
    }

    #[test]
    fn explore_runs_intake_clarify_route_sketch_only() {
        // Per `cli::run::pipeline_shape` docstring: explore = 0
        // proposals, 0 judges, 12 sketches. Pipeline ends at
        // sketches. Mirrors the legacy `if mode == Mode::Explore
        // { return pipeline; }` early return that pre-PR-#850
        // code enforced.
        assert_runs(Mode::Explore, &["intake", "sketch"]);
    }

    #[test]
    fn standard_runs_every_phase_except_decompose() {
        assert_runs(
            Mode::Standard,
            &[
                "intake",
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
            ],
        );
    }

    #[test]
    fn deep_runs_all_phases_including_decompose() {
        assert_runs(
            Mode::Deep,
            &[
                "intake",
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
            ],
        );
    }

    #[test]
    fn batch_runs_same_as_standard_minus_decompose() {
        assert_runs(
            Mode::Batch,
            &[
                "intake",
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
            ],
        );
    }

    #[test]
    fn unknown_phase_is_skipped() {
        for mode in [
            Mode::Fast,
            Mode::Standard,
            Mode::Deep,
            Mode::Explore,
            Mode::Batch,
        ] {
            assert!(
                !linear_phase_runs_in("not_a_real_phase", mode),
                "unknown phase must be skipped for {mode:?}"
            );
        }
    }
}
