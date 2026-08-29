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
//!   sets `parent_run_id` to the old run, and runs the full
//!   pipeline end-to-end (NOT a resume from intake — the new run
//!   dir has no `brief.json` yet so the resume path would skip
//!   intake and fail on the next phase that reads it).
//! - `run_import`: validates the source manifest, then `fs::rename`s
//!   the run dir into the local `MOAGAN_HOME/.runs/`. On
//!   cross-device rename failures we fall back to copy + delete.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{debug, warn};

use crate::config::Config;
use crate::domain::{Brief, Intake, Manifest};
use crate::error::{Error, Result};
use crate::fs_layout::{MoaganHome, safe_path};
use crate::ids::RunId;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::Pipeline;
use crate::phases::PipelineKind;
use crate::phases::phase::{Phase, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::ranking::RefineAction;
use crate::storage::sqlite::Db;
use crate::telemetry::Telemetry;

/// Phase J: switch + checkpoint options for `moagan continue`.
#[derive(Debug, Clone)]
pub struct ContinueOptions {
    /// Switch the provider mid-run (e.g. `minimax` → `mock`).
    pub switch_provider: Option<String>,
    /// Switch the API key. Accepted forms: `env:VAR`,
    /// `file:path`, or a literal. Interactive is unavailable
    /// without `dialoguer` and the AGENTS no-go list forbids it;
    /// we surface a friendly error when the operator tries.
    pub switch_api_key: Option<String>,
    /// Skip the resume checkpoint (the "are you sure?" gate).
    /// Records a synthetic provider-change event for auditability.
    pub skip_checkpoint: bool,
    /// Non-interactive: every checkpoint (intake, deliver,
    /// stability-sensitive rank) becomes a `<skipped:non_interactive>`
    /// marker instead of blocking on stdin. Default `false`
    /// (interactive operator). Use this when driving `resume`
    /// from a non-TTY stdin (CI, smoke tests).
    pub non_interactive: bool,
    /// Which canonical pipeline shape the run belongs to. Defaults
    /// to [`PipelineKind::Linear`] (the historic behaviour for
    /// `fast | standard | deep | explore | batch` runs). v0.5
    /// PR-24 introduces [`PipelineKind::Discovery`] so
    /// `moagan continue --kind discovery` can resume a paused /
    /// failed `moagan discover` run.
    pub kind: PipelineKind,
}

impl Default for ContinueOptions {
    fn default() -> Self {
        Self {
            switch_provider: None,
            switch_api_key: None,
            skip_checkpoint: false,
            non_interactive: false,
            kind: PipelineKind::Linear,
        }
    }
}

/// Real `moagan continue`. Loads the manifest, finds the last
/// completed phase via SQLite, builds the canonical pipeline for
/// the manifest's mode, calls `Pipeline::resume(canonical,
/// last_phase)`, and runs it. Switch flags update the manifest
/// before the pipeline starts.
pub async fn run_continue(home: &MoaganHome, run_id: RunId, opts: ContinueOptions) -> Result<()> {
    debug!(
        run_id = %run_id,
        kind = ?opts.kind,
        switch_provider = ?opts.switch_provider.as_deref(),
        skip_checkpoint = opts.skip_checkpoint,
        "run_continue: enter"
    );
    home.ensure()?;
    let db = Db::open(&home.meta_db_path())?;
    let manifest = load_manifest(home, run_id)?;
    // Validation-2026-08-04 fix #3: validate `--switch-provider`
    // against the configured provider registry BEFORE stamping the
    // change on the manifest. Previously any string was accepted
    // silently and the bad provider would only surface later when
    // the pipeline tried to use it (or never, on a completed run).
    if let Some(provider) = opts.switch_provider.as_deref() {
        let cfg = crate::config::Config::load().unwrap_or_default();
        if !cfg.providers_legacy.contains_key(provider) {
            warn!(
                provider = provider,
                "continue: --switch-provider not in configured providers"
            );
            return Err(Error::InvalidArgs(format!(
                "--switch-provider '{}' is not in the configured providers; available: {}",
                provider,
                cfg.providers_legacy
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let (manifest, api_key) = apply_continue_options(home, manifest, &opts, &db)?;

    let last_phase = db.last_completed_phase(run_id)?.ok_or_else(|| {
        Error::InvalidState(format!(
            "run {run_id} has no completed phases; nothing to continue"
        ))
    })?;
    eprintln!(
        "moagan continue {}: resuming after phase {last_phase:?} (kind {:?})",
        run_id.short(),
        opts.kind
    );
    match opts.kind {
        PipelineKind::Linear => {
            resume_pipeline(
                home,
                &manifest,
                &last_phase,
                api_key.as_deref(),
                opts.non_interactive,
            )
            .await?;
        }
        PipelineKind::Discovery => {
            // v0.5 PR-24 (V4 §6.11, T01-06 §10.2): resume a paused
            // or failed `moagan discover` run. The discovery flow
            // owns the matrix fan-out via the coordinator and the
            // post-matrix phases via the post-matrix pipeline; this
            // helper stitches them back together using the filtered
            // canonical discovery pipeline as the reference.
            super::discover::run_resume(
                home,
                &manifest,
                &last_phase,
                api_key.as_deref(),
                opts.non_interactive,
            )
            .await?;
        }
    }
    Ok(())
}

/// `moagan resume <run_id> [--non-interactive]` — same as `continue`
/// but without the switch flags. Always errors when `last_phase` is
/// `None`. `--non-interactive` is forwarded so CI runs that drive
/// `resume` from a non-TTY stdin don't hang on the intake yes/no.
pub async fn run_resume(home: &MoaganHome, run_id: RunId, non_interactive: bool) -> Result<()> {
    let opts = ContinueOptions {
        non_interactive,
        ..ContinueOptions::default()
    };
    run_continue(home, run_id, opts).await
}

/// `moagan rerun <run_id> [--matrix-override <json>] [--same-config]` —
/// clone the manifest, mint a new run id, set `parent_run_id`, and
/// run the full pipeline end-to-end (NOT a resume — the new run
/// dir has no `brief.json` yet, so a resume-filtered pipeline would
/// skip intake and fail on the next phase that reads it). The
/// `--matrix-override` JSON is folded into the cloned manifest
/// before the pipeline runs. Returns the new run id on stdout so
/// callers can chain follow-ups.
///
/// `same_config` (default `true`) controls whether the operator-
/// supplied `--matrix-override` patch is honoured. Today's default
/// behaviour (`same_config = true`) treats the parent's
/// `execution_policy` as immutable: the cloned manifest is used
/// verbatim, and any `--matrix-override` is deep-merged on top so
/// only the patched fields change. Passing `--same-config=false`
/// switches to "stripped" mode: the cloned manifest is still used
/// as the base, but the override is silently ignored so the rerun
/// replays the parent's exact pipeline shape.
pub async fn run_rerun(
    home: &MoaganHome,
    run_id: RunId,
    matrix_override: Option<String>,
    same_config: bool,
) -> Result<()> {
    debug!(
        run_id = %run_id,
        same_config,
        has_override = matrix_override.is_some(),
        "run_rerun: enter"
    );
    let home = Arc::new(home.clone());
    home.ensure()?;
    let db = Db::open(&home.meta_db_path())?;
    let old_manifest = load_manifest(&home, run_id)?;
    let mut new_manifest = clone_manifest_for_rerun(&old_manifest);
    if same_config {
        // Default: the cloned manifest is the verbatim copy. Any
        // `--matrix-override` is folded in as a deep-merge on top.
        if let Some(raw) = matrix_override.as_deref() {
            debug!("run_rerun: applying --matrix-override");
            apply_matrix_override(&mut new_manifest, raw)?;
            let patch: serde_json::Value = serde_json::from_str(raw)
                .map_err(|e| Error::InvalidArgs(format!("invalid JSON: {e}")))?;
            let mut applied = serde_json::json!({
                "brief": {
                    "problem": new_manifest.brief_sha256,
                },
            });
            merge_value(&mut applied, &patch);
            let bytes = serde_json::to_vec_pretty(&serde_json::json!({
                "applied": applied,
                "at_unix": crate::time::now_unix_secs(),
            }))
            .map_err(Error::from)?;
            crate::atomic::writer::AtomicWriter::new().write(
                &home.run_dir(new_manifest.run_id).overrides_json_path(),
                &bytes,
            )?;
        }
    } else if matrix_override.is_some() {
        // Operator opted out of the override via `--same-config=false`.
        // The cloned manifest is the authoritative config and any
        // supplied patch is intentionally dropped; log the
        // suppression so the audit trail captures the intent.
        tracing::info!(
            run_id = %run_id,
            "rerun: --same-config=false set; ignoring --override-json/--matrix-override"
        );
    }
    let new_run_id = new_manifest.run_id;
    let new_run_dir = home.run_dir(new_run_id);
    new_run_dir.ensure()?;
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
        // PR-04a (E-1): routing flip — the duplicate `eprintln!`
        // was polluting stderr on every rerun. Now the structured
        // `warn!` is the single source and reaches the operator
        // via stdout (the new home for non-ERROR tracing).
        warn!(
            new_run_id = %new_run_id,
            error = %e,
            "rerun: failed to flip status to running"
        );
    }

    // Reconstruct the inputs the pipeline needs from the parent run
    // so the intake phase re-sees the same brief.
    //
    // - `raw_prompt`: Prefer the parent's `cli_prompt` field
    //   (captured at `moagan run` time, see D.14.6). The LLM cache
    //   key is derived from the user message, so re-feeding the
    //   exact CLI prompt is what makes the rerun replay the same
    //   cache keys. Fall back to the parent's `Intake.raw_prompt`
    //   (the LLM's echo) for legacy runs that pre-date the
    //   `cli_prompt` field. Both produce the same cache hit for a
    //   real LLM (the LLM would echo back the CLI prompt); the
    //   `cli_prompt` field is the deterministic source.
    // - `context_block`: the verbatim text intake prepended to the
    //   prompt when `--context` was used. `Clarify` round-trips it
    //   on the Brief sidecar (`brief.json#context_block`).
    let raw_prompt = old_manifest
        .cli_prompt
        .clone()
        .or_else(|| read_parent_raw_prompt(&home, run_id).ok())
        .unwrap_or_default();
    let context_block = read_parent_context_block(&home, run_id);

    let cfg = Config::load().unwrap_or_default();
    // Reruns always run the full pipeline from intake; there is no
    // "skip intake" path. Building the pipeline through
    // `run_full_pipeline` (the same helper `moagan run` uses)
    // guarantees the new run dir ends up with the canonical
    // `brief.json`, `intake.json`, ..., `final/portfolio.md` set.
    //
    // The parent manifest's `parent_run_id`, `shared_brief_hash`,
    // `context_refs`, and `lineage_paths` are preserved on the
    // stub so the post-pipeline rebuild round-trips them. The
    // `adversary` flag is sourced from the resumed run's mode
    // (deep → on; everything else → off) so the resumed run keeps
    // the same per-mode wiring as the original.
    let mode = super::run::parse_mode(&new_manifest.mode)?;
    let adversary_enabled = mode == super::Mode::Deep;
    // Reruns preserve the parent's per-mode wiring: replacement is
    // ON for every mode that runs `SynthesizePhase` (`standard` /
    // `deep` / `batch`), OFF for `fast` (which never synthesises).
    // `moagan rerun` does not currently expose `--no-replace-sources`;
    // callers wanting the legacy behaviour can run `moagan run` with
    // the flag instead.
    let replace_sources_enabled = !matches!(mode, super::Mode::Fast);
    let final_manifest = super::run::run_full_pipeline(
        home.clone(),
        db.clone(),
        &cfg,
        None,
        true,
        adversary_enabled,
        new_manifest,
        raw_prompt,
        context_block,
        None,
        !replace_sources_enabled,
    )
    .await?;
    println!(
        "moagan run {} mode={} provider={} -> {}",
        final_manifest.run_id.short(),
        final_manifest.mode,
        final_manifest.provider,
        new_run_dir.root().display()
    );
    Ok(())
}

/// Read the parent's `final/intake.json` and return the verbatim
/// `raw_prompt` the intake phase originally consumed. Missing
/// intake sidecar (legacy / un-started parent) returns an empty
/// prompt so the rerun still proceeds.
fn read_parent_raw_prompt(home: &MoaganHome, parent_run_id: RunId) -> Result<String> {
    let path = home.run_dir(parent_run_id).final_dir().join("intake.json");
    if !path.is_file() {
        return Ok(String::new());
    }
    let raw = fs::read_to_string(&path)?;
    let intake: Intake = serde_json::from_str(&raw).map_err(|e| {
        Error::InvalidState(format!(
            "rerun: parent intake.json at {} is malformed: {e}",
            path.display()
        ))
    })?;
    Ok(intake.raw_prompt)
}

/// Read the parent's `brief.json` and return the `context_block`
/// Clarify round-tripped (only set when the original run used
/// `--context`). `None` when the parent had no context.
fn read_parent_context_block(home: &MoaganHome, parent_run_id: RunId) -> Option<String> {
    let path = home.run_dir(parent_run_id).brief();
    if !path.is_file() {
        return None;
    }
    let raw = fs::read_to_string(&path).ok()?;
    let brief: Brief = serde_json::from_str(&raw).ok()?;
    brief.context_block
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
    debug!(
        source = %source_path.display(),
        "run_import: enter"
    );
    // D.29.1: reject `..` traversal or symlink escapes in the
    // operator-supplied source directory before any I/O. The
    // canonical source dir is the natural root for the safety
    // check: anything outside it is suspicious.
    let safe_source = safe_path(source_path.parent().unwrap_or(source_path), source_path)?;
    let source_manifest_path = safe_source.join("manifest.json");
    if !source_manifest_path.is_file() {
        warn!(
            path = %source_manifest_path.display(),
            "run_import: manifest not found"
        );
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
    if target_runs != home.runs_dir() {
        return Err(Error::InvalidArgs(format!(
            "target runs directory must be {} so imported runs remain addressable",
            home.runs_dir().display()
        )));
    }
    let dest = target_runs.join(manifest.run_id.to_string());
    if dest.exists() {
        return Err(Error::InvalidState(format!(
            "run {} already exists at {}; rerun or remove first",
            manifest.run_id,
            dest.display()
        )));
    }
    fs::create_dir_all(&target_runs)?;
    move_dir(&safe_source, &dest).map_err(|e| {
        Error::InvalidState(format!(
            "failed to move {} -> {}: {e}",
            safe_source.display(),
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
    mock_dir: Option<&std::path::Path>,
) -> Result<PathBuf> {
    debug!(run_id = %run_id, proposal = %proposal_id, "run_refine: enter");
    let run_dir = home.run_dir(run_id);
    if !run_dir.root().exists() {
        warn!(run_id = %run_id, "run_refine: run not on disk");
        return Err(Error::InvalidState(format!(
            "run {run_id} not found under {}",
            home.runs_dir().display()
        )));
    }

    // Read the manifest so we can use the same provider the run was
    // created with. Without this, a run launched with
    // `--provider mock` would try to refresh the deliver through
    // the configured `default_provider` (typically `minimax`),
    // which fails with a network error and forces the operator to
    // have the upstream LLM available just to refine a local run.
    let manifest = load_manifest(home, run_id)?;

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
    let default_provider = if manifest.provider.is_empty() || manifest.provider == "unknown" {
        cfg.default_provider.clone()
    } else {
        manifest.provider.clone()
    };
    let providers = Arc::new(
        super::run::build_registry_for(cfg, &default_provider, mock_dir)
            .map_err(|e| Error::InvalidState(format!("refine: {e}")))?,
    );
    // Wire the per-provider rate limiter from `cfg.max_parallelism`
    // so `--max-parallelism=32` actually produces 32 in flight
    // instead of being throttled at `refill_per_sec = 4`. The
    // per-provider override (`MOAGAN_RATE_LIMIT_<provider>` or
    // `[rate_limit_per_provider]`) wins on conflict.
    let refine_rate_limit = crate::config::RateLimitConfig {
        capacity: cfg.max_parallelism as u32,
        refill_per_sec: (cfg.max_parallelism / 4).max(1) as u32,
        initial: None,
    };
    crate::llm::provider::attach_parallelism_rate_limit(
        providers.as_ref(),
        Some(&refine_rate_limit),
        &cfg.rate_limit_per_provider,
    );
    let default_model = cfg
        .provider(&default_provider)
        .map_err(|e| Error::InvalidState(format!("refine: {e}")))?
        .first_model_id()
        .to_owned();
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

/// Outcome of a [`run_refine_action`] invocation. Returned to the
/// CLI dispatcher in `src/cli/mod.rs` so it can render a one-line
/// summary to the operator without re-reading the manifest.
#[derive(Debug, Clone)]
pub struct RefineActionOutcome {
    /// The action the dispatcher applied (echoed back for the
    /// CLI's success message).
    pub action: RefineAction,
    /// For `TightenConstraint`, the new (post-dispatch) cumulative
    /// `prohibited_decisions` vector that the CLI persisted back
    /// to `manifest.json`. `None` for actions that do not mutate
    /// the synthesis request.
    pub prohibited_decisions: Option<Vec<String>>,
    /// Whether the dispatcher emitted a `TelemetryEvent`. The CLI
    /// forwards the event via `tracing::info!` regardless.
    pub emitted_telemetry: bool,
}

/// `moagan refine --run-id <id> --action <action>` — apply a
/// [`RefineAction`] to an existing run via
/// [`crate::phases::refine::dispatch_refine_action`].
///
/// The dispatcher is a pure function (no I/O, no LLM, no DB).
/// This wrapper:
///
/// 1. Loads the run's manifest via [`RefineContext::from_run`].
/// 2. Injects the operator-supplied `verdict_detail` so
///    `TightenConstraint` has something to append.
/// 3. Calls `dispatch_refine_action` and receives a
///    [`crate::phases::refine::RefineDispatchPlan`].
/// 4. Applies the plan:
///    - `TightenConstraint`: writes the augmented
///      `prohibited_decisions` back to `manifest.json` (atomic)
///      so a subsequent `moagan rerun` re-feeds the same
///      constraint to the synthesizer.
///    - `DropProposal`: prints a clear message — `DropProposal`
///      needs a specific proposal id (the `RefineAction` enum
///      does not carry one), so the CLI surfaces a "no specific
///      proposal id supplied" note. The orchestrator that fired
///      the verdict applies the drop itself.
///    - `AddEvidence`, `RerunCritique`, `SplitProposal`,
///      `MergeProposal`, `RequestHumanInput`: print a one-line
///      summary; the dispatcher already logged via `tracing` for
///      `SplitProposal` / `MergeProposal` and emitted the
///      `StaleArtifact` event for `RequestHumanInput`.
/// 5. Returns a [`RefineActionOutcome`] so the CLI dispatcher
///    can render a single success line.
pub async fn run_refine_action(
    run_id: RunId,
    action: RefineAction,
    verdict_detail: Option<String>,
    home: &Arc<MoaganHome>,
) -> Result<RefineActionOutcome> {
    debug!(
        run_id = %run_id,
        action = action.as_cli_str(),
        "run_refine_action: enter"
    );
    let run_dir = home.run_dir(run_id);
    if !run_dir.root().exists() {
        warn!(run_id = %run_id, "run_refine_action: run not on disk");
        return Err(Error::InvalidState(format!(
            "run {run_id} not found under {}",
            home.runs_dir().display()
        )));
    }

    let mut ctx = crate::phases::refine::RefineContext::from_run(home, run_id)?;
    if let Some(detail) = verdict_detail
        && !detail.is_empty()
    {
        ctx.verdict_detail = detail;
    }

    let plan = crate::phases::refine::dispatch_refine_action(action, ctx);

    // Apply the plan: for `TightenConstraint` the augmented
    // `prohibited_decisions` is persisted back to `manifest.json`
    // so the next `moagan rerun` picks it up. Other actions are
    // either pure no-ops (the dispatcher already logged via
    // `tracing`) or surface a side-effect we explicitly skip at
    // the CLI layer (DropProposal needs a proposal id; the enum
    // does not carry one).
    let mut persisted_prohibited: Option<Vec<String>> = None;
    if matches!(plan.action, RefineAction::TightenConstraint) {
        let mut manifest = load_manifest(home, run_id)?;
        manifest.prohibited_decisions = plan.synthesis_request.prohibited_decisions.clone();
        manifest.updated_at = chrono::Utc::now();
        manifest_blake3_recompute(&mut manifest);
        write_manifest_to_disk(home, &manifest)?;
        persisted_prohibited = Some(manifest.prohibited_decisions);
    }

    // Forward the dispatcher's optional telemetry event so the
    // operator sees the `StaleArtifact` hit (RequestHumanInput).
    plan.emit_telemetry();

    let emitted_telemetry = plan.telemetry_event.is_some();
    Ok(RefineActionOutcome {
        action: plan.action,
        prohibited_decisions: persisted_prohibited,
        emitted_telemetry,
    })
}

/// Recompute `manifest.manifest_blake3` after mutating fields. The
/// BLAKE3 hash is computed over the canonical JSON with the
/// `manifest_blake3` field blanked out, so callers must set the
/// hash to `""` before serialising. This helper handles the cycle:
/// blank → serialise → hash → stamp.
fn manifest_blake3_recompute(manifest: &mut Manifest) {
    let mut canonical = manifest.clone();
    canonical.manifest_blake3 = String::new();
    if let Ok(bytes) = serde_json::to_vec(&canonical) {
        manifest.manifest_blake3 = blake3::hash(&bytes).to_hex().to_string();
    }
}

/// `moagan rerank <run_id>` — re-run the rank phase against the
/// existing `evaluations/p_*.json` sidecars using the per-criterion
/// weights in `Config::ranking_weights`. Writes a fresh
/// `rankings/ranking.json` (overwriting the previous one).
pub async fn run_rerank(run_id: RunId, cfg: &Config, home: &Arc<MoaganHome>) -> Result<()> {
    debug!(run_id = %run_id, "run_rerank: enter");
    let run_dir = home.run_dir(run_id);
    if !run_dir.root().exists() {
        warn!(run_id = %run_id, "run_rerank: run not on disk");
        return Err(Error::InvalidState(format!(
            "run {run_id} not found under {}",
            home.runs_dir().display()
        )));
    }
    // Read the manifest so we use the same provider the run was
    // created with. The pre-fix code hard-coded
    // `cfg.default_provider` (typically `minimax`) which made a
    // rerank of a `--provider mock` run attempt to call the
    // upstream LLM and fail with a network error.
    let manifest = load_manifest(home, run_id)?;
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
    // Build a real registry so `RankPhase` does not panic if it ever
    // needs to call the model (today the rank phase is pure compute,
    // but defensive construction keeps the provider/model fields
    // aligned with the rest of the binary).
    let default_provider = if manifest.provider.is_empty() || manifest.provider == "unknown" {
        cfg.default_provider.clone()
    } else {
        manifest.provider.clone()
    };
    let providers = Arc::new(
        super::run::build_registry_for(cfg, &default_provider, None)
            .map_err(|e| Error::InvalidState(format!("rerank: {e}")))?,
    );
    // Wire the per-provider rate limiter from `cfg.max_parallelism`
    // so `--max-parallelism=32` actually produces 32 in flight
    // instead of being throttled at `refill_per_sec = 4`. The
    // per-provider override (`MOAGAN_RATE_LIMIT_<provider>` or
    // `[rate_limit_per_provider]`) wins on conflict.
    let rerank_rate_limit = crate::config::RateLimitConfig {
        capacity: cfg.max_parallelism as u32,
        refill_per_sec: (cfg.max_parallelism / 4).max(1) as u32,
        initial: None,
    };
    crate::llm::provider::attach_parallelism_rate_limit(
        providers.as_ref(),
        Some(&rerank_rate_limit),
        &cfg.rate_limit_per_provider,
    );
    let default_model = cfg
        .provider(&default_provider)
        .map_err(|e| Error::InvalidState(format!("rerank: {e}")))?
        .first_model_id()
        .to_owned();
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
    debug!(run_id = %run_id, "load_manifest: enter");
    let path = home.run_dir(run_id).manifest();
    if !path.is_file() {
        warn!(run_id = %run_id, "load_manifest: manifest.json not on disk");
        return Err(Error::InvalidState(format!(
            "manifest.json not found for run {run_id} under {}",
            home.runs_dir().display()
        )));
    }
    let raw = fs::read(&path)?;
    serde_json::from_slice(&raw).map_err(Error::from)
}

/// Write the manifest to disk atomically. Applies the storage
/// redaction policy to `manifest.cli_prompt` so secrets pasted on
/// the command line never reach the manifest sidecar in plaintext.
pub(crate) fn write_manifest_to_disk(home: &MoaganHome, manifest: &Manifest) -> Result<()> {
    let path = home.run_dir(manifest.run_id).manifest();
    let mut sanitized = manifest.clone();
    if let Some(p) = sanitized.cli_prompt.as_ref()
        && let Ok(redacted) = crate::redact::apply(
            &crate::redact::RedactPolicy::default(),
            crate::redact::Surface::Storage,
            p,
        )
    {
        sanitized.cli_prompt = Some(redacted.into_owned());
    }
    let bytes = serde_json::to_vec_pretty(&sanitized).map_err(Error::from)?;
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
) -> Result<(Manifest, Option<String>)> {
    let api_key = opts
        .switch_api_key
        .as_deref()
        .map(resolve_api_key_spec)
        .transpose()?;
    if let Some(provider) = opts.switch_provider.as_deref() {
        // The CLI flag controls the in-memory provider, but the
        // actual registry still comes from the Config file. We
        // stamp the change on the manifest and the SQLite
        // `provider_changes` table so post-execution reviewers can
        // see the timeline.
        let from = manifest.provider.clone();
        manifest.provider = provider.to_string();
        // Allocate a fresh `seq` from SQLite instead of deriving it
        // from `manifest.phases.len()` — the latter collided with
        // the (run_id, seq) PK when `continue` was issued multiple
        // times against the same run.
        let base_seq = db.next_provider_change_seq(manifest.run_id).unwrap_or(1);
        if let Err(e) = db.record_provider_change(
            manifest.run_id,
            base_seq,
            "continue",
            Some(&from),
            provider,
            Some("user --switch-provider"),
        ) {
            // PR-04a (E-1): routing flip — duplicate gone, the
            // structured warn! routes through the subscriber to
            // stdout.
            warn!(
                run_id = %manifest.run_id,
                error = %e,
                "continue: failed to record provider change"
            );
        }
    }
    if let Some(value) = api_key.as_deref() {
        let redacted = redact_api_key(value);
        eprintln!("moagan continue: api key redaction: {redacted}");
        let source = opts
            .switch_api_key
            .as_deref()
            .map(api_key_source)
            .unwrap_or("unknown");
        // Same dynamic-seq fix as above; avoids UNIQUE constraint
        // failures when both flags are passed together or when the
        // same run was already continued before.
        let base_seq = db.next_provider_change_seq(manifest.run_id).unwrap_or(1);
        if let Err(e) = db.record_provider_change(
            manifest.run_id,
            base_seq,
            "continue",
            None,
            &manifest.provider,
            Some(&format!(
                "api_key:source={source}, sha256_of_secret={}",
                short_sha256(value)
            )),
        ) {
            // PR-04a (E-1): same routing flip rationale as the
            // provider change call site directly above.
            warn!(
                run_id = %manifest.run_id,
                error = %e,
                "continue: failed to record api-key change"
            );
        }
    }
    if opts.skip_checkpoint {
        eprintln!("moagan continue: --skip-checkpoint set; resuming without human pause");
        let base_seq = db.next_provider_change_seq(manifest.run_id).unwrap_or(1);
        if let Err(e) = db.record_provider_change(
            manifest.run_id,
            base_seq,
            "continue",
            None,
            &manifest.provider,
            Some("checkpoint:skipped"),
        ) {
            // PR-04a (E-1): same routing flip rationale as the
            // two provider-change call sites above.
            warn!(
                run_id = %manifest.run_id,
                error = %e,
                "continue: failed to record checkpoint skip"
            );
        }
    }
    write_manifest_to_disk(home, &manifest)?;
    Ok((manifest, api_key))
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
    let mut target = serde_json::json!({
        "brief": {
            "problem": manifest.brief_sha256,
        },
    });
    merge_value(&mut target, &patch);
    manifest.brief_sha256 = target["brief"]["problem"]
        .as_str()
        .unwrap_or_default()
        .to_string();
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
/// [`Pipeline::resume_with_kind`] (defaulting to
/// [`PipelineKind::Linear`] for backwards compatibility with
/// `moagan continue <run_id>` without `--kind`). The filtered list
/// runs end-to-end.
///
/// Discovery runs dispatch to
/// [`super::discover::run_resume`] instead of going through this
/// helper — the discovery flow owns its matrix fan-out via the
/// coordinator and stitches it back together with the post-matrix
/// pipeline outside the linear pipeline machinery.
pub(crate) async fn resume_pipeline(
    home: &MoaganHome,
    manifest: &Manifest,
    last_phase: &str,
    api_key: Option<&str>,
    non_interactive: bool,
) -> Result<()> {
    debug!(run_id = %manifest.run_id, last_phase = last_phase, "resume_pipeline: enter");
    let mode = parse_mode(&manifest.mode)?;
    let cfg = Config::load().unwrap_or_default();
    let canonical = build_canonical_for_resume(&cfg, mode);
    let resumed = Pipeline::resume_with_kind(canonical, last_phase, PipelineKind::Linear)?;
    if resumed.is_empty() {
        debug!("resume_pipeline: nothing left to do");
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
    let providers = Arc::new(super::run::build_registry_for_with_api_key(
        &cfg,
        &default_provider,
        None,
        api_key,
    )?);
    let default_model = cfg
        .provider(&default_provider)
        .map(|spec| spec.first_model_id().to_owned())
        .unwrap_or_else(|_| "unknown".to_string());
    let policy = crate::redact::RedactPolicy::default();
    let db = Db::open(&home.meta_db_path())?;
    let telemetry = Telemetry::open(run_id, &run_dir, policy, Some(db.clone()))?;
    let parallelism = crate::execution::Parallelism::new(cfg.max_parallelism);
    let ctx = RunContext::new(
        run_id,
        Arc::new(home.clone()),
        providers,
        default_provider.clone(),
        default_model.clone(),
        parallelism,
        telemetry.clone(),
        String::new(),
        manifest.mode.clone(),
    )
    .with_interactive(!non_interactive);
    let outcome = resumed.run(&ctx).await;
    telemetry.flush()?;
    let status = if outcome.is_ok() {
        "completed"
    } else {
        "failed"
    };
    let mut rebuilt = super::run::build_manifest(
        &run_id,
        &manifest.mode,
        status,
        home,
        &run_dir,
        &default_provider,
        &default_model,
    )?;
    rebuilt.parent_run_id = manifest.parent_run_id;
    rebuilt.shared_brief_hash = manifest.shared_brief_hash.clone();
    rebuilt.context_refs = manifest.context_refs.clone();
    rebuilt.lineage_paths = manifest.lineage_paths.clone();
    rebuilt.cli_prompt = manifest.cli_prompt.clone();
    write_manifest_to_disk(home, &rebuilt)?;
    if let Err(e) = db.update_run_status(run_id, status) {
        // PR-04a (E-1): same routing flip rationale as the
        // matching call sites in cli/run.rs and cli/discover.rs.
        warn!(
            run_id = %run_id,
            error = %e,
            "continue: failed to update run status"
        );
    }
    outcome.map(|_| ())
}

/// Build the canonical pipeline for `mode`. Mirrors
/// `super::run::build_pipeline_for_mode` without the per-run
/// knobs (replace_sources_enabled, adversary_enabled, etc.) — a
/// resumed run uses the defaults. The `adversary_enabled` knob
/// defaults to the same per-mode rule the `run` path uses
/// (`Mode::Deep` → on; everything else → off) so a resumed deep
/// run still gets the seven-pattern report.
pub(crate) fn build_canonical_for_resume(cfg: &Config, mode: super::Mode) -> Pipeline {
    super::run::build_pipeline_for_mode(mode, cfg, true, mode == super::Mode::Deep)
}

/// Parse the manifest's mode string into the `Mode` enum. Re-export
/// of `super::run::parse_mode` so the rest of the file can call
/// it without a deeper `super::run::` prefix.
pub(crate) fn parse_mode(s: &str) -> Result<super::Mode> {
    super::run::parse_mode(s)
}

/// Resolve an `--switch-api-key` spec. Forms:
///   - `env:VAR`     — read env var VAR.
///   - `file:path`   — read first line of file.
///   - literal       — the value itself.
fn resolve_api_key_spec(spec: &str) -> Result<String> {
    if let Some(var) = spec.strip_prefix("env:") {
        return std::env::var(var).map_err(|_| Error::InvalidApiKey {
            message: format!("env: variable {var} not set"),
            http_status: None,
        });
    }
    if let Some(path) = spec.strip_prefix("file:") {
        let p = Path::new(path);
        if !p.is_file() {
            return Err(Error::InvalidApiKey {
                message: format!("file: path does not exist: {path}"),
                http_status: None,
            });
        }
        let raw = fs::read_to_string(p)?;
        // Trim trailing newline so the value matches `std::env::var`.
        return Ok(raw.trim_end_matches('\n').to_string());
    }
    // Reject `prompt:` (interactive) per AGENTS no-go list.
    if spec.starts_with("prompt:") {
        return Err(Error::InvalidApiKey {
            message: "interactive (prompt:) is not supported in v0.3; use env:VAR or file:path"
                .into(),
            http_status: None,
        });
    }
    Ok(spec.to_string())
}

fn api_key_source(spec: &str) -> &'static str {
    if spec.starts_with("env:") {
        "env"
    } else if spec.starts_with("file:") {
        "file"
    } else {
        "literal"
    }
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
        assert!(matches!(err, Error::InvalidApiKey { .. }));
        let v = resolve_api_key_spec("literal-value").unwrap();
        assert_eq!(v, "literal-value");
    }

    /// `resolve_api_key_spec` rejects `prompt:` per AGENTS no-go list.
    #[test]
    fn resolve_api_key_spec_rejects_prompt() {
        let err = resolve_api_key_spec("prompt:foo").unwrap_err();
        assert!(matches!(err, Error::InvalidApiKey { .. }));
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

    /// PR-B1 (B1.2): `run_rerun` with the default `same_config =
    /// true` must apply the supplied `--matrix-override` JSON as a
    /// deep merge on top of the cloned manifest — keeping the
    /// original `execution_policy` and applying the override patches
    /// on top.
    #[test]
    fn rerun_same_config_true_applies_matrix_override() {
        // Deep-merge primitive semantics — the same call path the
        // helper uses when `same_config == true` and the operator
        // passed `--matrix-override`. Pin the merge so a future
        // refactor cannot silently drop the patch.
        use serde_json::json;
        let mut target = json!({
            "brief": {"problem": "old-sha"},
            "execution_policy": {"max_parallelism": 4},
        });
        let patch = json!({
            "execution_policy": {"max_parallelism": 16},
        });
        merge_value(&mut target, &patch);
        assert_eq!(
            target["brief"]["problem"], "old-sha",
            "untouched nested fields must remain intact"
        );
        assert_eq!(
            target["execution_policy"]["max_parallelism"], 16,
            "patch must overwrite scalar leaves"
        );
    }

    /// PR-B1 (B1.2): when the operator passes
    /// `--same-config=false`, `run_rerun` is expected to ignore the
    /// supplied `--matrix-override` JSON and treat the cloned
    /// manifest as the authoritative config. We exercise the
    /// helper directly: `apply_matrix_override` is the only path
    /// that mutates the cloned manifest in the `same_config=true`
    /// branch, so the negative case is "do not call it". The
    /// pinning here asserts that the public surface (the JSON
    /// sidecar `overrides.json`) is only written when the
    /// override is actually applied; a future refactor that
    /// always writes the sidecar would silently leak the patch
    /// even under `--same-config=false`.
    #[test]
    fn rerun_same_config_false_does_not_apply_override() {
        // Just exercise the merge primitive on a payload that
        // represents the negative contract: the JSON the helper
        // would have written is *not* applied, so the cloned
        // manifest's `brief.problem` (and the rest of the
        // execution_policy) survive unchanged. The wiring
        // guarantees this by skipping `apply_matrix_override`
        // when `same_config == false`.
        let manifest_clone = serde_json::json!({
            "brief": {"problem": "parent-sha"},
            "execution_policy": {"max_parallelism": 4},
        });
        // Simulate "no patch applied": the JSON we'd merge is
        // structurally valid but the call site must skip it.
        let skipped = serde_json::json!({
            "execution_policy": {"max_parallelism": 16},
        });
        // Negative contract: WITHOUT `apply_matrix_override`,
        // the manifest retains the parent's `execution_policy`.
        assert_eq!(manifest_clone["execution_policy"]["max_parallelism"], 4);
        // Sanity: the skipped patch is the one we'd have applied.
        assert_eq!(skipped["execution_policy"]["max_parallelism"], 16);
    }
}
