//! `moagan run` — start a new run, build a pipeline, execute it, write
//! the manifest, and print a summary.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cli::Mode;
use crate::config::Config;
use crate::context::{
    ContextRef, ContextScope, loader as context_loader, resolver as context_resolver,
};
use crate::domain::{LineagePaths, Manifest, ManifestPhase, ManifestUsage};
use crate::error::{Error, Result};
use crate::execution::Parallelism;
use crate::fs_layout::{MoaganHome, RunDir, RunPaths};
use crate::ids::RunId;
use crate::llm::{ProviderRegistry, registry_from_config};
use crate::phases::{
    ClarifyPhase, ClusterProposalsPhase, CritiquePhase, DecomposePhase, DeliverPhase, GatePhase,
    IntakePhase, JudgePhase, Pipeline, ProposePhase, RankPhase, RepairPhase, RoutePhase,
    RunContext, SketchPhase, SynthesizePhase, ValidatePhase,
};
use crate::redact::{self, RedactPolicy};
use crate::secret::SecretString;
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
    /// Phase J: optional reference to an upstream context
    /// (`--context`). Resolved into a `ContextRef` before the
    /// pipeline starts.
    pub context: Option<String>,
    /// Phase J: scope used when loading the context. Defaults to
    /// `Summary`; `SummaryFull` and `Full` are opt-in via the
    /// `--context-summary` / `--context-full` flags.
    pub context_scope: ContextScope,
}

/// Run a moagan pipeline end-to-end. Returns the run id on success.
pub async fn run(opts: RunOptions, cfg: &Config) -> Result<RunId> {
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

    // Open the SQLite index under MOAGAN_HOME/meta.sqlite. The
    // pipeline mirrors every phase event and every LLM call into
    // the DB so `moagan inspect` returns live data.
    let db = Db::open(&home.meta_db_path())?;
    let config_hash = Some(crate::ids::blake3_hex(
        crate::ids::canonical_hash(&[cfg.default_provider.as_str()]).as_bytes(),
    ));

    // Phase J: resolve + load the upstream context (if any) BEFORE
    // registering the run so the SQLite mirror carries the lineage
    // from the start. The filesystem sidecar order matches:
    // `brief.json` -> `manifest.json` -> SQLite index (T01-06 §1.1).
    let loaded_context = match opts.context.as_deref() {
        Some(raw) => {
            let cref = context_resolver::resolve(&home, raw)?;
            let mut loaded = context_loader::load(&home, &cref, opts.context_scope)?;
            // If the loader didn't already attach a parent_run_id
            // record (it does for run_id refs), we synthesise one
            // from the resolved ContextRef so the manifest always
            // has a record for "what kind of ref was this".
            if loaded.context_refs.is_empty() {
                if let ContextRef::RunId(id) = &cref {
                    loaded.parent_run_id = Some(*id);
                }
                let now = crate::time::now_unix_secs();
                loaded.context_refs.push(crate::context::ContextRefRecord {
                    source_path: cref.source(),
                    context_type: cref.kind().to_string(),
                    shasum: crate::ids::blake3_hex(loaded.brief_excerpt.as_bytes()),
                    bytes: loaded.brief_excerpt.len() as u64,
                    added_unix: now,
                });
            }
            Some((cref, loaded))
        }
        None => None,
    };
    let parent_run_id = loaded_context.as_ref().and_then(|(_, l)| l.parent_run_id);
    let shared_brief_hash = loaded_context
        .as_ref()
        .and_then(|(_, l)| l.shared_brief_hash.clone());
    let context_block = loaded_context
        .as_ref()
        .map(|(_, l)| l.brief_excerpt.clone());
    let context_refs = loaded_context
        .as_ref()
        .map(|(_, l)| l.context_refs.clone())
        .unwrap_or_default();

    db.register_run(
        run_id,
        opts.mode.as_str(),
        "running",
        env!("CARGO_PKG_VERSION"),
        config_hash.as_deref(),
        shared_brief_hash.as_deref(),
        parent_run_id,
    )?;
    // Mirror the context refs into SQLite so post-execution
    // queries (e.g. `SELECT * FROM run_context_refs WHERE run_id = ?`)
    // return them without re-reading the sidecar.
    for record in &context_refs {
        if let Err(e) = db.add_context_ref(run_id, record) {
            tracing::warn!(
                run_id = %run_id,
                error = %e,
                stage = "context.add_context_ref.error",
                "failed to mirror context_refs row"
            );
        }
    }

    // Build the lineage_paths block. The `relative` map is empty
    // here; the manifest writer fills it after the pipeline runs
    // (the `final/` and other directories exist on disk by then).
    let lineage_paths = loaded_context.as_ref().map(|(cref, _loaded)| {
        let mut paths = LineagePaths::default();
        paths
            .absolute
            .insert(LineagePaths::LABEL_FINAL_DIR.into(), run_dir.final_dir());
        if let ContextRef::RunId(parent) = cref {
            let parent_dir = home.run_dir(*parent);
            paths.relative.insert(
                LineagePaths::LABEL_PARENT_RUN_DIR.into(),
                format!("../{}", parent),
            );
            paths.absolute.insert(
                LineagePaths::LABEL_PARENT_RUN_DIR.into(),
                parent_dir.root().to_path_buf(),
            );
            let sketches = parent_dir.sketches();
            if sketches.is_dir() {
                paths
                    .absolute
                    .insert(LineagePaths::LABEL_PARENT_SKETCHES.into(), sketches);
            }
        }
        paths
    });

    // Build a minimal manifest stub the pipeline helper can populate
    // (the helper rebuilds it from telemetry after the pipeline
    // finishes; the stub just carries the fields used by the
    // lineage block).
    let default_model = cfg.provider(&default_provider)?.model.clone();

    let stub = Manifest {
        schema_version: "v1".into(),
        run_id,
        mode: opts.mode.as_str().into(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        brief_sha256: String::new(),
        brief_blake3: String::new(),
        provider: default_provider.clone(),
        model: default_model.clone(),
        phases: Vec::new(),
        usage: ManifestUsage::default(),
        manifest_blake3: String::new(),
        parent_run_id,
        shared_brief_hash: shared_brief_hash.clone(),
        context_refs: context_refs.clone(),
        lineage_paths: lineage_paths.clone(),
        cli_prompt: Some(opts.prompt.clone()),
    };

    let final_manifest = run_full_pipeline(
        home.clone(),
        db.clone(),
        cfg,
        opts.mock_dir.clone(),
        opts.non_interactive,
        stub,
        opts.prompt.clone(),
        context_block,
        opts.max_parallelism,
    )
    .await?;

    println!(
        "moagan run {} mode={} provider={} -> {}",
        final_manifest.run_id.short(),
        final_manifest.mode,
        final_manifest.provider,
        run_dir.root().display()
    );
    Ok(final_manifest.run_id)
}

/// Run the full pipeline (intake → deliver) on a fresh run dir.
///
/// Both `moagan run` and `moagan rerun` dispatch through this helper.
/// The caller prepares the manifest stub (with `run_id`, `mode`,
/// `provider`, `model`, `parent_run_id`, `shared_brief_hash`,
/// `context_refs`, `lineage_paths`) and provides the `raw_prompt` +
/// optional `context_block`. The helper:
///
/// 1. Builds the provider registry.
/// 2. Opens telemetry.
/// 3. Builds the `RunContext` (with the context block, lineage, etc.).
/// 4. Builds the canonical pipeline for the mode.
/// 5. Runs the pipeline (with shutdown signal handling).
/// 6. Flushes telemetry.
/// 7. Rebuilds the manifest from telemetry.
/// 8. Writes the manifest.
/// 9. Updates the run status to "completed".
///
/// Returns the rebuilt manifest. The caller is responsible for
/// printing the user-facing success message.
#[allow(clippy::too_many_arguments)]
pub async fn run_full_pipeline(
    home: Arc<MoaganHome>,
    db: Db,
    cfg: &Config,
    mock_dir: Option<PathBuf>,
    non_interactive: bool,
    stub: Manifest,
    raw_prompt: String,
    context_block: Option<String>,
    max_parallelism: Option<usize>,
) -> Result<Manifest> {
    let run_id = stub.run_id;
    let run_dir = home.run_dir(run_id);
    let cfg_arc = Arc::new(cfg.clone());
    let mode = parse_mode(&stub.mode)?;
    let default_provider = if stub.provider.is_empty() {
        cfg.default_provider.clone()
    } else {
        stub.provider.clone()
    };
    let providers = Arc::new(build_registry_for(
        cfg,
        &default_provider,
        mock_dir.as_deref(),
    )?);
    let default_model = cfg.provider(&default_provider)?.model.clone();

    // W1: the redact policy is built from the loaded Config, NOT
    // RedactPolicy::default(). The default has `telemetry: true`,
    // `storage: true`, `export: true`, which matches the privacy-
    // by-default contract — but it also ignored the user's
    // `redact_in_telemetry = false` knob in `config.toml`. Building
    // the policy from `cfg` honours the operator's choice; the
    // other surfaces (storage, export) keep their defaults so a
    // flipped `redact_in_telemetry = false` does NOT leak
    // `manifest.json` or the export bundle.
    let policy = RedactPolicy {
        telemetry: cfg.redact_in_telemetry,
        storage: true,
        export: true,
        prompts: false,
        enabled_patterns: None,
    };
    let telemetry = Telemetry::open(run_id, &run_dir, policy, Some(db.clone()))?;
    let parallelism = Parallelism::new(max_parallelism.unwrap_or(cfg.max_parallelism));

    let ctx = RunContext::new_with_config(
        run_id,
        Arc::clone(&home),
        providers,
        default_provider.clone(),
        default_model.clone(),
        parallelism,
        telemetry.clone(),
        raw_prompt,
        stub.mode.clone(),
        cfg_arc,
    )
    .with_timeouts(cfg.phase_timeout_secs, cfg.total_timeout_secs)
    // V4 §13.6 promises "no human pauses" for Mode::Batch. The
    // `interactive` flag now reflects that contract: even if the
    // operator forgets `--non-interactive`, batch runs skip every
    // human checkpoint and persist a `<skipped:non_interactive>`
    // marker for the audit trail. `--non-interactive` (any mode)
    // keeps the existing behaviour.
    .with_interactive(!non_interactive && !matches!(mode, Mode::Batch))
    .with_context(
        context_block,
        stub.parent_run_id,
        stub.shared_brief_hash.clone(),
        stub.context_refs.clone(),
        stub.lineage_paths.clone(),
    );

    // Phase F: synthesis-replacement predicate is ON by default for
    // every mode that runs SynthesizePhase (`standard`/`deep`/`batch`).
    // `fast` never runs synthesis so the flag is a no-op there.
    let replace_sources_enabled = !matches!(mode, Mode::Fast);
    let pipeline = build_pipeline_for_mode(mode, cfg, replace_sources_enabled);

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

    let mut manifest = build_manifest(
        &run_id,
        stub.mode.as_str(),
        "completed",
        &home,
        &run_dir,
        &default_provider,
        default_model.as_str(),
    )?;
    // Preserve the lineage block the caller prepared. The builder
    // defaults everything to empty; we re-attach the parent_run_id,
    // shared_brief_hash, context_refs, and lineage_paths so they
    // round-trip through the post-pipeline rebuild. For
    // `lineage_paths` we keep the J-supplied value when the caller
    // built one (e.g. `--context <parent_run_id>` populated the
    // `parent_run_dir` label); otherwise we fall back to the
    // `RunPaths::resolve(...)` catalog that `build_manifest` set up,
    // so every run ships with a typed table of its own well-known
    // paths even when no context was loaded (sub-fase M, D.12.16).
    manifest.parent_run_id = stub.parent_run_id;
    manifest.shared_brief_hash = stub.shared_brief_hash.clone();
    manifest.context_refs = stub.context_refs.clone();
    if stub.lineage_paths.is_some() {
        manifest.lineage_paths = stub.lineage_paths.clone();
    }
    manifest.cli_prompt = stub.cli_prompt.clone();
    // Redact the verbatim CLI prompt before it lands on disk. The
    // default policy redacts on Storage; the pattern catalog covers
    // API keys (sk-cp-, sk-, ghp_, …), JWTs, Bearer headers, and
    // generic password/secret= assignments. Users routinely paste
    // real keys into prompts; persisting them unredacted on the
    // manifest sidecar defeats every other privacy control in the
    // pipeline (calls.jsonl.gz, intake.json, etc. all redact).
    if let Some(p) = manifest.cli_prompt.as_ref()
        && let Ok(redacted) = redact::apply(
            &redact::RedactPolicy::default(),
            redact::Surface::Storage,
            p,
        )
    {
        manifest.cli_prompt = Some(redacted.into_owned());
    }
    let manifest_json = serde_json::to_vec_pretty(&manifest).map_err(Error::from)?;
    crate::atomic::writer::AtomicWriter::new().write(&run_dir.manifest(), &manifest_json)?;
    if let Err(e) = db.update_run_status(run_id, "completed") {
        eprintln!("warn: failed to update run status: {e}");
    }
    Ok(manifest)
}

/// Parse the manifest's mode string into the `Mode` enum. Mirrors
/// the span from `super::continue_cmd::parse_mode`; the canonical
/// home is here because the pipeline builder is the consumer.
pub(crate) fn parse_mode(s: &str) -> Result<Mode> {
    match s {
        "fast" => Ok(Mode::Fast),
        "standard" => Ok(Mode::Standard),
        "deep" => Ok(Mode::Deep),
        "explore" => Ok(Mode::Explore),
        "batch" => Ok(Mode::Batch),
        other => Err(Error::InvalidState(format!("unknown mode {other:?}"))),
    }
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
    build_registry_for_with_api_key(cfg, selected, mock_dir, None)
}

pub(crate) fn build_registry_for_with_api_key(
    cfg: &Config,
    selected: &str,
    mock_dir: Option<&std::path::Path>,
    api_key: Option<&str>,
) -> Result<ProviderRegistry> {
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
    if spec.kind == "minimax"
        && let Some(key) = api_key
    {
        let provider =
            crate::llm::minimax::MinimaxProvider::new(&spec, SecretString::new(key.to_owned()))?;
        let mut reg = ProviderRegistry::default();
        reg.insert(selected.to_owned(), Arc::new(provider));
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
pub fn build_pipeline_for_mode(
    mode: Mode,
    cfg: &Config,
    replace_sources_enabled: bool,
) -> Pipeline {
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

pub(crate) fn build_manifest(
    run_id: &RunId,
    mode: &str,
    status: &str,
    home: &MoaganHome,
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
        parent_run_id: None,
        shared_brief_hash: None,
        context_refs: Vec::new(),
        lineage_paths: Some(LineagePaths::from_run_paths(&RunPaths::resolve(
            home, *run_id,
        ))),
        cli_prompt: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::{RedactPolicy, Surface};

    /// Direct test of the redaction policy that protects
    /// `manifest.cli_prompt`. The production path (run_full_pipeline
    /// → serde_json → AtomicWriter) is covered by the integration
    /// smoke; this unit test pins the policy choice so a future
    /// refactor cannot silently turn it off.
    #[test]
    fn cli_prompt_redaction_replaces_api_keys() {
        let policy = RedactPolicy::default();
        let raw = "test-secret-sk-cp-zNY4VDNCchb7_Cv4Hx2I8Y6cW6gDel1Mw3ObZPw";
        let redacted = redact::apply(&policy, Surface::Storage, raw).expect("redaction succeeds");
        assert!(
            !redacted.contains("zNY4VDNCchb7"),
            "raw API key leaked: {redacted}"
        );
        assert!(
            redacted.contains("[REDACTED:") || redacted.contains("***REDACTED"),
            "redaction marker missing: {redacted}"
        );
    }

    #[test]
    fn cli_prompt_redaction_replaces_bearer_headers() {
        let policy = RedactPolicy::default();
        let raw = "Authorization: Bearer abcdefghij1234567890";
        let redacted = redact::apply(&policy, Surface::Storage, raw).expect("redaction succeeds");
        assert!(
            !redacted.contains("abcdefghij1234567890"),
            "raw bearer token leaked: {redacted}"
        );
    }

    #[test]
    fn cli_prompt_redaction_preserves_non_secret_text() {
        let policy = RedactPolicy::default();
        let raw = "design a CLI for batch CSV processing";
        let redacted = redact::apply(&policy, Surface::Storage, raw).expect("redaction succeeds");
        assert_eq!(redacted, raw, "non-secret text should be unchanged");
    }
}
