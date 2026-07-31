//! `moagan run` — start a new run, build a pipeline, execute it, write
//! the manifest, and print a summary.

use std::path::Path;
use std::sync::Arc;

use crate::cli::Mode;
use crate::config::Config;
use crate::domain::{Manifest, ManifestPhase, ManifestUsage};
use crate::error::{Error, Result};
use crate::execution::Parallelism;
use crate::fs_layout::{MoaganHome, RunDir};
use crate::ids::RunId;
use crate::llm::{ProviderRegistry, registry_from_config};
use crate::phases::{
    ClarifyPhase, ClusterProposalsPhase, CritiquePhase, DecomposePhase, DeliverPhase, GatePhase,
    IntakePhase, JudgePhase, Pipeline, ProposePhase, RankPhase, RepairPhase, RoutePhase,
    SketchPhase, SynthesizePhase, ValidatePhase,
};
use crate::redact::RedactPolicy;
use crate::storage::sqlite::Db;
use crate::telemetry::{PhaseEvent, Telemetry};

/// Options for `moagan run`.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Pipeline mode (closed enum: `fast` or `standard`).
    pub mode: Mode,
    /// Provider name (must be in config).
    pub provider: String,
    /// User prompt.
    pub prompt: String,
    /// Optional override of the home directory.
    pub home: Option<std::path::PathBuf>,
    /// Optional directory of canned mock responses. Only used when
    /// `provider` resolves to a `mock` kind; ignored otherwise.
    pub mock_dir: Option<std::path::PathBuf>,
    /// Whether to be non-interactive (no prompts).
    pub non_interactive: bool,
    /// Override the global cap on concurrent LLM calls. When `None`
    /// the config-file value (`cfg.max_parallelism`, default 4) is
    /// used. The constructor (`Parallelism::new`) clamps to `>= 1`.
    pub max_parallelism: Option<usize>,
    /// Phase F: opt-out of the synthesis-replacement predicate. When
    /// `true`, the synthesis and its sources all stay in the ranking
    /// (V4 §5.13 "no sustituye automáticamente"). Default `false`
    /// (replacement ON for `standard`/`deep`/`batch`).
    pub no_replace_sources: bool,
}

/// Run a moagan pipeline end-to-end. Returns the run id on success.
pub async fn run(opts: RunOptions, cfg: &Config) -> Result<RunId> {
    if let Some(ref home) = opts.home {
        unsafe {
            std::env::set_var("MOAGAN_HOME", home);
        }
    }
    let home = Arc::new(MoaganHome::resolve()?);
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
    let default_model_for_manifest = default_model.clone();
    // Open the SQLite index under MOAGAN_HOME/meta.sqlite. The
    // pipeline mirrors every phase event and every LLM call into
    // the DB so `moagan inspect` returns live data.
    let db = Db::open(&home.meta_db_path())?;
    let config_hash = Some(crate::ids::blake3_hex(
        crate::ids::canonical_hash(&[cfg.default_provider.as_str()]).as_bytes(),
    ));
    db.register_run(
        run_id,
        opts.mode.as_str(),
        "running",
        env!("CARGO_PKG_VERSION"),
        config_hash.as_deref(),
        None,
        None,
    )?;
    let telemetry = Telemetry::open(run_id, &run_dir, policy, Some(db.clone()))?;
    // CLI `--max-parallelism` overrides the config-file value.
    // When neither is set the constructor falls back to 4 inside
    // `cfg::Config::default`; `Parallelism::new` clamps to >= 1.
    let max_parallelism = opts.max_parallelism.unwrap_or(cfg.max_parallelism);
    let parallelism = Parallelism::new(max_parallelism);

    let ctx = crate::phases::RunContext::new(
        run_id,
        Arc::clone(&home),
        Arc::clone(&providers),
        default_provider.clone(),
        default_model,
        parallelism,
        telemetry.clone(),
        opts.prompt.clone(),
        opts.mode.as_str().to_owned(),
    )
    .with_timeouts(cfg.phase_timeout_secs, cfg.total_timeout_secs)
    .with_interactive(!opts.non_interactive);

    // Phase F: synthesis-replacement predicate is ON by default for
    // every mode that runs SynthesizePhase (`standard`/`deep`/`batch`).
    // `fast` never runs synthesis so the flag would be a no-op there.
    // The CLI opt-out (`--no-replace-sources`) flips it for the
    // current run regardless of mode.
    let replace_sources_enabled = !opts.no_replace_sources && !matches!(opts.mode, Mode::Fast);

    let pipeline = build_pipeline_for_mode(opts.mode, cfg, replace_sources_enabled);
    let pipeline_future = pipeline.run(&ctx);
    tokio::pin!(pipeline_future);
    let _outputs = tokio::select! {
        result = &mut pipeline_future => result?,
        signal = shutdown_signal() => {
            signal?;
            ctx.cancel().cancel(crate::cancel::CancelReason::UserInterrupt);
            return Err(ctx.cancel().into_error());
        }
    };

    // Flush telemetry before the manifest reads phases/calls.
    // Without this, the gzip stream is incomplete (no CRC/length
    // trailer) and `MultiGzDecoder` returns `UnexpectedEof`,
    // silently leaving the manifest with empty `phases`/`usage`.
    telemetry.flush()?;

    let manifest = build_manifest(
        &run_id,
        opts.mode.as_str(),
        "completed",
        &run_dir,
        &default_provider,
        default_model_for_manifest.as_str(),
    )?;
    let manifest_json = serde_json::to_vec_pretty(&manifest).map_err(Error::from)?;
    crate::atomic::writer::AtomicWriter::new().write(&run_dir.manifest(), &manifest_json)?;
    if let Err(e) = db.update_run_status(run_id, "completed") {
        eprintln!("warn: failed to update run status: {e}");
    }
    println!(
        "moagan run {} mode={} provider={} -> {}",
        run_id.short(),
        opts.mode.as_str(),
        default_provider,
        run_dir.root().display()
    );
    Ok(run_id)
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(())
    }
}

/// Build a `ProviderRegistry` containing only `selected` (the provider
/// the user asked for). Reused by `continue_cmd::run_refine` and
/// `run_rerank` so those flows get a real provider and not an empty
/// registry that panics on the first `RunContext::provider()` call.
pub fn build_registry_for(
    cfg: &Config,
    selected: &str,
    mock_dir: Option<&std::path::Path>,
) -> Result<ProviderRegistry> {
    // Build a registry containing only the selected provider. The full
    // cfg may declare other providers whose `from_config` requires an
    // API key; we must not construct them when the user did not ask
    // for them.
    let spec = cfg
        .providers
        .get(selected)
        .ok_or_else(|| Error::InvalidArgs(format!("provider '{selected}' is not in config")))?
        .clone();
    if spec.kind == "mock"
        && let Some(dir) = mock_dir
    {
        let mock = crate::llm::MockProvider::from_dir(dir)?;
        let mut reg = ProviderRegistry::default();
        reg.insert(selected.to_owned(), Arc::new(mock));
        return Ok(reg);
    }
    let mut spec_map = std::collections::BTreeMap::new();
    spec_map.insert(selected.to_owned(), spec);
    registry_from_config(&spec_map)
}

/// Build the canonical pipeline for a given mode. The cardinality
/// table mirrors the v0.1 MVP in `docs/proposal-01-concept.md` §13.6
/// and §5.3 of `docs/proposal-02-rust.md` for the v0.2 additions:
///
/// - `fast`: 3 proposals, 2 critics, 3 judges. No sketch phase.
///   Cluster/synthesize/adversary skipped to keep the loop short.
/// - `standard`: 3 proposals, 3 critics, 5 judges, 4 sketches.
///   Phase D enabled.
/// - `deep`: 5 proposals, 4 critics, 7 judges, 6 sketches,
///   2 repair rounds, Phase D enabled (synthesis + adversary).
/// - `explore`: 0 proposals, 0 critics, 0 judges, 12 sketches.
///   Pipeline ends at sketches; the user inspects the sketch map
///   manually.
/// - `batch`: 3 proposals, 2 critics, 3 judges, 4 sketches.
///   Mirrors fast cardinality plus sketches; differs in its
///   JSON-stable output contract and lack of human pauses. Phase D
///   enabled but the human checkpoints are auto-skipped (handled
///   inside the phases via `CheckpointOpts::interactive`).
///
/// Phase F (`replace_sources_enabled` flag): the synthesis-replacement
/// predicate is wired into `RankPhase` for every mode that runs
/// `SynthesizePhase`. `fast` skips synthesis entirely so the flag is
/// off there; the rest default to ON. The CLI flag
/// `--no-replace-sources` overrides the per-mode default.
fn build_pipeline_for_mode(mode: Mode, cfg: &Config, replace_sources_enabled: bool) -> Pipeline {
    let (proposals, critics, judges, sketches) = match mode {
        Mode::Fast => (3u32, 2u32, 3u32, 0u32),
        Mode::Standard => (3u32, 3u32, 5u32, 4u32),
        Mode::Deep => (5u32, 4u32, 7u32, 6u32),
        Mode::Explore => (0u32, 0u32, 0u32, 12u32),
        Mode::Batch => (3u32, 2u32, 3u32, 4u32),
    };
    let cfg_arc = std::sync::Arc::new(cfg.clone());
    let mut pipeline = Pipeline::new()
        .push(IntakePhase)
        .push(ClarifyPhase)
        .push(RoutePhase);

    // Phase G (V4 §5.3 + T01-06 §8.1 step 3 + §16.4): the
    // `DecomposePhase` only runs in `deep` mode. It is a no-op for
    // every other mode (the wiring is conditional here, not inside
    // the phase) so non-deep runs never pay the cost of an extra
    // pipeline node. The phase itself short-circuits to a trivial
    // `ProblemGraph` when the brief does not meet the trigger
    // ladder.
    if mode == Mode::Deep {
        pipeline = pipeline.push(DecomposePhase);
    }

    // SketchPhase runs after Route whenever the mode says so. When
    // `count == 0` the phase short-circuits to an empty
    // `PhaseOutput::Sketches`, but we still insert it so the
    // manifest's phase list reflects the intended shape.
    if mode.runs_sketches() {
        pipeline = pipeline.push(SketchPhase { count: sketches });
    }
    // `explore` ends at sketches — no proposals, no judging. The user
    // inspects the sketch map manually (see final/sketches_summary.json
    // and sketches/sk_*.json). Inserting the downstream phases would
    // crash deliver with "no proposals to portfolio".
    if mode == Mode::Explore {
        return pipeline;
    }
    pipeline = pipeline.push(ProposePhase { count: proposals });

    // The Validate phase runs the executable validator suite
    // (structural + constraints + language validators) for every
    // mode that produces full proposals AND has the budget to
    // afford the extra sandbox invocation. `fast` stays fast
    // because the structural checks live entirely inside Gate
    // already; `explore` ends at sketches and never reaches this
    // branch. `standard`, `deep`, and `batch` get it so proposals
    // carrying code snippets can be type-checked / compiled
    // before the gate phase decides which proposals advance.
    // Compliance with V4 §5.8 + §13.6.
    if matches!(mode, Mode::Standard | Mode::Deep | Mode::Batch) {
        pipeline = pipeline.push(ValidatePhase::new());
    }

    // Phase D wiring (V4 §5.13 + §13.6):
    // - `ClusterProposalsPhase` runs after critique (which has the
    //   most up-to-date repaired proposal as input).
    // - `SynthesizePhase` runs after clustering and before judging
    //   so the rank phase can fold the synthesized proposal into
    //   the same ranking. The synthesized proposal competes with
    //   its sources per §5.13.
    // - The adversary pass is a conditional branch inside
    //   `JudgePhase` so it stays out of the pipeline vector.
    // - `fast` skips both: the loop is meant to stay fast.
    if !matches!(mode, Mode::Fast) {
        pipeline = pipeline
            .push(ClusterProposalsPhase::default())
            .push(SynthesizePhase::default());
    }

    pipeline
        .push(GatePhase)
        .push(CritiquePhase {
            critics_per_proposal: critics,
        })
        .push(RepairPhase::from_config(cfg))
        .push(JudgePhase {
            judges,
            ..JudgePhase::default()
        })
        .push(RankPhase {
            config: cfg_arc.clone(),
            replace_sources_enabled,
            stability_enabled: cfg_arc.stability.enabled,
        })
        .push(DeliverPhase)
}

fn build_manifest(
    run_id: &RunId,
    mode: &str,
    status: &str,
    run_dir: &RunDir<'_>,
    provider: &str,
    model: &str,
) -> Result<Manifest> {
    use chrono::Utc;
    let now = Utc::now();

    // 1. Compute the brief hashes from the on-disk canonical brief.
    let (brief_sha256, brief_blake3) = match std::fs::read(run_dir.brief()) {
        Ok(bytes) => {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let sha = hex::encode(hasher.finalize());
            let blake = blake3::hash(&bytes).to_hex().to_string();
            (sha, blake)
        }
        Err(_) => (String::new(), String::new()),
    };

    // 2. Aggregate phase events from telemetry/phases.jsonl.gz into one
    //    ManifestPhase per phase name. Start events set started_unix;
    //    end events set ended_unix + status; error events set error.
    //    `read_to_string` auto-detects the `.gz` suffix and falls back
    //    to plain `.jsonl` for runs produced before compression was
    //    wired (legacy readers).
    let phases_path = run_dir.telemetry().join("phases.jsonl.gz");
    let legacy_phases_path = run_dir.telemetry().join("phases.jsonl");
    let phase_events = read_phase_events(&phases_path, &legacy_phases_path);
    let calls_path = run_dir.telemetry().join("calls.jsonl.gz");
    let legacy_calls_path = run_dir.telemetry().join("calls.jsonl");
    let call_counts = count_calls_per_phase(&calls_path, &legacy_calls_path);
    let manifest_phases = aggregate_phase_events(&phase_events, &call_counts);

    // 3. Aggregate usage from telemetry/calls.jsonl[.gz].
    let usage = aggregate_usage(&calls_path, &legacy_calls_path);

    let mut manifest = Manifest {
        schema_version: "v1".into(),
        run_id: *run_id,
        mode: mode.into(),
        status: status.into(),
        created_at: now,
        updated_at: now,
        client_version: env!("CARGO_PKG_VERSION").into(),
        brief_sha256,
        brief_blake3,
        provider: provider.into(),
        model: model.into(),
        phases: manifest_phases,
        usage,
        manifest_blake3: String::new(),
    };

    // 4. Compute the self-hash over the canonical JSON with
    //    `manifest_blake3` set to the empty string. The hash is then
    //    filled into the manifest, so consumers can verify the
    //    contents by re-hashing with the field blanked.
    let mut canonical = manifest.clone();
    canonical.manifest_blake3 = String::new();
    let json = serde_json::to_vec(&canonical).map_err(Error::from)?;
    let hash = blake3::hash(&json).to_hex().to_string();
    manifest.manifest_blake3 = hash;

    Ok(manifest)
}

/// Read every `phases.jsonl[.gz]` line into a `PhaseEvent` list.
/// Tries the gzipped path first (current default), then the legacy
/// plain path (runs produced before compression was wired). Missing
/// files and individual malformed lines are silently skipped so a
/// partial telemetry stream never blocks manifest emission.
fn read_phase_events(primary: &Path, legacy: &Path) -> Vec<PhaseEvent> {
    let raw = read_telemetry_text(primary, legacy);
    if raw.is_empty() {
        return Vec::new();
    }
    raw.lines()
        .filter_map(|line| serde_json::from_str::<PhaseEvent>(line).ok())
        .collect()
}

/// Resolve a telemetry text stream. Prefers the primary path (the
/// current spec default, e.g. `phases.jsonl.gz`); falls back to the
/// legacy plain path. Returns an empty string if neither exists.
fn read_telemetry_text(primary: &Path, legacy: &Path) -> String {
    match crate::storage::compression::read_to_string(primary) {
        Ok(s) => s,
        Err(_) => crate::storage::compression::read_to_string(legacy).unwrap_or_default(),
    }
}

fn aggregate_phase_events(
    events: &[PhaseEvent],
    call_counts: &std::collections::BTreeMap<String, u32>,
) -> Vec<ManifestPhase> {
    use std::collections::BTreeMap;
    let mut by_name: BTreeMap<String, ManifestPhase> = BTreeMap::new();
    for ev in events {
        let phase_name = ev.phase.clone();
        let calls = call_counts.get(&phase_name).copied().unwrap_or(0);
        let entry = by_name
            .entry(phase_name.clone())
            .or_insert_with(|| ManifestPhase {
                phase: phase_name.clone(),
                started_unix: ev.at_unix,
                ended_unix: 0,
                status: "running".into(),
                calls,
                error: None,
            });
        match ev.status.as_str() {
            "start" => {
                entry.started_unix = ev.at_unix;
            }
            "end" => {
                entry.ended_unix = ev.at_unix;
                entry.status = "end".into();
            }
            "error" => {
                entry.ended_unix = ev.at_unix;
                entry.status = "error".into();
                entry.error = ev.error.clone();
            }
            _ => {}
        }
    }
    by_name.into_values().collect()
}

/// Walk every `calls.jsonl[.gz]` line and count how many calls landed
/// in each phase name. Used to populate `ManifestPhase.calls`.
fn count_calls_per_phase(primary: &Path, legacy: &Path) -> std::collections::BTreeMap<String, u32> {
    use std::collections::BTreeMap;
    let raw = read_telemetry_text(primary, legacy);
    if raw.is_empty() {
        return BTreeMap::new();
    }
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for line in raw.lines() {
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(phase) = ev.get("phase").and_then(|v| v.as_str()) {
            *counts.entry(phase.to_owned()).or_default() += 1;
        }
    }
    counts
}

/// Sum input / output / cache tokens from every recorded call.
fn aggregate_usage(primary: &Path, legacy: &Path) -> ManifestUsage {
    let raw = read_telemetry_text(primary, legacy);
    if raw.is_empty() {
        return ManifestUsage::default();
    }
    let mut usage = ManifestUsage::default();
    for line in raw.lines() {
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        usage.input_tokens += ev.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        usage.output_tokens += ev
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        usage.cache_read += ev.get("cache_read").and_then(|v| v.as_u64()).unwrap_or(0);
        usage.cache_creation += ev
            .get("cache_creation")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
    }
    usage
}
