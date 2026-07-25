//! `moagan run` — start a new run, build a pipeline, execute it, write
//! the manifest, and print a summary.

use std::sync::Arc;

use crate::config::Config;
use crate::domain::Manifest;
use crate::error::{Error, Result};
use crate::execution::Parallelism;
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::llm::{ProviderRegistry, registry_from_config};
use crate::phases::{
    ClarifyPhase, CritiquePhase, DeliverPhase, GatePhase, IntakePhase, JudgePhase, Pipeline,
    ProposePhase, RankPhase, RepairPhase, RoutePhase,
};
use crate::redact::RedactPolicy;
use crate::telemetry::Telemetry;

/// Options for `moagan run`.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Mode name (`"fast"`, `"standard"`, ...).
    pub mode: String,
    /// Provider name (must be in config).
    pub provider: String,
    /// User prompt.
    pub prompt: String,
    /// Optional override of the home directory.
    pub home: Option<std::path::PathBuf>,
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
    let providers = Arc::new(build_registry_for(cfg, &default_provider)?);
    let default_model = cfg.provider(&default_provider)?.model.clone();

    let policy = RedactPolicy::default();
    let telemetry = Telemetry::open(run_id, &run_dir, policy)?;
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
        opts.mode.clone(),
    );

    let pipeline = build_pipeline_for_mode(&opts.mode, cfg);
    let outputs = pipeline.run(&ctx)?;

    let manifest = build_manifest(&run_id, &opts.mode, "completed", &outputs);
    let manifest_json = serde_json::to_vec_pretty(&manifest).map_err(Error::from)?;
    crate::atomic::writer::AtomicWriter::new().write(&run_dir.manifest(), &manifest_json)?;
    telemetry.flush()?;
    println!(
        "moagan run {} mode={} provider={} -> {}",
        run_id.short(),
        opts.mode,
        default_provider,
        run_dir.root().display()
    );
    Ok(run_id)
}

fn build_registry_for(cfg: &Config, selected: &str) -> Result<ProviderRegistry> {
    // Build a registry containing only the selected provider (plus `mock`
    // when explicitly requested). The full cfg may declare other
    // providers whose `from_config` requires an API key; we must not
    // construct them when the user did not ask for them.
    let mut spec_map = std::collections::BTreeMap::new();
    if let Some(spec) = cfg.providers.get(selected) {
        spec_map.insert(selected.to_owned(), spec.clone());
    } else {
        return Err(Error::InvalidArgs(format!(
            "provider '{selected}' is not in config"
        )));
    }
    registry_from_config(&spec_map)
}

fn build_pipeline_for_mode(mode: &str, cfg: &Config) -> Pipeline {
    let (proposals, critics, judges) = match mode {
        "fast" => (3u32, 2u32, 3u32),
        "standard" => (3u32, 3u32, 5u32),
        _ => (3u32, 2u32, 3u32),
    };
    let _ = cfg;
    Pipeline::new()
        .push(IntakePhase)
        .push(ClarifyPhase)
        .push(RoutePhase)
        .push(ProposePhase { count: proposals })
        .push(GatePhase)
        .push(CritiquePhase {
            critics_per_proposal: critics,
        })
        .push(RepairPhase)
        .push(JudgePhase { judges })
        .push(RankPhase)
        .push(DeliverPhase)
}

fn build_manifest(
    run_id: &RunId,
    mode: &str,
    status: &str,
    _outputs: &[crate::phases::PhaseOutput],
) -> Manifest {
    use chrono::Utc;
    let now = Utc::now();
    Manifest {
        schema_version: "v1".into(),
        run_id: *run_id,
        mode: mode.into(),
        status: status.into(),
        created_at: now,
        updated_at: now,
        client_version: env!("CARGO_PKG_VERSION").into(),
        brief_sha256: String::new(),
        brief_blake3: String::new(),
        provider: String::new(),
        model: String::new(),
        phases: Vec::new(),
        usage: crate::domain::ManifestUsage::default(),
        manifest_blake3: String::new(),
    }
}
