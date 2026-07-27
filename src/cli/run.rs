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
    ClarifyPhase, CritiquePhase, DeliverPhase, GatePhase, IntakePhase, JudgePhase, Pipeline,
    ProposePhase, RankPhase, RepairPhase, RoutePhase,
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
}

/// Run a moagan pipeline end-to-end. Returns the run id on success.
pub fn run(opts: RunOptions, cfg: &Config) -> Result<RunId> {
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
    let parallelism = Parallelism::new(cfg.max_parallelism);

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
    );

    let pipeline = build_pipeline_for_mode(opts.mode, cfg);
    let _outputs = pollster::block_on(pipeline.run(&ctx))?;

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
    telemetry.flush()?;
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

/// Build a `ProviderRegistry` containing only `selected` (the provider
/// the user asked for). Reused by `continue_cmd::run_refine` and
/// `run_rerank` so those flows get a real provider and not an empty
/// registry that panics on the first `RunContext::provider()` call.
pub(crate) fn build_registry_for(
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
/// (3 proposals, 2 critics, 3 judges for `fast`; 3 proposals, 3
/// critics, 5 judges for `standard`).
fn build_pipeline_for_mode(mode: Mode, cfg: &Config) -> Pipeline {
    let (proposals, critics, judges) = match mode {
        Mode::Fast => (3u32, 2u32, 3u32),
        Mode::Standard => (3u32, 3u32, 5u32),
    };
    let cfg_arc = std::sync::Arc::new(cfg.clone());
    Pipeline::new()
        .push(IntakePhase)
        .push(ClarifyPhase)
        .push(RoutePhase)
        .push(ProposePhase { count: proposals })
        .push(GatePhase)
        .push(CritiquePhase {
            critics_per_proposal: critics,
        })
        .push(RepairPhase::from_config(cfg))
        .push(JudgePhase { judges })
        .push(RankPhase { config: cfg_arc })
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

    // 2. Aggregate phase events from telemetry/phases.jsonl into one
    //    ManifestPhase per phase name. Start events set started_unix;
    //    end events set ended_unix + status; error events set error.
    let phases_path = run_dir.telemetry().join("phases.jsonl");
    let phase_events = read_phase_events(&phases_path);
    let call_counts = count_calls_per_phase(&run_dir.telemetry().join("calls.jsonl"));
    let manifest_phases = aggregate_phase_events(&phase_events, &call_counts);

    // 3. Aggregate usage from telemetry/calls.jsonl.
    let usage = aggregate_usage(&run_dir.telemetry().join("calls.jsonl"));

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

/// Read every `phases.jsonl` line into a `PhaseEvent` list. Missing
/// files and individual malformed lines are silently skipped so a
/// partial telemetry stream never blocks manifest emission.
fn read_phase_events(path: &Path) -> Vec<PhaseEvent> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    raw.lines()
        .filter_map(|line| serde_json::from_str::<PhaseEvent>(line).ok())
        .collect()
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

/// Walk every `calls.jsonl` line and count how many calls landed in
/// each phase name. Used to populate `ManifestPhase.calls`.
fn count_calls_per_phase(path: &Path) -> std::collections::BTreeMap<String, u32> {
    use std::collections::BTreeMap;
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return BTreeMap::new(),
    };
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
fn aggregate_usage(path: &Path) -> ManifestUsage {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return ManifestUsage::default(),
    };
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
