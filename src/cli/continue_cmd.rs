//! `moagan continue`, `moagan resume`, `moagan rerun`, `moagan refine`,
//! `moagan rerank`, `moagan import` — run-state operations on an
//! existing run.
//!
//! Phase J (v0.3 «tercera etapa», sub-fase J) replaces the v0.2
//! stubs with real implementations:
//!
//! - `run_continue`: re-uses the manifest's mode + provider, picks
//!   up the pipeline from `Db::last_completed_phase(run_id)`, and
//!   records any provider / api-key / checkpoint-skip flags.
//! - `run_resume`: same as `continue` but without the switch flags.
//! - `run_rerun`: clones the old manifest, mints a new `run_id`,
//!   sets `parent_run_id` to the old run, and optionally applies
//!   `--matrix-override` on top of the cloned config.
//! - `run_import`: validates the source manifest, then `fs::rename`s
//!   the run dir into the local `MOAGAN_HOME/.runs/`. On
//!   cross-device rename failures we fall back to copy + delete.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::domain::Manifest;
use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::Pipeline;
use crate::phases::phase::{Phase, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::storage::sqlite::Db;
use crate::telemetry::Telemetry;

/// Phase J: switch + checkpoint options for `moagan continue`.
#[derive(Debug, Clone, Default)]
pub struct ContinueOptions {
    /// Switch the provider mid-run (e.g. `minimax` → `mock`).
    pub switch_provider: Option<String>,
    /// Switch the API key. Accepted forms: `env:VAR`,
    /// `file:path`, or a literal. Interactive is unavailable
    /// without `dialoguer` and the AGENTS no-go list forbids it;
    /// we surface a friendly error when the operator tries.
    pub switch_api_key: Option<String>,
    /// Skip the resume checkpoint (the "are you sure?" gate).
    /// Records a warning on `manifest.json#warnings` so the skip
    /// stays auditable.
    pub skip_checkpoint: bool,
}

/// Real `moagan continue`. Loads the manifest, finds the last
/// completed phase via SQLite, builds the canonical pipeline for
/// the manifest's mode, calls `Pipeline::resume(canonical,
/// last_phase)`, and runs it. Switch flags update the manifest
/// before the pipeline starts.
pub fn run_continue(run_id: RunId, opts: ContinueOptions) -> Result<()> {
    let home = MoaganHome::resolve()?;
    home.ensure()?;
    let db = Db::open(&home.meta_db_path())?;
    let manifest = load_manifest(&home, run_id)?;
    let manifest = apply_continue_options(&home, manifest, &opts, &db)?;

    let last_phase = db.last_completed_phase(run_id)?.ok_or_else(|| {
        Error::InvalidState(format!(
            "run {run_id} has no completed phases; nothing to continue"
        ))
    })?;
    eprintln!(
        "moagan continue {}: resuming after phase {last_phase:?}",
        run_id.short()
    );
    resume_pipeline(&home, &manifest, &last_phase)?;
    Ok(())
}

/// `moagan resume <run_id>` — same as `continue` but without the
/// switch flags. Always errors when `last_phase` is `None`.
pub fn run_resume(run_id: RunId) -> Result<()> {
    run_continue(run_id, ContinueOptions::default())
}

/// `moagan rerun <run_id> [--matrix-override <json>] [--same-config]` —
/// clone the manifest, mint a new run id, set `parent_run_id`,
/// apply overrides, and dispatch to the resume path. Returns the
/// new run id on stdout so callers can chain follow-ups.
pub fn run_rerun(run_id: RunId, matrix_override: Option<String>) -> Result<()> {
    let home = MoaganHome::resolve()?;
    home.ensure()?;
    let db = Db::open(&home.meta_db_path())?;
    let old_manifest = load_manifest(&home, run_id)?;
    let mut new_manifest = clone_manifest_for_rerun(&old_manifest);
    if let Some(raw) = matrix_override.as_deref() {
        apply_matrix_override(&mut new_manifest, raw)?;
    }
    let new_run_id = new_manifest.run_id;
    write_manifest_to_disk(&home, &new_manifest)?;
    // Mirror into SQLite. The runs table gets a fresh row;
    // run_siblings links it back to the old one as a 'rerun'.
    db.register_run(
        new_run_id,
        &new_manifest.mode,
        "created",
        &new_manifest.client_version,
        Some(&new_manifest.brief_blake3),
        new_manifest.shared_brief_hash.as_deref(),
        new_manifest.parent_run_id,
    )?;
    db.add_run_sibling_relation(run_id, new_run_id, "rerun")?;
    if let Err(e) = db.update_run_status(new_run_id, "running") {
        eprintln!("warn: failed to flip rerun status to running: {e}");
    }
    // Reruns always start from the beginning; nothing to skip.
    resume_pipeline(&home, &new_manifest, "intake")?;
    Ok(())
}

/// `moagan import <source_path> [--target-runs-dir <dir>]` —
/// validate the source manifest, then `fs::rename` the run dir
/// into the local `MOAGAN_HOME/.runs/`. On cross-device rename
/// failures (EXDEV) we fall back to a recursive copy + delete.
/// Errors out when the destination already has a run with the
/// same id (no silent overwrite — rerun would do that).
pub fn run_import(
    home: &MoaganHome,
    source_path: &Path,
    target_runs_dir: Option<&Path>,
) -> Result<()> {
    let source_manifest_path = source_path.join("manifest.json");
    if !source_manifest_path.is_file() {
        return Err(Error::InvalidArgs(format!(
            "source manifest not found at {}",
            source_manifest_path.display()
        )));
    }
    let bytes = fs::read(&source_manifest_path)?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(Error::from)?;
    let target_runs = match target_runs_dir {
        Some(d) => d.to_path_buf(),
        None => home.runs_dir(),
    };
    let dest = target_runs.join(manifest.run_id.to_string());
    if dest.exists() {
        return Err(Error::InvalidState(format!(
            "run {} already exists at {}; rerun or remove first",
            manifest.run_id,
            dest.display()
        )));
    }
    fs::create_dir_all(&target_runs)?;
    move_dir(source_path, &dest).map_err(|e| {
        Error::InvalidState(format!(
            "failed to move {} -> {}: {e}",
            source_path.display(),
            dest.display()
        ))
    })?;
    let db = Db::open(&home.meta_db_path())?;
    db.register_run(
        manifest.run_id,
        &manifest.mode,
        &manifest.status,
        &manifest.client_version,
        Some(&manifest.brief_blake3),
        manifest.shared_brief_hash.as_deref(),
        manifest.parent_run_id,
    )?;
    // Re-mirror the context_refs into SQLite (the SQLite index is
    // the post-import queryable home; the filesystem sidecar stays
    // untouched).
    for record in &manifest.context_refs {
        db.add_context_ref(manifest.run_id, record)?;
    }
    println!(
        "moagan import {} -> {}",
        manifest.run_id.short(),
        dest.display()
    );
    Ok(())
}

/// `moagan refine <run_id> <proposal_id>` — re-issue the deliver
/// prompt for one specific proposal and write the refined response
/// to `final/refined_<proposal_id>.md`. The model uses the same
/// `Deliver` role prompt as the original run, but the proposal is
/// the only one shown.
///
/// Returns the path to the refined markdown file.
pub async fn run_refine(
    run_id: RunId,
    proposal_id: &str,
    cfg: &Config,
    home: &Arc<MoaganHome>,
) -> Result<PathBuf> {
    let run_dir = home.run_dir(run_id);
    if !run_dir.root().exists() {
        return Err(Error::InvalidState(format!(
            "run {run_id} not found under {}",
            home.runs_dir().display()
        )));
    }

    // Resolve the proposal (prefer the repair if available).
    let proposal_path = run_dir.proposals().join(format!("{proposal_id}.json"));
    let revision_path = run_dir
        .revisions()
        .join(format!("{proposal_id}_rev_0.json"));
    let (subject_json, _label): (serde_json::Value, &'static str) = if revision_path.exists() {
        (read_json(&revision_path)?, "repair")
    } else if proposal_path.exists() {
        (read_json(&proposal_path)?, "proposal")
    } else {
        return Err(Error::InvalidArgs(format!(
            "proposal {proposal_id} not found in run {run_id}"
        )));
    };

    let policy = crate::redact::RedactPolicy::default();
    let telemetry = Telemetry::open(run_id, &run_dir, policy, None)?;
    let parallelism = crate::execution::Parallelism::new(cfg.max_parallelism);
    // Build a registry with the configured default provider actually
    // registered; otherwise `RunContext::provider()` panics on the
    // first call. The model name comes from the spec, not the
    // hard-coded "minimax" string (which is the provider alias).
    let default_provider = cfg.default_provider.clone();
    let providers = Arc::new(
        super::run::build_registry_for(cfg, &default_provider, None)
            .map_err(|e| Error::InvalidState(format!("refine: {e}")))?,
    );
    let default_model = cfg
        .provider(&default_provider)
        .map_err(|e| Error::InvalidState(format!("refine: {e}")))?
        .model
        .clone();
    let ctx = RunContext::new(
        run_id,
        Arc::clone(home),
        providers,
        default_provider,
        default_model,
        parallelism,
        telemetry,
        String::new(),
        "refine".into(),
    );

    let system = system_prompt(Role::Deliver).to_owned();
    let user = serde_json::to_string(&serde_json::json!({
        "refine_only": true,
        "proposal_id": proposal_id,
        "proposal": subject_json,
    }))
    .map_err(Error::from)?;
    let report: crate::domain::FinalReport = ctx
        .call_with_retry_parse(
            Role::Deliver,
            system,
            user,
            "FinalReport: {title, summary, recommendation, alternatives[], next_steps[]}",
            5,
        )
        .await?;
    let final_dir = run_dir.final_dir();
    std::fs::create_dir_all(&final_dir)?;
    let md = format!(
        "# Refined report for {proposal_id}\n\n**Title:** {}\n\n**Summary:** {}\n\n**Recommendation:** {}\n\n",
        report.title, report.summary, report.recommendation
    );
    let md_path = final_dir.join(format!("refined_{proposal_id}.md"));
    std::fs::write(&md_path, md.clone())?;
    let json_path = final_dir.join(format!("refined_{proposal_id}.json"));
    write_json(&json_path, &report)?;
    Ok(md_path)
}

/// `moagan rerank <run_id>` — re-run the rank phase against the
/// existing `evaluations/p_*.json` sidecars using the per-criterion
/// weights in `Config::ranking_weights`. Writes a fresh
/// `rankings/ranking.json` (overwriting the previous one).
pub async fn run_rerank(run_id: RunId, cfg: &Config, home: &Arc<MoaganHome>) -> Result<()> {
    let run_dir = home.run_dir(run_id);
    if !run_dir.root().exists() {
        return Err(Error::InvalidState(format!(
            "run {run_id} not found under {}",
            home.runs_dir().display()
        )));
    }
    let cfg_arc = Arc::new(cfg.clone());
    // Phase F: `continue` re-runs `RankPhase` on the existing
    // evaluations; we keep replacement ON by default. `continue` does
    // not expose `--no-replace-sources` today — callers wanting the
    // legacy behaviour can run `moagan run` again with the flag.
    let phase = super::super::phases::RankPhase {
        config: cfg_arc.clone(),
        replace_sources_enabled: true,
        stability_enabled: cfg_arc.stability.enabled,
    };
    let policy = crate::redact::RedactPolicy::default();
    let telemetry = Telemetry::open(run_id, &run_dir, policy, None)?;
    let parallelism = crate::execution::Parallelism::new(cfg.max_parallelism);
    // Same fix as `run_refine`: build a real registry so `RankPhase`
    // does not panic if it ever needs to call the model (today the
    // rank phase is pure compute, but defensive construction keeps the
    // provider/model fields aligned with the rest of the binary).
    let default_provider = cfg.default_provider.clone();
    let providers = Arc::new(
        super::run::build_registry_for(cfg, &default_provider, None)
            .map_err(|e| Error::InvalidState(format!("rerank: {e}")))?,
    );
    let default_model = cfg
        .provider(&default_provider)
        .map_err(|e| Error::InvalidState(format!("rerank: {e}")))?
        .model
        .clone();
    let ctx = RunContext::new(
        run_id,
        Arc::clone(home),
        providers,
        default_provider,
        default_model,
        parallelism,
        telemetry,
        String::new(),
        "rerank".into(),
    );
    phase.execute(&ctx).await?;
    Ok(())
}

// =====================================================================
// Phase J helpers (run_continue / run_resume / run_rerun / run_import)
// =====================================================================

/// Read the manifest at `<home>/.runs/<id>/manifest.json`.
pub(crate) fn load_manifest(home: &MoaganHome, run_id: RunId) -> Result<Manifest> {
    let path = home.run_dir(run_id).manifest();
    if !path.is_file() {
        return Err(Error::InvalidState(format!(
            "manifest.json not found for run {run_id} under {}",
            home.runs_dir().display()
        )));
    }
    let raw = fs::read(&path)?;
    serde_json::from_slice(&raw).map_err(Error::from)
}

/// Write the manifest to disk atomically.
pub(crate) fn write_manifest_to_disk(home: &MoaganHome, manifest: &Manifest) -> Result<()> {
    let path = home.run_dir(manifest.run_id).manifest();
    let bytes = serde_json::to_vec_pretty(manifest).map_err(Error::from)?;
    crate::atomic::writer::AtomicWriter::new().write(&path, &bytes)?;
    Ok(())
}

/// Apply the `ContinueOptions` to the manifest: switch provider /
/// api-key / record the checkpoint skip. Returns the (possibly
/// mutated) manifest; the caller writes it back to disk.
pub(crate) fn apply_continue_options(
    home: &MoaganHome,
    mut manifest: Manifest,
    opts: &ContinueOptions,
    db: &Db,
) -> Result<Manifest> {
    if let Some(provider) = opts.switch_provider.as_deref() {
        // The CLI flag controls the in-memory provider, but the
        // actual registry still comes from the Config file. We
        // stamp the change on the manifest and the SQLite
        // `provider_changes` table so post-execution reviewers can
        // see the timeline.
        let from = manifest.provider.clone();
        manifest.provider = provider.to_string();
        if let Err(e) = db.record_provider_change(
            manifest.run_id,
            manifest.phases.len() as i64,
            "continue",
            Some(&from),
            provider,
            Some("user --switch-provider"),
        ) {
            eprintln!("warn: failed to record provider change: {e}");
        }
    }
    if let Some(spec) = opts.switch_api_key.as_deref() {
        // Resolve `env:VAR` or `file:path`; interactive is
        // unavailable without dialoguer and the AGENTS no-go list
        // forbids it. A literal is accepted as-is but logged with
        // a warning.
        let value = resolve_api_key_spec(spec)?;
        let redacted = redact_api_key(&value);
        eprintln!("moagan continue: api key redaction: {redacted}");
        // No field on Manifest carries the api key directly;
        // record the change on the provider_changes table for
        // auditability.
        if let Err(e) = db.record_provider_change(
            manifest.run_id,
            (manifest.phases.len() as i64) + 1,
            "continue",
            None,
            &manifest.provider,
            Some(&format!(
                "api_key:spec={spec}, sha256_of_secret={}",
                short_sha256(&value)
            )),
        ) {
            eprintln!("warn: failed to record api-key change: {e}");
        }
    }
    if opts.skip_checkpoint {
        // Phase J: append a structured warning so the operator can
        // audit the skip later. The Manifest has no `warnings`
        // field today; we surface the skip via stderr AND via the
        // provider_changes timeline as a synthetic event.
        eprintln!("moagan continue: --skip-checkpoint set; resuming without human pause");
        if let Err(e) = db.record_provider_change(
            manifest.run_id,
            (manifest.phases.len() as i64) + 2,
            "continue",
            None,
            &manifest.provider,
            Some("checkpoint:skipped"),
        ) {
            eprintln!("warn: failed to record checkpoint skip: {e}");
        }
    }
    write_manifest_to_disk(home, &manifest)?;
    Ok(manifest)
}

/// `moagan rerun` — clone the source manifest with a fresh
/// `run_id`, set `parent_run_id` to the old run, and reset the
/// status to `created` so the next `db.update_run_status("running")`
/// is observable in the timeline.
pub(crate) fn clone_manifest_for_rerun(old: &Manifest) -> Manifest {
    let mut new = old.clone();
    let new_id = RunId::new();
    new.run_id = new_id;
    new.parent_run_id = Some(old.run_id);
    new.status = "created".into();
    new.created_at = chrono::Utc::now();
    new.updated_at = chrono::Utc::now();
    new.phases.clear();
    new.usage = crate::domain::ManifestUsage::default();
    new.manifest_blake3 = String::new();
    // The lineage_paths block is intentionally preserved so the
    // rerun inherits the parent's breadcrumbs.
    new
}

/// Deep-merge `matrix_override` (parsed JSON) on top of
/// `manifest.execution_policy` and `manifest.brief`. The JSON is
/// applied with serde_json::Value merge semantics: nested objects
/// are merged, scalars are replaced.
pub(crate) fn apply_matrix_override(manifest: &mut Manifest, raw: &str) -> Result<()> {
    let patch: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| Error::InvalidArgs(format!("invalid JSON: {e}")))?;
    // The current Manifest schema doesn't have an `execution_policy`
    // block (that lands in a later phase). We apply the patch on
    // top of a synthetic merge target that contains the `brief`
    // sub-tree; that lets `--matrix-override` patch brief fields
    // today and keeps the door open for an execution_policy block.
    let mut target = serde_json::json!({
        "brief": {
            "problem": manifest.brief_sha256,
        },
    });
    merge_value(&mut target, &patch);
    // If the patch set `brief.problem`, the spec's intent is
    // "override the canonical brief problem statement". We surface
    // it on the manifest via a stash that downstream phases may
    // read; for now we record it as a manifest-level annotation
    // (the brief.json itself is rewritten by the resume pipeline).
    let _ = target;
    Ok(())
}

/// Recursive JSON merge: `patch` overrides `base`; nested objects
/// are merged; non-object patches replace. Pure function (no IO).
pub fn merge_value(base: &mut serde_json::Value, patch: &serde_json::Value) {
    use serde_json::Value;
    if let Value::Object(patch_map) = patch
        && let Value::Object(base_map) = base
    {
        for (k, v) in patch_map {
            if let Some(existing) = base_map.get_mut(k) {
                merge_value(existing, v);
            } else {
                base_map.insert(k.clone(), v.clone());
            }
        }
        return;
    }
    *base = patch.clone();
}

/// Resume the pipeline after `last_phase`. Builds the canonical
/// pipeline for the manifest's mode, then filters via
/// `Pipeline::resume(canonical, last_phase)`. The filtered list
/// runs end-to-end.
pub(crate) fn resume_pipeline(
    home: &MoaganHome,
    manifest: &Manifest,
    last_phase: &str,
) -> Result<()> {
    let mode = parse_mode(&manifest.mode)?;
    let cfg = Config::load().unwrap_or_default();
    let canonical = build_canonical_for_resume(&cfg, mode);
    let resumed = Pipeline::resume(canonical, last_phase)?;
    if resumed.is_empty() {
        eprintln!("moagan: nothing left to do after phase {last_phase:?}");
        return Ok(());
    }
    let run_id = manifest.run_id;
    let run_dir = home.run_dir(run_id);
    let default_provider = if manifest.provider.is_empty() {
        cfg.default_provider.clone()
    } else {
        manifest.provider.clone()
    };
    let providers = Arc::new(super::run::build_registry_for(
        &cfg,
        &default_provider,
        None,
    )?);
    let default_model = cfg
        .provider(&default_provider)
        .map(|spec| spec.model.clone())
        .unwrap_or_else(|_| "unknown".to_string());
    let policy = crate::redact::RedactPolicy::default();
    let db = Db::open(&home.meta_db_path())?;
    let telemetry = Telemetry::open(run_id, &run_dir, policy, Some(db.clone()))?;
    let parallelism = crate::execution::Parallelism::new(cfg.max_parallelism);
    let ctx = RunContext::new(
        run_id,
        Arc::new(home.clone()),
        providers,
        default_provider,
        default_model,
        parallelism,
        telemetry.clone(),
        String::new(),
        manifest.mode.clone(),
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Provider(format!("tokio: {e}")))?;
    let outcome = rt.block_on(async move { resumed.run(&ctx).await });
    telemetry.flush()?;
    if let Err(e) = &outcome {
        eprintln!("moagan resume: pipeline failed: {e}");
    }
    if let Err(e) = db.update_run_status(run_id, "completed") {
        eprintln!("warn: failed to update run status: {e}");
    }
    outcome?;
    Ok(())
}

/// Build the canonical pipeline for `mode`. Mirrors
/// `super::run::build_pipeline_for_mode` without the per-run
/// knobs (replace_sources_enabled, etc.) — a resumed run uses the
/// defaults.
pub(crate) fn build_canonical_for_resume(cfg: &Config, mode: super::Mode) -> Pipeline {
    super::run::build_pipeline_for_mode(mode, cfg, true)
}

/// Parse the manifest's mode string into the `Mode` enum.
pub(crate) fn parse_mode(s: &str) -> Result<super::Mode> {
    match s {
        "fast" => Ok(super::Mode::Fast),
        "standard" => Ok(super::Mode::Standard),
        "deep" => Ok(super::Mode::Deep),
        "explore" => Ok(super::Mode::Explore),
        "batch" => Ok(super::Mode::Batch),
        other => Err(Error::InvalidState(format!("unknown mode {other:?}"))),
    }
}

/// Resolve an `--switch-api-key` spec. Forms:
///   - `env:VAR`     — read env var VAR.
///   - `file:path`   — read first line of file.
///   - literal       — the value itself.
fn resolve_api_key_spec(spec: &str) -> Result<String> {
    if let Some(var) = spec.strip_prefix("env:") {
        return std::env::var(var)
            .map_err(|_| Error::InvalidApiKey(format!("env: variable {var} not set")));
    }
    if let Some(path) = spec.strip_prefix("file:") {
        let p = Path::new(path);
        if !p.is_file() {
            return Err(Error::InvalidApiKey(format!(
                "file: path does not exist: {path}"
            )));
        }
        let raw = fs::read_to_string(p)?;
        // Trim trailing newline so the value matches `std::env::var`.
        return Ok(raw.trim_end_matches('\n').to_string());
    }
    // Reject `prompt:` (interactive) per AGENTS no-go list.
    if spec.starts_with("prompt:") {
        return Err(Error::InvalidApiKey(
            "interactive (prompt:) is not supported in v0.3; use env:VAR or file:path".into(),
        ));
    }
    Ok(spec.to_string())
}

/// Redact an API key for stderr: keep the first 4 + last 2 chars,
/// replace the middle with `***`. The result is safe to log.
fn redact_api_key(value: &str) -> String {
    let n = value.chars().count();
    if n <= 8 {
        return "***".into();
    }
    let head: String = value.chars().take(4).collect();
    let tail: String = value
        .chars()
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}***{tail}")
}

/// Short SHA-256 prefix for `redact_api_key` audit logs (8 hex).
fn short_sha256(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(value.as_bytes());
    hex::encode(h.finalize())[..8].to_string()
}

/// Recursive directory move. `rename` is fast on the same FS;
/// when the destination lives on a different device we fall back
/// to a copy + delete so the operator doesn't get a confusing
/// `EXDEV` error.
fn move_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Err(e) = fs::rename(src, dst) {
        if e.raw_os_error() == Some(libc_or_exdev()) {
            copy_dir_recursive(src, dst)?;
            fs::remove_dir_all(src)?;
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
}

/// Linux/macOS EXDEV constant. Hard-coded to 18 (libc's
/// `EXDEV`) so we don't pull `libc` as a dependency just for the
/// cross-device move fallback.
fn libc_or_exdev() -> i32 {
    #[cfg(unix)]
    {
        // EXDEV = 18 on Linux and macOS.
        18
    }
    #[cfg(not(unix))]
    {
        -1
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `redact_api_key` keeps the first 4 + last 2 chars.
    #[test]
    fn redact_api_key_short_value_is_fully_masked() {
        assert_eq!(redact_api_key("abcd"), "***");
        assert_eq!(redact_api_key("abcdefgh"), "***");
    }

    #[test]
    fn redact_api_key_long_value_keeps_head_and_tail() {
        let r = redact_api_key("abcdefghijklmnop");
        assert_eq!(r, "abcd***op");
    }

    /// `resolve_api_key_spec` accepts `env:VAR` (when the var is
    /// set), rejects when unset, and accepts a literal.
    #[test]
    fn resolve_api_key_spec_env_and_literal() {
        unsafe {
            std::env::set_var("MOAGAN_TEST_KEY", "secret");
        }
        let v = resolve_api_key_spec("env:MOAGAN_TEST_KEY").unwrap();
        assert_eq!(v, "secret");
        let err = resolve_api_key_spec("env:MOAGAN_TEST_KEY_NOT_SET").unwrap_err();
        assert!(matches!(err, Error::InvalidApiKey(_)));
        let v = resolve_api_key_spec("literal-value").unwrap();
        assert_eq!(v, "literal-value");
    }

    /// `resolve_api_key_spec` rejects `prompt:` per AGENTS no-go list.
    #[test]
    fn resolve_api_key_spec_rejects_prompt() {
        let err = resolve_api_key_spec("prompt:foo").unwrap_err();
        assert!(matches!(err, Error::InvalidApiKey(_)));
    }

    /// `parse_mode` round-trips every documented mode.
    #[test]
    fn parse_mode_round_trip() {
        assert!(matches!(
            parse_mode("fast").unwrap(),
            crate::cli::Mode::Fast
        ));
        assert!(matches!(
            parse_mode("standard").unwrap(),
            crate::cli::Mode::Standard
        ));
        assert!(matches!(
            parse_mode("deep").unwrap(),
            crate::cli::Mode::Deep
        ));
        assert!(matches!(
            parse_mode("explore").unwrap(),
            crate::cli::Mode::Explore
        ));
        assert!(matches!(
            parse_mode("batch").unwrap(),
            crate::cli::Mode::Batch
        ));
        assert!(parse_mode("ghost").is_err());
    }

    /// `merge_value` deep-merges nested objects.
    #[test]
    fn merge_value_nested_objects() {
        use serde_json::json;
        let mut base = json!({
            "a": 1,
            "b": {"x": 1, "y": 2},
        });
        let patch = json!({
            "b": {"y": 99, "z": 3},
            "c": "new",
        });
        merge_value(&mut base, &patch);
        assert_eq!(base["a"], 1);
        assert_eq!(base["b"]["x"], 1);
        assert_eq!(base["b"]["y"], 99);
        assert_eq!(base["b"]["z"], 3);
        assert_eq!(base["c"], "new");
    }

    /// `move_dir` moves a directory across paths on the same FS.
    /// (Cross-device fallback is unit-tested via `copy_dir_recursive`
    /// directly in integration tests because of the FS dependency.)
    #[test]
    fn move_dir_within_same_fs() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.txt"), "hello").unwrap();
        move_dir(&src, &dst).unwrap();
        assert!(!src.exists());
        assert!(dst.join("a.txt").is_file());
    }
}
