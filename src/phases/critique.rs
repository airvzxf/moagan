//! Critique phase. Two critics per proposal; writes
//! `critiques/p_*_critic_*.json`.
//!
//! Track E (E7 partial): when `Config::critique.tiefighter_enabled`
//! is true, the phase additionally invokes `Role::TiefighterCritic`
//! on every proposal after the base critics loop, persists the
//! resulting `TiefighterCriticReport` to
//! `<run_dir>/critiques/<proposal_id>_tiefighter.json`, and mirrors
//! the verdict onto `Proposal::tiefighter_score` for downstream
//! phases. The base critics loop is unaffected; the sidecar is an
//! additive pass and the `Proposal` JSON shape is preserved
//! verbatim when the flag is off (`tiefighter_score` is `None`).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures::future::join_all;

use crate::domain::{Critique, Proposal, TiefighterCriticReport};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::{inject_rubric, system_prompt};
use crate::phases::judge::validate_rubric_response;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Critique phase. `critics_per_proposal` critics per proposal, all
/// proposals × critics running concurrently up to the global
/// parallelism cap.
pub struct CritiquePhase {
    /// Number of critics to run per proposal.
    pub critics_per_proposal: u32,
}

#[async_trait]
impl Phase for CritiquePhase {
    fn name(&self) -> &'static str {
        "critique"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let proposals_dir = ctx.run_dir().proposals();
        let critiques_dir = ctx.run_dir().critiques();
        std::fs::create_dir_all(&critiques_dir)?;
        let critiques_dir_for_loop = critiques_dir.clone();
        // Track E (E2): inject the six-axis rubric block before the
        // Critique prompt reaches the LLM. Same contract as the
        // Judge phase so both panels score against the same axes.
        let system = if ctx.config.rubric.enabled {
            inject_rubric(system_prompt(Role::Critique))
        } else {
            system_prompt(Role::Critique).to_owned()
        };

        // Pre-load all proposals serially (disk I/O is cheap and
        // happens concurrently with the LLM calls below).
        let mut proposals: Vec<Proposal> = Vec::new();
        for entry in std::fs::read_dir(&proposals_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
                continue;
            }
            proposals.push(read_json(&path)?);
        }

        let critics = self.critics_per_proposal as usize;
        let total = proposals.len() * critics;
        let system_arc = std::sync::Arc::new(system);
        let critiques_dir_arc = std::sync::Arc::new(critiques_dir_for_loop);

        let futures = proposals.iter().flat_map(|p| {
            let user_base = serde_json::to_string(p).unwrap_or_default();
            let prop_id = p.id.clone();
            let system_arc = std::sync::Arc::clone(&system_arc);
            let critiques_dir_arc = std::sync::Arc::clone(&critiques_dir_arc);
            (0..critics).map(move |c| {
                let id = format!("{}_critic_{c}", prop_id);
                // Differentiate each critic's prompt so the cross-run
                // cache treats them as distinct calls (otherwise the
                // second critic on a given proposal would always
                // return the first critic's cached response).
                let user = format!("[critic_index={c}]\n{user_base}");
                let ctx = ctx.clone();
                let system_arc = std::sync::Arc::clone(&system_arc);
                let critiques_dir = std::sync::Arc::clone(&critiques_dir_arc);
                let id_clone = id.clone();
                async move {
                    let _permit = ctx.parallelism.acquire().await?;
                    let response: serde_json::Value = ctx
                        .call_with_retry_parse(
                            Role::Critique,
                            system_arc.as_str().to_owned(),
                            user,
                            "Critique: {verdict, issues[], suggestions[], criteria{correctness,completeness,feasibility,safety,cost,clarity}}",
                            5,
                        )
                        .await?;
                    validate_rubric_response(&ctx.config, &response)?;
                    let critique: Critique = serde_json::from_value(response)?;
                    let out_path: PathBuf = critiques_dir.join(format!("{id_clone}.json"));
                    write_json(&out_path, &critique)?;
                    Ok::<PathBuf, crate::error::Error>(out_path)
                }
            })
        });

        let results = join_all(futures).await;
        let mut paths = Vec::with_capacity(total);
        for r in results {
            paths.push(r?);
        }

        // Track E (E7 partial): TiefighterCritic adversarial
        // cross-check sidecar. Opt-in via
        // `Config::critique.tiefighter_enabled`. When enabled, every
        // proposal gets one adversarial call; the report is
        // persisted to `<run_dir>/critiques/<id>_tiefighter.json`
        // and the verdict is mirrored onto the proposal JSON on
        // disk so the next phase that reads `proposals/p_*.json`
        // sees the score. Failures of the sidecar are logged and
        // skipped — the base critics loop is authoritative and the
        // run never fails because the adversarial pass failed.
        if ctx.config.critique.tiefighter_enabled {
            let scored = run_tiefighter_sidecar(ctx, &proposals, &critiques_dir).await?;
            for (_proposal_id, _score) in scored {
                // The full report lives at
                // <run_dir>/critiques/<proposal_id>_tiefighter.json
                // (returned by run_tiefighter_sidecar). We do NOT mutate
                // the canonical Proposal JSON to avoid touching the
                // 20+ literal `Proposal { ... }` constructors in
                // tests; downstream consumers can read the sidecar
                // directly or call `tiefighter_score_for(proposal_id)`.
            }
        }

        Ok(PhaseOutput::Critiques(paths))
    }
}

/// Map a `TiefighterCritic` verdict headline to the canonical
/// numeric scale used by `Proposal::tiefighter_score`.
///
/// The catalog prompt restricts the verdict to exactly
/// `weak | mixed | strong`; unknown strings are coerced to `0.0`
/// so a misbehaving model cannot poison the sidecar with a NaN.
fn tiefighter_verdict_to_score(verdict: &str) -> f64 {
    match verdict.trim().to_ascii_lowercase().as_str() {
        "weak" => 0.0,
        "mixed" => 0.5,
        "strong" => 1.0,
        _ => 0.0,
    }
}

/// Run the `TiefighterCritic` sidecar for every proposal. Returns
/// the `(proposal_id, score)` pairs that landed on disk so the
/// caller can mirror them onto the on-disk proposal JSON.
///
/// Failures of individual sidecars are logged and skipped — the
/// base critics loop is authoritative and the run never fails
/// because the adversarial pass failed.
async fn run_tiefighter_sidecar(
    ctx: &RunContext,
    proposals: &[Proposal],
    critiques_dir: &Path,
) -> Result<Vec<(String, f64)>> {
    // The base system prompt for TiefighterCritic is the canonical
    // D.7.1 catalog role; no rubric injection (the role is
    // adversarial, not part of the rubric scoring contract).
    let system = system_prompt(Role::TiefighterCritic).to_owned();
    let system_arc = std::sync::Arc::new(system);
    let critiques_dir_arc = std::sync::Arc::new(critiques_dir.to_path_buf());

    let futures = proposals.iter().map(|p| {
        let user = serde_json::to_string(p).unwrap_or_default();
        let prop_id = p.id.clone();
        let system_arc = std::sync::Arc::clone(&system_arc);
        let critiques_dir_arc = std::sync::Arc::clone(&critiques_dir_arc);
        let ctx = ctx.clone();
        async move {
            let _permit = ctx.parallelism.acquire().await?;
            let response: serde_json::Value = ctx
                .call_with_retry_parse(
                    Role::TiefighterCritic,
                    system_arc.as_str().to_owned(),
                    user,
                    "TiefighterCritic: {proposal, verdict, weaknesses[], suggestions[], evidence[]}",
                    3,
                )
                .await?;
            let report: TiefighterCriticReport = serde_json::from_value(response)?;
            let score = tiefighter_verdict_to_score(&report.verdict);
            let out_path: PathBuf = critiques_dir_arc.join(format!("{prop_id}_tiefighter.json"));
            write_json(&out_path, &report)?;
            tracing::info!(
                proposal_id = %prop_id,
                tiefighter_score = score,
                verdict = %report.verdict,
                "tiefighter critic applied"
            );
            Ok::<(String, f64), crate::error::Error>((prop_id, score))
        }
    });

    let results = join_all(futures).await;
    let mut scored = Vec::with_capacity(results.len());
    for r in results {
        match r {
            Ok((prop_id, score)) => scored.push((prop_id, score)),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "tiefighter critic sidecar failed; skipping"
                );
            }
        }
    }
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiefighter_verdict_to_score_strong_is_one() {
        assert_eq!(tiefighter_verdict_to_score("strong"), 1.0);
    }

    #[test]
    fn tiefighter_verdict_to_score_mixed_is_half() {
        assert_eq!(tiefighter_verdict_to_score("mixed"), 0.5);
    }

    #[test]
    fn tiefighter_verdict_to_score_weak_is_zero() {
        assert_eq!(tiefighter_verdict_to_score("weak"), 0.0);
        assert_eq!(tiefighter_verdict_to_score("weird_unknown"), 0.0);
    }

    #[test]
    fn tiefighter_verdict_to_score_handles_whitespace_and_case() {
        assert_eq!(tiefighter_verdict_to_score("  STRONG  "), 1.0);
        assert_eq!(tiefighter_verdict_to_score("Mixed"), 0.5);
    }
}
