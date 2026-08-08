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
use crate::phases::Pipeline;
use crate::phases::PipelineKind;
use crate::phases::RunContext;
use crate::phases::{
    ClarifyPhase, DiscoverClusterPhase, DiscoverContradictPhase, DiscoverExtractPhase,
    DiscoverFacetPhase, DiscoverIntegratePhase, DiscoverMatrixPhase, DiscoverSummaryPhase,
    DiscoverTagPhase, IntakePhase,
};
use crate::redact::RedactPolicy;
use crate::storage::sqlite::Db;
use crate::telemetry::Telemetry;

use crate::cli::continue_cmd::load_manifest;
use crate::domain::Manifest;

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
///
/// v0.5 PR-24: the same canonical list is also exposed via
/// [`Pipeline::canonical_phase_order_for(PipelineKind::Discovery)`]
/// so `Pipeline::resume_with_kind(canonical, last_phase, Discovery)`
/// can dispatch correctly. Keep this list and
/// [`Pipeline::canonical_phase_order_for(PipelineKind::Discovery)`]
/// in lock-step: a re-ordering on either side silently breaks
/// resume.
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

// =====================================================================
// v0.5 PR-24: discovery resume
// =====================================================================

/// Default cardinalities used by [`run_resume`] when the
/// `discovery_matrix.json` artefact is missing or malformed on
/// resume. The coordinator falls back to
/// [`Cardinality::for_mode_default`] which uses 80 sketches; we
/// pass 80 explicitly here so the `ExplorationMatrix` we rebuild
/// for the resume matches the size the run was originally
/// configured with.
const RESUME_DEFAULT_CARDINALITY: usize = 80;
const RESUME_DEFAULT_DIMENSIONS: usize = 4;
const RESUME_DEFAULT_FACETS_PER_DIMENSION: usize = 2;
const RESUME_DEFAULT_CLUSTER_THRESHOLD: f32 = 0.7;

/// Read the discovery matrix cardinality from
/// `<run_dir>/exploration_matrix.json` if present. Falls back to
/// [`RESUME_DEFAULT_CARDINALITY`] when the file is missing or
/// malformed; the coordinator's persisted state
/// (`.discovery_state.json`) takes precedence over both when the
/// matrix size does not match the loop's `completed_sketches`.
fn resume_target_cardinality(home: &MoaganHome, run_id: RunId) -> usize {
    let path = home.run_dir(run_id).root().join("exploration_matrix.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return RESUME_DEFAULT_CARDINALITY;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return RESUME_DEFAULT_CARDINALITY;
    };
    value
        .get("cardinality")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(RESUME_DEFAULT_CARDINALITY)
}

/// Resume a paused or failed `moagan discover` run.
///
/// v0.5 PR-24 (V4 §6.11, T01-06 §10.2). The dispatch contract:
///
/// - The caller ([`crate::cli::continue_cmd::run_continue`])
///   guarantees `manifest.mode == "discover"` and the kind is
///   [`PipelineKind::Discovery`]. Linear runs do NOT route
///   through this helper; they use
///   [`crate::cli::continue_cmd::resume_pipeline`].
/// - The canonical 10-phase discovery pipeline is rebuilt via
///   [`Pipeline::resume_with_kind`] against
///   [`PipelineKind::Discovery`] so the cutoff index is sourced
///   from the discovery canonical list (not the linear one — that
///   was the bug PR-24 closed: `unknown phase "discover_matrix"`).
/// - The execution strategy mirrors [`run`]: the matrix fan-out
///   is driven by [`DiscoveryCoordinator::run_with_ctx`] when the
///   resume point is at-or-before `discover_matrix`; the
///   post-matrix phases run through the standard
///   [`Pipeline::run`] path with `resume: true` so each phase
///   event in `telemetry/phases.jsonl.gz` carries the
///   `resume: true` flag.
///
/// The `last_phase` argument comes from
/// `Db::last_completed_phase(run_id)` and is the phase name
/// recorded in the SQLite `phases` table. We use it as the cutoff
/// for [`Pipeline::resume_with_kind`]; the helper returns the
/// remaining phases and we translate them into the coordinator +
/// post-matrix execution.
pub async fn run_resume(
    home: &MoaganHome,
    manifest: &Manifest,
    last_phase: &str,
    api_key: Option<&str>,
    non_interactive: bool,
) -> Result<()> {
    if manifest.mode != "discover" {
        return Err(Error::InvalidArgs(format!(
            "continue --kind discovery requires manifest.mode = \"discover\"; \
             got {:?} (use `--kind linear` or omit `--kind` for linear runs)",
            manifest.mode
        )));
    }

    let run_id = manifest.run_id;
    let run_dir = home.run_dir(run_id);

    // Build the canonical discovery pipeline (10 phases) and
    // filter it via `Pipeline::resume_with_kind` so we get the
    // list of phases the resume should run. This is the same list
    // exposed by `Pipeline::canonical_phase_order_for(Discovery)`;
    // using the kind-aware resume means `last_phase == "clarify"`
    // correctly resolves and produces `discover_matrix + ... +
    // discover_summary` instead of the linear `unknown phase`
    // error that motivated PR-24.
    let canonical = build_canonical_for_resume_pipeline(manifest);
    let resumed = Pipeline::resume_with_kind(canonical, last_phase, PipelineKind::Discovery)?;
    if resumed.is_empty() {
        eprintln!(
            "moagan continue --kind discovery {run_id}: nothing left to do after phase {last_phase:?}"
        );
        return Ok(());
    }

    let default_provider = if manifest.provider.is_empty() || manifest.provider == "unknown" {
        Config::load().unwrap_or_default().default_provider.clone()
    } else {
        manifest.provider.clone()
    };
    let cfg = Config::load().unwrap_or_default();

    let home_arc = Arc::new(home.clone());
    let providers = Arc::new(super::run::build_registry_for_with_api_key(
        &cfg,
        &default_provider,
        None,
        api_key,
    )?);
    let default_model = cfg
        .provider(&default_provider)
        .map(|spec| spec.model.clone())
        .unwrap_or_else(|_| "unknown".to_string());
    let policy = RedactPolicy::default();
    let db = Db::open(&home.meta_db_path())?;
    let telemetry = Telemetry::open(run_id, &run_dir, policy, Some(db.clone()))?;
    let parallelism = Parallelism::new(cfg.max_parallelism);
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home_arc),
        providers,
        default_provider.clone(),
        default_model.clone(),
        parallelism,
        telemetry.clone(),
        String::new(),
        manifest.mode.clone(),
    )
    .with_interactive(!non_interactive);

    // Decide whether the resume should re-run the coordinator
    // (matrix fan-out) or skip directly to the post-matrix
    // pipeline. The rule mirrors the canonical discovery order:
    //   last_phase ∈ {"intake", "clarify"} → re-run matrix.
    //   last_phase == "discover_matrix"    → matrix already done, run post.
    //   last_phase is a discover_* post-matrix phase → filter post.
    let needs_matrix = matches!(last_phase, "intake" | "clarify");

    if needs_matrix {
        // Wrap the coordinator call with phase events so
        // `telemetry/phases.jsonl.gz` records `discover_matrix` as
        // a real phase (start + end) on the resumed run, mirroring
        // the original run's behaviour. `resume: true` flows
        // through `Pipeline::run`'s marker so every event from
        // the resumed pipeline carries the flag.
        ctx.telemetry
            .phase("discover_matrix", 0, "start", None, true)?;
        let coordinator = DiscoveryCoordinator::new(
            (*home_arc).clone(),
            run_id,
            ctx.cancel().clone(),
            crate::domain::Brief::default(),
            "deployment-model:serverless".to_owned(),
            crate::cli::Mode::Fast,
        );
        let coordinator_ctx = Arc::new(ctx.clone());
        let target = resume_target_cardinality(home_arc.as_ref(), run_id);
        let outcome = match tokio::select! {
            result = coordinator.run_with_ctx_and_target(coordinator_ctx.clone(), Some(target)) => result,
            _ = tokio::signal::ctrl_c() => {
                ctx.cancel().cancel(crate::cancel::CancelReason::UserInterrupt);
                return Err(ctx.cancel().into_error());
            }
        } {
            Ok(o) => o,
            Err(crate::discovery::coordinator::CoordinatorError::Error(inner)) => {
                ctx.telemetry.phase(
                    "discover_matrix",
                    0,
                    "error",
                    Some(&inner.to_string()),
                    true,
                )?;
                return Err(inner);
            }
        };
        ctx.telemetry
            .phase("discover_matrix", 0, "end", None, true)?;
        tracing::info!(
            sketches_completed = outcome.sketches_completed,
            sketches_failed = outcome.sketches_failed,
            "discovery resume: coordinator finished; running post-matrix pipeline"
        );
    } else {
        // The matrix fan-out is already complete; emit a no-op
        // marker so the resumed pipeline's `discover_matrix` is
        // distinguishable from the original run's. We log
        // "skipped" rather than touching the SQLite `phases` table
        // — the resume is allowed to skip already-completed phases
        // without poisoning the timeline.
        tracing::info!(
            last_phase,
            "discovery resume: skipping matrix fan-out (already complete)"
        );
    }

    // Run the post-matrix pipeline end-to-end from this point on.
    // The matrix completion (or skip) above guarantees the input
    // artefacts the post-matrix phases expect are present.
    let post_opts = DiscoverOptions {
        provider: default_provider.clone(),
        prompt: String::new(),
        home: Some(home_arc.root().to_path_buf()),
        mock_dir: None,
        cardinality: RESUME_DEFAULT_CARDINALITY,
        max_parallelism: None,
        dimensions: RESUME_DEFAULT_DIMENSIONS,
        facets_per_dimension: RESUME_DEFAULT_FACETS_PER_DIMENSION,
        cluster_threshold: RESUME_DEFAULT_CLUSTER_THRESHOLD,
        out_dir: None,
        non_interactive,
        cache_facets: false,
    };
    let post_pipeline = build_post_matrix_pipeline(&post_opts);

    // Translate the discovery `last_phase` into the equivalent
    // post-matrix cutoff so the filter skips phases that were
    // already completed in the original run.
    let post_filter_from = match last_phase {
        "intake" | "clarify" | "discover_matrix" => "discover_tag",
        other => other,
    };
    let post_resumed = Pipeline::resume(post_pipeline, post_filter_from)?;

    let post_future = post_resumed.run(&ctx);
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
        "moagan continue --kind discovery {run_id}: resumed after phase {last_phase:?}",
        run_id = run_id.short(),
    );
    Ok(())
}

/// Build the canonical 10-phase discovery pipeline used as the
/// reference list for [`Pipeline::resume_with_kind`] in
/// [`run_resume`]. The matrix's `cardinality` field is sourced
/// from `exploration_matrix.json` if present, otherwise the
/// default; this keeps the resumed matrix shape consistent with
/// the original run. The remaining dimensions/threshold knobs
/// fall back to the documented defaults because the canonical
/// pipeline only uses them at the matrix boundary.
fn build_canonical_for_resume_pipeline(manifest: &Manifest) -> Pipeline {
    let target = resume_target_cardinality(
        &MoaganHome::resolve().unwrap_or_else(|_| {
            // Fall back to a tempdir-anchored home if `MOAGAN_HOME`
            // is unset; `run_resume` always overwrites the run_dir
            // via `home.run_dir(run_id)` so this synthetic home is
            // only used for the cardinality probe. Operators running
            // `moagan continue` always have a real `MOAGAN_HOME`.
            let tmp = std::env::temp_dir().join(format!("moagan-resume-{}", manifest.run_id));
            MoaganHome::at(tmp)
        }),
        manifest.run_id,
    );
    let opts = DiscoverOptions {
        provider: manifest.provider.clone(),
        prompt: String::new(),
        home: None,
        mock_dir: None,
        cardinality: target,
        max_parallelism: None,
        dimensions: RESUME_DEFAULT_DIMENSIONS,
        facets_per_dimension: RESUME_DEFAULT_FACETS_PER_DIMENSION,
        cluster_threshold: RESUME_DEFAULT_CLUSTER_THRESHOLD,
        out_dir: None,
        non_interactive: true,
        cache_facets: false,
    };
    build_discovery_pipeline(&opts)
}

/// Re-export of [`load_manifest`] for the dispatcher path; the
/// discovery resume helper reads the manifest exactly the way
/// [`crate::cli::continue_cmd`] does. Kept as a top-level import
/// so the function path stays short for tests.
#[allow(dead_code)]
pub(crate) fn load_manifest_for_resume(home: &MoaganHome, run_id: RunId) -> Result<Manifest> {
    load_manifest(home, run_id)
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
