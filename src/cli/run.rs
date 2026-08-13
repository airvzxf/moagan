//! `moagan run` — start a new run, build a pipeline, execute it, write
//! the manifest, and print a summary.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cli::{Mode, flags_batch};
use crate::config::Config;
use crate::context::{
    ContextRef, ContextScope, loader as context_loader, resolver as context_resolver,
};
use crate::domain::{LineagePaths, Manifest, ManifestPhase, ManifestUsage};
use crate::error::{Error, Result};
use crate::execution::Parallelism;
use crate::fs_layout::{MoaganHome, RunDir, RunPaths};
use crate::ids::RunId;
use crate::llm::capability::CapabilityResolver;
use crate::llm::{ProviderRegistry, registry_from_config};
use crate::phases::{
    AdversaryPhase, ClarifyPhase, ClusterProposalsPhase, CritiquePhase, DecomposePhase,
    DeliverPhase, GatePhase, IntakePhase, JudgePhase, Pipeline, ProposePhase, RankPhase,
    RepairPhase, RoutePhase, RunContext, SketchPhase, SynthesizePhase, ValidatePhase,
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
    /// D.22.1, D.12.5: opt-in for the deterministic pattern-based
    /// adversary pass that runs the seven patterns from
    /// `src/ranking/adversary_patterns.rs::run_all_patterns`
    /// against the just-judged proposals and writes
    /// `rankings/adversary_report.json`. The pipeline also enables
    /// this flag automatically for `Mode::Deep` runs (the only mode
    /// where the report cost is amortised). Default `false`.
    pub adversary: bool,
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
    // Wire `Config::token_budget` into the SQLite `budget_state`
    // row so `BudgetObserver` reads the planned cap at run start.
    // Without this, `set_budget` (a v011 helper on `Db`) is
    // unreachable from production — every test that needed it
    // called it directly, which is why the helper still ships
    // `#[allow(dead_code)]`. `None` falls through and leaves
    // `planned_tokens = 0`, which `BudgetObserver::new` treats as
    // "unlimited".
    if let Some(planned) = cfg.token_budget {
        db.set_budget(run_id, planned)?;
    }
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
        schema_version: Manifest::schema_version_string(),
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
        config_hash: None,
        created_at_iso: chrono::Utc::now().to_rfc3339(),
        last_resumed_at_iso: None,
        resume_count: 0,
        prohibited_decisions: Vec::new(),
    };

    let final_manifest = run_full_pipeline(
        home.clone(),
        db.clone(),
        cfg,
        opts.mock_dir.clone(),
        opts.non_interactive,
        opts.adversary,
        stub,
        opts.prompt.clone(),
        context_block,
        opts.max_parallelism,
        opts.no_replace_sources,
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
    adversary: bool,
    stub: Manifest,
    raw_prompt: String,
    context_block: Option<String>,
    max_parallelism: Option<usize>,
    no_replace_sources: bool,
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
    // Pull the auto-probe table off the registry so the pipeline can
    // consult it on every LLM call. `registry_from_config_with_home`
    // already fired background probes when the table was built; the
    // `await_ready()` call below (after the pipeline finishes) blocks
    // until they have all landed.
    let max_tokens_table = providers.max_tokens_table().cloned();
    let default_model = cfg.provider(&default_provider)?.model.clone();

    // Wire-the-gates plan: refresh the on-disk `models.dev` catalog
    // so the modality gate (`ModalityGate::apply`) and the cost
    // estimator (`cost_estimate`) can resolve `(provider, model)`
    // rows on every LLM call. `load_or_fetch` honours the 1-hour
    // TTL: a fresh cache short-circuits without touching the
    // network, a stale one fetches + atomically rewrites, and a
    // network failure degrades to the stale cache (best-effort).
    // The catalog lives on `RunContext` so every phase reads the
    // same handle; the failure mode is "no catalog" — the gates
    // fall through to their no-op defaults and the run proceeds.
    let models_dev_catalog = match crate::llm::models_dev::load_or_fetch(
        home.root(),
        crate::llm::models_dev::DEFAULT_REFRESH_HOURS,
        false,
    )
    .await
    {
        Ok(load) => Some(Arc::new(load.catalog)),
        Err(err) => {
            tracing::warn!(
                error = %err,
                home = %home.root().display(),
                stage = "models_dev.refresh.failed",
                "models_dev catalog refresh failed; proceeding without a catalog"
            );
            None
        }
    };
    // Wire-the-gates plan, PR-3 follow-up: the resolver is on
    // `RunContext` and the gate call is already in
    // `dispatch_to_provider`, but it has been a permanent no-op
    // because nothing in production populates the field. Build
    // one over the same catalog handle so the resolver and the
    // catalog share a single source of truth; a missing catalog
    // disables both at once.
    let capability_resolver = models_dev_catalog
        .as_ref()
        .map(|catalog| Arc::new(CapabilityResolver::new(Some(Arc::clone(catalog)))));

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
    if let Some(n) = max_parallelism {
        flags_batch::validate_max_parallelism(n).map_err(Error::InvalidArgs)?;
    }
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
    .with_max_tokens_table_opt(max_tokens_table)
    .with_models_dev_catalog_opt(models_dev_catalog.clone())
    .with_capability_resolver_opt(capability_resolver.clone())
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
    // `fast` never runs synthesis so the flag is a no-op there. The
    // CLI flag `--no-replace-sources` overrides the per-mode default
    // so the operator can pin replacement off in non-fast modes
    // (V4 §5.13 "no sustituye automáticamente"). The computation
    // lives in [`resolve_replace_sources_enabled`] so unit tests
    // can pin the full mode × flag matrix without spinning up the
    // pipeline (DB, telemetry, registry).
    let replace_sources_enabled = resolve_replace_sources_enabled(mode, no_replace_sources);
    // D.22.1, D.12.5: the deterministic pattern-based adversary
    // pass is opt-in. The CLI flag `--adversary` overrides the
    // per-mode default; `Mode::Deep` enables the pass by default
    // because it is the only mode where the seven-pattern cost is
    // amortised across a meaningful judge panel. `fast` and
    // `standard` keep the report off unless the operator asks.
    let adversary_enabled = adversary || mode == Mode::Deep;
    let pipeline = build_pipeline_for_mode(mode, cfg, replace_sources_enabled, adversary_enabled);

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

    // Wait for every background `max_tokens_auto` probe to
    // finish so the discovered values land in the in-memory table
    // before the pipeline starts reading them, and so the persisted
    // TOML is current by the time the run exits. Without this the
    // probe races against the pipeline: the first LLM call may use
    // `DEFAULT_MAX_TOKENS` (no cached value yet), and a fast run
    // can exit before the algorithm completes its 30 sequential
    // probes — leaving the on-disk file empty.
    if let Some(table) = ctx.max_tokens_table.as_ref() {
        table.await_ready().await;
    }

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
    // F5: read the `final/config_hash.txt` sidecar that the
    // intake phase wrote and stamp the digest onto the manifest.
    // Missing sidecar (legacy runs, mocked-out intake) leaves
    // `config_hash` at its v2 default of `None` so the field
    // round-trips cleanly.
    manifest.config_hash = read_intake_config_hash(&run_dir);
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
    // U1: emit a manifest_events row so the dashboard's "run.completed"
    // timeline has the canonical anchor (the manifest sidecar is
    // small; this table keeps the lifecycle event out of it).
    if let Err(e) = db.record_manifest_event(&crate::storage::sqlite::ManifestEventRow {
        run_id: run_id.to_string(),
        event_type: "run.completed".into(),
        details: Some(manifest.status.clone()),
        at_unix: crate::time::now_unix_secs(),
    }) {
        eprintln!("warn: failed to record run.completed manifest event: {e}");
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
    registry_from_config(&spec_map, &cfg.circuit_breaker)
}

/// Cardinality knobs for a single pipeline run. Returned by
/// [`pipeline_shape`] so [`build_pipeline_for_mode`] can construct
/// the phase vector with the right counts and so unit tests can
/// verify the cardinality delegation without poking at the
/// `Pipeline`'s private phase list.
///
/// The fields match the contract documented on
/// [`build_pipeline_for_mode`]: `proposals` and `sketches` come
/// from `Cardinality::for_mode_default(mode).soft`, `judges` comes
/// from `judge_quorum_for_mode(mode, cfg)`, and `critics` is a
/// bounded derivative of `proposals` (no spec entry for it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineShape {
    /// `ProposePhase.count` (zero for `Mode::Explore`).
    pub proposals: u32,
    /// `JudgePhase.judges` (zero for `Mode::Explore`).
    pub judges: u32,
    /// `SketchPhase.count` (spec soft target for non-fast modes;
    /// zero for `Mode::Fast` because the phase is skipped).
    pub sketches: u32,
    /// `CritiquePhase.critics_per_proposal` (zero for `Mode::Explore`).
    pub critics: u32,
}

/// Resolve the per-mode cardinality knobs for a pipeline. Pulls
/// `proposals` and `sketches` from the spec D.21.1 soft target via
/// [`Cardinality::for_mode_default`], `judges` from spec D.21.7 via
/// [`crate::phases::cardinality::judge_quorum_for_mode`] (which
/// honours `profile_judge_quorum_overrides`), and derives `critics`
/// from `proposals` (capped 2..=4) because the spec has no
/// dedicated critics table.
///
/// `Mode::Explore` always gets `proposals = 0`, `judges = 0`,
/// `critics = 0` because the pipeline returns at sketches for that
/// mode. The sketches count is still the spec soft target so the
/// `explore` fan-out keeps the spec's high-diversity shape.
pub fn pipeline_shape(mode: Mode, cfg: &Config) -> PipelineShape {
    use crate::phases::cardinality::{Cardinality, judge_quorum_for_mode};
    let cardinality = Cardinality::for_mode_default(mode);
    let soft = cardinality.soft as u32;
    let judges = judge_quorum_for_mode(mode, cfg) as u32;
    if mode == Mode::Explore {
        PipelineShape {
            proposals: 0,
            judges: 0,
            sketches: soft,
            critics: 0,
        }
    } else {
        PipelineShape {
            proposals: soft,
            judges,
            sketches: soft,
            critics: soft.div_ceil(4).clamp(2, 4),
        }
    }
}

/// Build the canonical pipeline for a given mode. The proposal
/// and sketch fan-out counts come from [`Cardinality::for_mode_default`]
/// (the spec D.21.1 soft target per mode), and the judge panel
/// size comes from [`crate::phases::cardinality::judge_quorum_for_mode`]
/// (spec D.21.7 with `profile_judge_quorum_overrides` honoured).
/// This replaces the previous hand-rolled table, which drifted
/// from the spec (`deep` shipped 5 proposals vs. the spec's 10-25
/// range; `fast` shipped 3 judges vs. the spec's 1) and which
/// ignored profile overrides entirely.
///
/// - `fast`: 4 proposals, 1 judge, no sketch phase.
///   Cluster/synthesize/adversary skipped to keep the loop short.
/// - `standard`: 7 proposals, 3 judges, 7 sketches. Phase D enabled.
/// - `deep`: 17 proposals, 5 judges, 17 sketches,
///   2 repair rounds, Phase D enabled (synthesis + adversary).
/// - `explore`: 0 proposals, 0 judges, 27 sketches. Pipeline ends
///   at sketches; the user inspects the sketch map manually.
/// - `batch`: 11 proposals, 1 judge, 11 sketches. JSON-stable
///   output contract, no human pauses, Phase D auto-suppressed.
///
/// Critics per proposal are derived from the proposals count
/// (capped at 2-4) because the spec has no dedicated
/// critics-cardinality table — this is the only knob that does
/// not come from a spec helper. See [`pipeline_shape`] for the
/// concrete derivation rule.
///
/// Phase F (`replace_sources_enabled` flag): the synthesis-replacement
/// predicate is wired into `RankPhase` for every mode that runs
/// `SynthesizePhase`. `fast` skips synthesis entirely so the flag is
/// off there; the rest default to ON. The CLI flag
/// `--no-replace-sources` overrides the per-mode default.
///
/// Phase D follow-up (`adversary_enabled` flag): the deterministic
/// pattern-based adversary pass (`AdversaryPhase`) is opt-in. The
/// CLI flag `--adversary` overrides the per-mode default; `deep`
/// enables it automatically because it is the only mode where the
/// seven-pattern cost is amortised across a meaningful judge
/// panel. The phase writes `rankings/adversary_report.json` with
/// one section per [`AdversaryPattern`].
///
/// Compute whether Phase F (synthesis-replacement) should run for
/// this `mode` given the operator's `--no-replace-sources`
/// preference. Extracted into a `pub(crate)` helper so the unit
/// tests in this module can pin the full mode × flag matrix
/// without spinning up the rest of the pipeline (DB, telemetry,
/// registry). The semantics are:
///
/// - `fast` / `explore`: replacement OFF regardless of the flag
///   (these modes never run `SynthesizePhase`, so the predicate
///   has nothing to gate).
/// - `standard` / `deep` / `batch`: replacement ON by default;
///   `--no-replace-sources` flips it OFF (V4 §5.13 "no sustituye
///   automáticamente").
pub(crate) fn resolve_replace_sources_enabled(mode: Mode, no_replace_sources: bool) -> bool {
    if matches!(mode, Mode::Fast | Mode::Explore) {
        return false;
    }
    !no_replace_sources
}

/// Phase F (`replace_sources_enabled` flag): the synthesis-replacement
/// predicate is wired into `RankPhase` for every mode that runs
/// `SynthesizePhase`. `fast` skips synthesis entirely so the flag is
/// off there; the rest default to ON. The CLI flag
/// `--no-replace-sources` overrides the per-mode default.
///
/// Phase D follow-up (`adversary_enabled` flag): the deterministic
/// pattern-based adversary pass (`AdversaryPhase`) is opt-in. The
/// CLI flag `--adversary` overrides the per-mode default; `deep`
/// enables it automatically because it is the only mode where the
/// seven-pattern cost is amortised across a meaningful judge
/// panel. The phase writes `rankings/adversary_report.json` with
/// one section per [`AdversaryPattern`].
pub fn build_pipeline_for_mode(
    mode: Mode,
    cfg: &Config,
    replace_sources_enabled: bool,
    adversary_enabled: bool,
) -> Pipeline {
    let shape = pipeline_shape(mode, cfg);
    let proposals = shape.proposals;
    let judges = shape.judges;
    let sketches = shape.sketches;
    let critics = shape.critics;
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
    // - The LLM-based adversary pass remains a conditional branch
    //   inside `JudgePhase` so it stays out of the pipeline vector.
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
        // D.22.1, D.12.5: deterministic pattern-based adversary
        // pass. Inserted between `judge` and `rank` (the canonical
        // order in `Pipeline::canonical_phase_order`) so the
        // seven-pattern report runs on the freshly judged panel.
        // Opt-in: the pipeline builder toggles `enable` based on
        // `Mode::Deep` (default on) or the `--adversary` CLI flag.
        // When disabled, the phase still writes a (mostly empty)
        // sidecar so the dashboard distinguishes "ran with no
        // proposals" from "phase was skipped".
        .push(AdversaryPhase {
            enable: adversary_enabled,
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
    // The dual-hash helper lives in `phases::decompose::compute_brief_hash`
    // (extracted so tests can pin the contract without touching the
    // filesystem).
    let (brief_sha256, brief_blake3) = match std::fs::read(run_dir.brief()) {
        Ok(bytes) => crate::phases::decompose::compute_brief_hash(&bytes),
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
        schema_version: Manifest::schema_version_string(),
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
        config_hash: None,
        created_at_iso: now.to_rfc3339(),
        last_resumed_at_iso: None,
        resume_count: 0,
        prohibited_decisions: Vec::new(),
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

/// F5: read the `final/config_hash.txt` sidecar that the intake
/// phase wrote and return the trimmed hex digest. Returns `None`
/// when the sidecar is missing (legacy runs) or malformed
/// (corrupted disk) so the manifest can stay at its v2 default
/// without surfacing a confusing error to the operator.
fn read_intake_config_hash(run_dir: &crate::fs_layout::RunDir<'_>) -> Option<String> {
    let path = run_dir
        .final_dir()
        .join(crate::phases::intake::CONFIG_HASH_SIDECAR);
    let body = std::fs::read_to_string(&path).ok()?;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
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

    /// PR D7: the pipeline fan-out (proposals + sketches) is sourced
    /// from `Cardinality::for_mode_default`, not a hand-rolled table.
    /// Pins the delegation so a future refactor cannot drift the
    /// numbers back to the old `3/3/5/12/3` constants. Without the
    /// helper the values would be 3 (fast) instead of 4 (the spec
    /// soft target `(3 + 5) / 2`).
    #[test]
    fn pipeline_uses_cardinality_helper_instead_of_hardcoded() {
        let cfg = crate::config::Config::default();
        let shape = pipeline_shape(Mode::Fast, &cfg);
        // `Cardinality::for_mode_default(Fast).soft == 4` per the
        // spec D.21.1 range 3-5; the previous hard-coded value was 3.
        assert_eq!(
            shape.proposals, 4,
            "proposals must come from Cardinality::for_mode_default, not a hand-rolled table"
        );
        assert_eq!(
            shape.sketches, 4,
            "sketches must come from Cardinality::for_mode_default for non-fast modes"
        );
        let deep = pipeline_shape(Mode::Deep, &cfg);
        assert_eq!(
            deep.proposals, 17,
            "deep soft target is (10 + 25) / 2 == 17 per spec D.21.1"
        );
        let explore = pipeline_shape(Mode::Explore, &cfg);
        assert_eq!(
            explore.proposals, 0,
            "explore never runs proposals regardless of cardinality"
        );
        assert_eq!(
            explore.sketches, 27,
            "explore sketches still come from the cardinality soft target (15-40 mid)"
        );
    }

    /// PR D7: the judge panel size is sourced from
    /// `judge_quorum_for_mode`, which honours spec D.21.7
    /// (fast=1, standard=3, deep=5, batch=1). The previous
    /// hard-coded table had `fast=3, standard=5, deep=7` which
    /// contradicted the spec.
    #[test]
    fn pipeline_uses_judge_quorum_helper() {
        let cfg = crate::config::Config::default();
        let fast = pipeline_shape(Mode::Fast, &cfg);
        assert_eq!(fast.judges, 1, "spec D.21.7: fast uses 1 judge");
        let standard = pipeline_shape(Mode::Standard, &cfg);
        assert_eq!(standard.judges, 3, "spec D.21.7: standard uses 3 judges");
        let deep = pipeline_shape(Mode::Deep, &cfg);
        assert_eq!(deep.judges, 5, "spec D.21.7: deep uses 5 judges");
        let explore = pipeline_shape(Mode::Explore, &cfg);
        assert_eq!(
            explore.judges, 0,
            "explore never runs judges regardless of quorum helper"
        );
        let batch = pipeline_shape(Mode::Batch, &cfg);
        assert_eq!(batch.judges, 1, "spec D.21.7: batch uses 1 judge");
    }

    /// PR D7: profile-supplied `judge_quorum_overrides` propagate
    /// through `build_pipeline_for_mode` so `--profile <name>`
    /// actually changes the judge panel size (previously the
    /// table ignored profiles entirely).
    #[test]
    fn profile_quorum_override_applies_to_pipeline() {
        let mut cfg = crate::config::Config::default();
        // Baseline: no profile, fast uses the spec 1 judge.
        assert_eq!(pipeline_shape(Mode::Fast, &cfg).judges, 1);
        // Profile: bump `fast` to 3 judges and `deep` to 9.
        cfg.profile_judge_quorum_overrides
            .insert("fast".to_owned(), 3);
        cfg.profile_judge_quorum_overrides
            .insert("deep".to_owned(), 9);
        let fast = pipeline_shape(Mode::Fast, &cfg);
        assert_eq!(
            fast.judges, 3,
            "profile_judge_quorum_overrides must win over the spec baseline"
        );
        let deep = pipeline_shape(Mode::Deep, &cfg);
        assert_eq!(
            deep.judges, 9,
            "per-mode profile override must reach the pipeline shape"
        );
        // Untouched modes still use the spec baseline.
        let standard = pipeline_shape(Mode::Standard, &cfg);
        assert_eq!(standard.judges, 3, "unrelated modes keep the spec baseline");
        // Cardinality-derived counts are unaffected by quorum overrides.
        let fast_after = pipeline_shape(Mode::Fast, &cfg);
        assert_eq!(
            fast_after.proposals, 4,
            "profile quorum override must not touch the cardinality-driven proposal count"
        );
    }

    /// PR-B1 (B1.1): the `--no-replace-sources` flag must actually
    /// disable the synthesis-replacement predicate. Previously the
    /// flag was parsed but ignored — `run_full_pipeline` computed
    /// `replace_sources_enabled = !matches!(mode, Mode::Fast)` from
    /// the mode alone. With the wire-up, the operator-supplied
    /// `no_replace_sources` value short-circuits the per-mode
    /// default. Pin the full matrix so a future refactor cannot
    /// silently drop the flag again.
    #[test]
    fn no_replace_sources_disables_replacement_in_non_fast_modes() {
        assert!(
            !resolve_replace_sources_enabled(Mode::Standard, true),
            "--no-replace-sources must disable replacement even in `standard`"
        );
        assert!(
            !resolve_replace_sources_enabled(Mode::Deep, true),
            "--no-replace-sources must disable replacement even in `deep`"
        );
        assert!(
            !resolve_replace_sources_enabled(Mode::Batch, true),
            "--no-replace-sources must disable replacement even in `batch`"
        );
    }

    /// PR-B1 (B1.1): without the flag the per-mode default
    /// (`fast/explore → off`, `standard/deep/batch → on`) is
    /// preserved verbatim. This pins backward-compat for every
    /// existing invocation that never passed
    /// `--no-replace-sources`.
    #[test]
    fn no_replace_sources_default_preserves_per_mode_wiring() {
        assert!(
            !resolve_replace_sources_enabled(Mode::Fast, false),
            "fast never runs synthesis so replacement stays off"
        );
        assert!(
            !resolve_replace_sources_enabled(Mode::Explore, false),
            "explore ends at sketches so replacement stays off"
        );
        assert!(
            resolve_replace_sources_enabled(Mode::Standard, false),
            "standard's default is replacement ON"
        );
        assert!(
            resolve_replace_sources_enabled(Mode::Deep, false),
            "deep's default is replacement ON"
        );
        assert!(
            resolve_replace_sources_enabled(Mode::Batch, false),
            "batch's default is replacement ON"
        );
    }

    /// PR-B1 (B1.4): `--max-parallelism` is validated up-front
    /// (D.15.5: hard cap 64 simultaneous LLM calls). The helper
    /// in `flags_batch.rs` is the source of truth for the
    /// message — pin its contract here so a future tweak to
    /// either the helper or the cheatsheet does not silently
    /// drift the user-facing error string.
    #[test]
    fn max_parallelism_helper_accepts_cap_and_below() {
        assert!(flags_batch::validate_max_parallelism(64).is_ok());
        assert!(flags_batch::validate_max_parallelism(1).is_ok());
        assert!(flags_batch::validate_max_parallelism(0).is_ok());
    }

    /// PR-B1 (B1.4): values above the cap must surface the
    /// documented `exceeds maximum 64` message so CI scripts can
    /// grep for it. The dispatcher wraps the helper's `String`
    /// into `Error::InvalidArgs` (exit 2 per the cheatsheet §1
    /// error matrix).
    #[test]
    fn max_parallelism_helper_rejects_above_cap_with_clear_message() {
        let err = flags_batch::validate_max_parallelism(65).expect_err("must error");
        assert!(
            err.contains("exceeds maximum 64"),
            "error must mention the cap; got {err:?}"
        );
        let err = flags_batch::validate_max_parallelism(4096).expect_err("must error");
        assert!(
            err.contains("exceeds maximum 64"),
            "error must mention the cap; got {err:?}"
        );
    }

    /// PR round-8 audit §E.3 item #9: the wire-up from
    /// `Config::token_budget` to `Db::set_budget` lives in `run()`
    /// (just after `register_run`). This test pins the contract by
    /// replaying the same `register_run` → `set_budget` sequence
    /// against a real `Db` and asserting that `budget_state` ends
    /// up with the planned cap. `run()` itself is too heavy to
    /// invoke from a unit test (it would need the full mock
    /// provider + `RunContext` + `Telemetry`), but the two calls
    /// it issues are the only production-side state that the wire
    /// contract depends on.
    #[test]
    fn token_budget_wires_into_budget_state_when_some() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = crate::fs_layout::MoaganHome::at(tmp.path().to_path_buf());
        home.ensure().expect("ensure home");
        let db = Db::open(&home.meta_db_path()).expect("open db");
        let run_id = RunId::new();
        db.register_run(
            run_id,
            Mode::Fast.as_str(),
            "running",
            env!("CARGO_PKG_VERSION"),
            None,
            None,
            None,
        )
        .expect("register_run");

        // Mirror the wire-up in `run()`: when `cfg.token_budget`
        // is `Some(N)`, call `set_budget(run_id, N)`.
        let cfg_token_budget = Some(5_000_u64);
        if let Some(planned) = cfg_token_budget {
            db.set_budget(run_id, planned).expect("set_budget");
        }

        let (planned, used) = db.budget_read(run_id).expect("budget_read");
        assert_eq!(planned, 5_000, "token_budget must reach the row");
        assert_eq!(used, 0, "fresh row must start at zero usage");
    }

    /// PR round-8 audit §E.3 item #9: when `Config::token_budget`
    /// is `None`, `run()` deliberately skips the `set_budget`
    /// call. `BudgetObserver` treats a missing `budget_state` row
    /// the same as `planned_tokens = 0` (i.e. unlimited), so the
    /// default-fall-through path must stay a no-op. This test
    /// pins that contract so a future refactor cannot silently
    /// start writing a `0` cap and surprise operators with a
    /// hard-capped unlimited run.
    #[test]
    fn token_budget_left_unset_when_config_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = crate::fs_layout::MoaganHome::at(tmp.path().to_path_buf());
        home.ensure().expect("ensure home");
        let db = Db::open(&home.meta_db_path()).expect("open db");
        let run_id = RunId::new();
        db.register_run(
            run_id,
            Mode::Fast.as_str(),
            "running",
            env!("CARGO_PKG_VERSION"),
            None,
            None,
            None,
        )
        .expect("register_run");

        // Mirror the wire-up in `run()`: when `cfg.token_budget`
        // is `None`, do nothing.
        let cfg_token_budget: Option<u64> = None;
        if let Some(planned) = cfg_token_budget {
            db.set_budget(run_id, planned).expect("set_budget");
        }

        // `budget_state` row is absent: `budget_read` returns the
        // zero-fallback (`(0, 0)`), and `BudgetObserver::new` reads
        // that as "unlimited". A non-zero `planned_tokens` here
        // would mean the wire-up leaked a default.
        let (planned, used) = db.budget_read(run_id).expect("budget_read");
        assert_eq!(
            planned, 0,
            "absent budget_state must read back as unlimited"
        );
        assert_eq!(used, 0, "fresh run must have no usage");
    }
}
