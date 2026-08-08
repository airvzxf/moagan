//! `moagan discover` — discovery mode (Plan B sub-phase B).
//!
//! Discovery is a separate subcommand (not a `--mode discovery` flag on
//! `moagan run`) because its pipeline diverges heavily from the
//! linear `intake → clarify → route → propose → ...` flow:
//!
//! 1. Build an `ExplorationMatrix` (roles × models × temperatures).
//! 2. Fan out 80+ sketches via `DiscoverMatrixPhase`.
//! 3. Tag each sketch with `DiscoverTagPhase` (LLM tagger).
//! 4. Cluster via SimHash + LLM refinement (`DiscoverClusterPhase`).
//! 5. Detect cross-cluster contradictions (`DiscoverContradictPhase`).
//! 6. Derive facets per category (`DiscoverFacetPhase`).
//! 7. Extract per-facet markdown (`DiscoverExtractPhase`).
//! 8. Integrate into `final/cat_NN.md` + `final/summary.md`
//!    (`DiscoverIntegratePhase`).
//!
//! The output is a *biblia* (knowledge base), not a winning proposal.
//!
//! Discovery deliberately does NOT route through `cmd::Run::Run`:
//! - It uses different LLM roles (`tagger`, `extractor`, `integrator`).
//! - It writes to `tags/`, `clusters/`, `facets/`, `extractions/`,
//!   `final/cat_NN.md` instead of the standard `proposals/` path.
//! - It does not produce a `ranking.json` or `portfolio.md`.
//!
//! Cardinality minimum is 80 sketches; the spec says 40–500 (V4 §6.4)
//! and the user's Plan B preferred the upper half of the lower band.

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;
use crate::discovery::{DiscoveryCoordinator, DiscoveryOutcome};
use crate::error::{Error, Result};
use crate::execution::Parallelism;
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::phases::RunContext;
use crate::phases::{
    ClarifyPhase, DiscoverClusterPhase, DiscoverContradictPhase, DiscoverExtractPhase,
    DiscoverFacetPhase, DiscoverIntegratePhase, DiscoverMatrixPhase, DiscoverSummaryPhase,
    DiscoverTagPhase, IntakePhase, Pipeline,
};
use crate::redact::RedactPolicy;
use crate::storage::sqlite::Db;
use crate::telemetry::Telemetry;

use super::run::build_registry_for;

/// Build the discovery pipeline. The phases are wired in the order
/// they appear in V4 §6.3:
///
/// 1. intake + clarify (mandatory seeding of the brief).
/// 2. discover_matrix (sketch fan-out).
/// 3. discover_tag (LLM tagger).
/// 4. discover_cluster (SimHash + LLM refinement).
/// 5. discover_contradict (cross-cluster disagreements).
/// 6. discover_facet (per-cluster facet list).
/// 7. discover_extract (per-facet markdown).
/// 8. discover_integrate (one `final/cat_NN.md` per cluster).
/// 9. discover_summary (executive index + optional uncategorized).
///
/// PR-17 (Coordinator wire-up) preserves this builder as a
/// "flat-pipeline" reference path. The CLI dispatcher
/// ([`run`]) now drives the sketch fan-out through
/// [`DiscoveryCoordinator::run_with_ctx`] and stitches the
/// post-matrix phases back together via
/// [`build_post_matrix_pipeline`]; the flat builder stays
/// available for unit tests that want to inspect the canonical
/// 10-phase order in isolation.
pub fn build_discovery_pipeline(opts: &DiscoverOptions) -> Pipeline {
    Pipeline::new()
        .push(IntakePhase)
        .push(ClarifyPhase)
        .push(DiscoverMatrixPhase::from_dimensions(
            opts.dimensions,
            opts.facets_per_dimension,
            opts.cardinality,
        ))
        .push(DiscoverTagPhase)
        .push(DiscoverClusterPhase {
            threshold: opts.cluster_threshold,
        })
        .push(DiscoverContradictPhase::default())
        .push(DiscoverFacetPhase::with_cache(opts.cache_facets))
        .push(DiscoverExtractPhase)
        .push(DiscoverIntegratePhase)
        .push(DiscoverSummaryPhase)
}

/// Build the pre-matrix pipeline (intake + clarify). PR-17 splits the
/// discovery flow so the sketch fan-out is driven by
/// [`DiscoveryCoordinator::run_with_ctx`], not by the pipeline runner.
/// Keeping `intake` + `clarify` in the pipeline preserves the
/// pause/resume hooks at those phase boundaries.
fn build_pre_matrix_pipeline() -> Pipeline {
    Pipeline::new().push(IntakePhase).push(ClarifyPhase)
}

/// Build the post-matrix pipeline (tag → cluster → … → summary).
/// PR-17 keeps these phases in the flat pipeline runner; the
/// coordinator owns only the matrix part. The pipeline's per-phase
/// cancel token still surfaces as a `StopDecision` at the matrix
/// boundary when the operator presses Ctrl-C.
fn build_post_matrix_pipeline(opts: &DiscoverOptions) -> Pipeline {
    Pipeline::new()
        .push(DiscoverTagPhase)
        .push(DiscoverClusterPhase {
            threshold: opts.cluster_threshold,
        })
        .push(DiscoverContradictPhase::default())
        .push(DiscoverFacetPhase::with_cache(opts.cache_facets))
        .push(DiscoverExtractPhase)
        .push(DiscoverIntegratePhase)
        .push(DiscoverSummaryPhase)
}

/// Options for `moagan discover`.
#[derive(Debug, Clone)]
pub struct DiscoverOptions {
    /// Provider name (must be in config).
    pub provider: String,
    /// User prompt.
    pub prompt: String,
    /// Optional override of the home directory.
    pub home: Option<PathBuf>,
    /// Optional directory of canned mock responses.
    pub mock_dir: Option<PathBuf>,
    /// Minimum number of sketches to generate. Default 80.
    pub cardinality: usize,
    /// Optional override of the global parallel cap.
    pub max_parallelism: Option<usize>,
    /// Number of dimensions in the exploration matrix. Default 4.
    pub dimensions: usize,
    /// Number of facets per dimension. Default 2.
    pub facets_per_dimension: usize,
    /// SimHash threshold for clustering (0..=1). Default 0.7.
    pub cluster_threshold: f32,
    /// Output directory for the run. Defaults to MOAGAN_HOME resolution.
    pub out_dir: Option<PathBuf>,
    /// Non-interactive: every checkpoint is a `<skipped:non_interactive>`
    /// marker instead of blocking on stdin. Required for CI / smoke
    /// runs where stdin is not a TTY.
    pub non_interactive: bool,
    /// Enable the cross-run facet cache. When `true`, the
    /// `discover_facet` phase writes derived facet lists to
    /// `<MOAGAN_HOME>/cache/facets/` and skips the
    /// `facet_deriver` LLM call on subsequent runs that share
    /// the same `(brief, category_id)` (V4 §6.8 + catalog
    /// D.13.13). Default `false` so the LLM-every-run baseline
    /// is preserved unless the operator opts in via the
    /// `--cache-facets` CLI flag.
    pub cache_facets: bool,
}

/// Run discovery end-to-end. Returns the run id on success.
pub async fn run(opts: DiscoverOptions, cfg: &Config) -> Result<RunId> {
    let home = Arc::new(match opts.home.clone() {
        Some(path) => MoaganHome::at(path),
        None => MoaganHome::resolve()?,
    });
    home.ensure()?;
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;

    let default_provider = if opts.provider.is_empty() {
        cfg.default_provider.clone()
    } else {
        opts.provider.clone()
    };
    let providers = Arc::new(build_registry_for(
        cfg,
        &default_provider,
        opts.mock_dir.as_deref(),
    )?);
    let default_model = cfg.provider(&default_provider)?.model.clone();

    let policy = RedactPolicy::default();
    let db = Db::open(&home.meta_db_path())?;
    db.register_run(
        run_id,
        "discover",
        "running",
        env!("CARGO_PKG_VERSION"),
        None,
        None,
        None,
    )?;
    let telemetry = Telemetry::open(run_id, &run_dir, policy, Some(db.clone()))?;
    let max_parallelism = opts.max_parallelism.unwrap_or(cfg.max_parallelism);
    let parallelism = Parallelism::new(max_parallelism);

    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        Arc::clone(&providers),
        default_provider.clone(),
        default_model,
        parallelism,
        telemetry.clone(),
        opts.prompt.clone(),
        "discover".to_owned(),
    )
    .with_timeouts(cfg.phase_timeout_secs, cfg.total_timeout_secs)
    .with_interactive(!opts.non_interactive);

    let pipeline = build_pre_matrix_pipeline();
    let pipeline_future = pipeline.run(&ctx);
    tokio::pin!(pipeline_future);
    let _outputs = tokio::select! {
        result = &mut pipeline_future => result?,
        _ = tokio::signal::ctrl_c() => {
            ctx.cancel().cancel(crate::cancel::CancelReason::UserInterrupt);
            return Err(ctx.cancel().into_error());
        }
    };

    // PR-17: drive the sketch fan-out through the discovery
    // coordinator instead of the flat `DiscoverMatrixPhase`. The
    // coordinator owns its own crash-recovery state machine
    // (`SketchLoopState`) and applies the spec's saturation
    // stop policy via `SaturationTracker`. The cancel token is
    // the same handle the pre-matrix pipeline honoured, so a
    // Ctrl-C during the matrix part still short-circuits the
    // loop cleanly.
    let coordinator = DiscoveryCoordinator::new(
        (*home).clone(),
        run_id,
        ctx.cancel().clone(),
        crate::domain::Brief::default(),
        "deployment-model:serverless".to_owned(),
        crate::cli::Mode::Fast,
    );
    let coordinator_ctx = Arc::new(ctx.clone());
    let coordinator_future =
        coordinator.run_with_ctx_and_target(coordinator_ctx.clone(), Some(opts.cardinality));
    tokio::pin!(coordinator_future);
    let outcome: DiscoveryOutcome = tokio::select! {
        result = &mut coordinator_future => result.map_err(|e| match e {
            crate::discovery::coordinator::CoordinatorError::Error(inner) => inner,
        })?,
        _ = tokio::signal::ctrl_c() => {
            ctx.cancel().cancel(crate::cancel::CancelReason::UserInterrupt);
            return Err(ctx.cancel().into_error());
        }
    };
    tracing::info!(
        sketches_completed = outcome.sketches_completed,
        sketches_failed = outcome.sketches_failed,
        "DiscoveryCoordinator::run_with_ctx finished; running post-matrix pipeline"
    );

    let post_pipeline = build_post_matrix_pipeline(&opts);
    let post_future = post_pipeline.run(&ctx);
    tokio::pin!(post_future);
    let _outputs = tokio::select! {
        result = &mut post_future => result?,
        _ = tokio::signal::ctrl_c() => {
            ctx.cancel().cancel(crate::cancel::CancelReason::UserInterrupt);
            return Err(ctx.cancel().into_error());
        }
    };

    telemetry.flush()?;
    if let Err(e) = db.update_run_status(run_id, "completed") {
        eprintln!("warn: failed to update run status: {e}");
    }
    println!(
        "moagan discover {} provider={} -> {}",
        run_id.short(),
        default_provider,
        run_dir.root().display()
    );
    Ok(run_id)
}

/// Pull the `Discover` subcommand's `cardinality` value out of an
/// argument list, falling back to the provided default when the flag
/// is absent. Used by `discover_cmd` to validate the input before
/// spawning the run.
#[allow(dead_code)]
pub(crate) fn parse_cardinality(args: &[String], default: usize) -> Result<usize> {
    let mut value = default;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--cardinality" || args[i] == "--sketches" {
            let next = args
                .get(i + 1)
                .ok_or_else(|| Error::InvalidArgs(format!("{} needs a value", args[i])))?;
            value = next
                .parse()
                .map_err(|e| Error::InvalidArgs(format!("invalid cardinality: {e}")))?;
            break;
        }
        i += 1;
    }
    if value < 80 {
        return Err(Error::InvalidArgs(format!(
            "cardinality {value} below the discovery minimum of 80"
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cardinality_default_when_flag_missing() {
        let v = parse_cardinality(&[], 80).unwrap();
        assert_eq!(v, 80);
    }

    #[test]
    fn parse_cardinality_reads_explicit_flag() {
        let v = parse_cardinality(&["--cardinality".to_string(), "120".to_string()], 80).unwrap();
        assert_eq!(v, 120);
    }

    #[test]
    fn parse_cardinality_accepts_long_sketches_alias() {
        let v = parse_cardinality(&["--sketches".to_string(), "200".to_string()], 80).unwrap();
        assert_eq!(v, 200);
    }

    #[test]
    fn parse_cardinality_rejects_below_minimum() {
        let e = parse_cardinality(&["--cardinality".to_string(), "5".to_string()], 80).unwrap_err();
        assert!(e.to_string().contains("below the discovery minimum"));
    }

    #[test]
    fn parse_cardinality_rejects_non_numeric() {
        let e =
            parse_cardinality(&["--cardinality".to_string(), "abc".to_string()], 80).unwrap_err();
        assert!(e.to_string().contains("invalid cardinality"));
    }

    #[test]
    fn parse_cardinality_requires_value() {
        let e = parse_cardinality(&["--cardinality".to_string()], 80).unwrap_err();
        assert!(e.to_string().contains("needs a value"));
    }
}
