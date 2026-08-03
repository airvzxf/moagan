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
        .push(DiscoverFacetPhase)
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
    .with_timeouts(cfg.phase_timeout_secs, cfg.total_timeout_secs);

    let pipeline = build_discovery_pipeline(&opts);
    let pipeline_future = pipeline.run(&ctx);
    tokio::pin!(pipeline_future);
    let _outputs = tokio::select! {
        result = &mut pipeline_future => result?,
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
