//! `moagan continue`, `moagan resume`, `moagan rerun`, `moagan refine`,
//! `moagan rerank` — run-state operations on an existing run.
//!
//! In v0.1 the first three are still stubs (return
//! `Error::InvalidState` with a friendly message); the latter two are
//! functional: `refine` re-runs the deliver phase for a specific
//! proposal, and `rerank` re-runs the rank phase from the existing
//! evaluation sidecars.

use std::sync::Arc;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::telemetry::Telemetry;

/// Stub for `moagan continue`. Returns a friendly "not yet" error.
pub fn run_continue(run_id: RunId) -> Result<()> {
    Err(Error::InvalidState(format!(
        "continue for {run_id} not yet implemented; v0.2 will resume from manifest"
    )))
}

/// Stub for `moagan resume`.
pub fn run_resume(run_id: RunId) -> Result<()> {
    Err(Error::InvalidState(format!(
        "resume for {run_id} not yet implemented; v0.2 will resume mid-phase"
    )))
}

/// Stub for `moagan rerun`.
pub fn run_rerun(run_id: RunId) -> Result<()> {
    Err(Error::InvalidState(format!(
        "rerun for {run_id} not yet implemented; v0.2 will clone the run config"
    )))
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
) -> Result<std::path::PathBuf> {
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
    let phase = super::super::phases::RankPhase { config: cfg_arc };
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
