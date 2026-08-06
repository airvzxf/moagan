//! Judge phase. For each proposal (or repair if available), gather
//! `judges` judge scores and average them. Writes
//! `evaluations/p_*.json`.
//!
//! Phase D (V4 §5.11 + T01-06 §5.10/§5.13): after the panel runs, the
//! phase computes `disagreement_score` (stddev of the judges' overall
//! scores) and — when the score exceeds `disagreement_threshold` —
//! fires an adversarial pass with the `Adversary` role. The
//! adversary writes `adversaries/p_<id>.json` and the report's
//! `score_delta` is folded into the aggregated evaluation under
//! `Aggregated.adversary_delta`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};

use crate::domain::{AdversaryReport, JudgeScore};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::{inject_rubric, system_prompt};
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::write_json;

/// Default threshold (stddev on a 0..=10 scale) above which the
/// adversary pass is triggered. Tunable via [`JudgePhase::with_threshold`].
pub const DEFAULT_DISAGREEMENT_THRESHOLD: f32 = 0.5;

/// Judge phase. `judges` judge scores per proposal. All proposals ×
/// judges are scheduled concurrently up to the global parallelism
/// cap; each proposal's individual scores are aggregated once its
/// `judges` calls complete. After the panel runs, an optional
/// adversary pass fires per proposal when `disagreement_score`
/// exceeds the configured threshold.
pub struct JudgePhase {
    /// Number of judges per proposal.
    pub judges: u32,
    /// Disagreement threshold for the adversary pass. Default
    /// [`DEFAULT_DISAGREEMENT_THRESHOLD`].
    pub disagreement_threshold: f32,
    /// When `false`, skip the adversary pass entirely (Phase D
    /// opt-out for `fast` mode and `--no-adversary`).
    pub enable_adversary: bool,
}

impl Default for JudgePhase {
    fn default() -> Self {
        Self {
            judges: 3,
            disagreement_threshold: DEFAULT_DISAGREEMENT_THRESHOLD,
            enable_adversary: true,
        }
    }
}

impl JudgePhase {
    /// Construct a `JudgePhase` with a custom threshold.
    pub fn with_threshold(mut self, threshold: f32) -> Self {
        self.disagreement_threshold = threshold;
        self
    }

    /// Compute the disagreement score (population stddev) of the
    /// judge scores. `None` when there are fewer than two scores
    /// (the stddev is undefined).
    pub fn disagreement_score(scores: &[JudgeScore]) -> Option<f32> {
        if scores.len() < 2 {
            return None;
        }
        let n = scores.len() as f32;
        let mean: f32 = scores.iter().map(|s| s.score).sum::<f32>() / n;
        let variance: f32 = scores.iter().map(|s| (s.score - mean).powi(2)).sum::<f32>() / n;
        Some(variance.sqrt())
    }
}

/// Aggregated judge score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Aggregated {
    /// Average overall score.
    pub score: f32,
    /// Average correctness.
    pub correctness: f32,
    /// Average completeness.
    pub completeness: f32,
    /// Average fit.
    pub fit: f32,
    /// Average evidence.
    pub evidence: f32,
    /// Average clarity.
    pub clarity: f32,
    /// Number of judges.
    pub judges: usize,
    /// Adversary score delta (Phase D). 0.0 when the adversary did
    /// not fire or surfaced no adjustment. Range -2..=+2.
    #[serde(default)]
    pub adversary_delta: f32,
}

#[async_trait]
impl Phase for JudgePhase {
    fn name(&self) -> &'static str {
        "judge"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let proposals_dir = ctx.run_dir().proposals();
        let revisions_dir = ctx.run_dir().revisions();
        let evaluations_dir = ctx.run_dir().evaluations();
        let adversaries_dir = ctx.run_dir().adversaries();
        std::fs::create_dir_all(&evaluations_dir)?;
        std::fs::create_dir_all(&adversaries_dir)?;
        // Track E (E2): inject the six-axis rubric block before the
        // Judge prompt reaches the LLM. The substitution is a no-op
        // when the placeholder is absent (e.g. cached runs that
        // already saw the injected text).
        let system = inject_rubric(system_prompt(Role::Judge));

        // First pass: collect every (proposal_id, subject_json) pair.
        let mut subjects: Vec<(String, serde_json::Value)> = Vec::new();
        for entry in std::fs::read_dir(&proposals_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
                continue;
            }
            let proposal_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("p_unknown")
                .to_owned();
            let revision_path: PathBuf = revisions_dir.join(format!("{proposal_id}_rev_0.json"));
            let subject: serde_json::Value = if revision_path.exists() {
                serde_json::from_slice(&std::fs::read(&revision_path)?)?
            } else {
                serde_json::from_slice(&std::fs::read(&path)?)?
            };
            subjects.push((proposal_id, subject));
        }

        let judges = self.judges as usize;
        let total = subjects.len() * judges;
        let system_arc = Arc::new(system);
        let evaluations_dir_arc = Arc::new(evaluations_dir.clone());

        let futures = subjects.iter().flat_map(|(proposal_id, subject)| {
            let user_base = serde_json::to_string(subject).unwrap_or_default();
            let prop_id = proposal_id.clone();
            let system_arc = Arc::clone(&system_arc);
            let evaluations_dir_arc = Arc::clone(&evaluations_dir_arc);
            (0..judges).map(move |j| {
                let ctx = ctx.clone();
                let system_arc = Arc::clone(&system_arc);
                // Differentiate each judge's prompt so the cross-run
                // cache treats them as distinct calls (otherwise the
                // second judge on a given proposal would always
                // return the first judge's cached response).
                let user = format!("[judge_index={j}]\n{user_base}");
                let proposal_id = prop_id.clone();
                let evaluations_dir = Arc::clone(&evaluations_dir_arc);
                async move {
                    let _permit = ctx.parallelism.acquire().await?;
                    tracing::debug!(
                        proposal_id = %proposal_id,
                        judge_index = j,
                        stage = "judge.future.started",
                        "Judge stage"
                    );
                    let score: JudgeScore = ctx
                        .call_with_retry_parse(
                            Role::Judge,
                            system_arc.as_str().to_owned(),
                            user,
                            "JudgeScore: {score, criteria{correctness,completeness,fit,evidence,clarity}, comments}",
                            5,
                        )
                        .await?;
                    tracing::debug!(
                        proposal_id = %proposal_id,
                        judge_index = j,
                        stage = "judge.future.completed",
                        "Judge stage"
                    );
                    Ok::<(String, JudgeScore, Arc<PathBuf>), crate::error::Error>(
                        (proposal_id, score, evaluations_dir),
                    )
                }
            })
        });

        tracing::debug!(total, stage = "judge.join.started", "Judge stage");
        let results = join_all(futures).await;
        tracing::debug!(total, stage = "judge.join.completed", "Judge stage");

        // Aggregate per proposal.
        use std::collections::BTreeMap;
        let mut by_proposal: BTreeMap<String, Vec<JudgeScore>> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut first_dir: Option<Arc<PathBuf>> = None;
        for r in results {
            let (proposal_id, score, dir) = r?;
            if first_dir.is_none() {
                first_dir = Some(dir);
            }
            if !by_proposal.contains_key(&proposal_id) {
                order.push(proposal_id.clone());
            }
            by_proposal.entry(proposal_id).or_default().push(score);
        }
        let evaluations_dir = first_dir
            .map(|a| (*a).clone())
            .unwrap_or_else(|| ctx.run_dir().evaluations());

        // Phase D: optional adversary pass. Runs after the panel
        // because the disagreement score depends on the aggregated
        // scores. Skipped entirely in `fast` mode (`enable_adversary
        // = false`) and when the threshold is non-positive.
        let mut adversary_paths: Vec<PathBuf> = Vec::new();
        if self.enable_adversary && self.disagreement_threshold > 0.0 {
            let adv_system = system_prompt(Role::Adversary).to_owned();
            let adv_system_arc = Arc::new(adv_system);
            let adversaries_dir_arc = Arc::new(adversaries_dir.clone());
            let adv_futures = order.iter().filter_map(|proposal_id| {
                let scores = by_proposal.get(proposal_id)?.clone();
                let disagreement = Self::disagreement_score(&scores)?;
                if disagreement < self.disagreement_threshold {
                    return None;
                }
                let proposal_id = proposal_id.clone();
                let adv_system_arc = Arc::clone(&adv_system_arc);
                let adversaries_dir_arc = Arc::clone(&adversaries_dir_arc);
                Some(async move {
                    let ctx_for_adv = ctx.clone();
                    let user = serde_json::to_string(&serde_json::json!({
                        "proposal_id": proposal_id,
                        "aggregated": aggregate_no_delta(&scores),
                        "scores": scores,
                        "disagreement_score": disagreement,
                    }))
                    .unwrap_or_default();
                    let _permit = ctx_for_adv.parallelism.acquire().await.ok()?;
                    let report: AdversaryReport = ctx_for_adv
                        .call_with_retry_parse(
                            Role::Adversary,
                            adv_system_arc.as_str().to_owned(),
                            user,
                            "AdversaryReport: {proposal_id, consensus_check, disagreement_score, weaknesses[], unverified_claims[], score_delta, rationale}",
                            2,
                        )
                        .await
                        .ok()?;
                    let path = adversaries_dir_arc.join(format!("{proposal_id}.json"));
                    write_json(&path, &report).ok()?;
                    Some((proposal_id, disagreement, report.score_delta, path))
                })
            });
            let adv_results = join_all(adv_futures).await;
            // Aggregate the adversary deltas into the per-proposal map
            // so we can apply them in the next loop. We use a parallel
            // BTreeMap keyed by proposal_id.
            let mut deltas: BTreeMap<String, f32> = BTreeMap::new();
            for (proposal_id, disagreement, delta, path) in adv_results.into_iter().flatten() {
                tracing::debug!(
                    proposal_id = %proposal_id,
                    disagreement,
                    score_delta = delta,
                    stage = "judge.adversary.fired",
                    "Judge stage"
                );
                deltas.insert(proposal_id, delta);
                adversary_paths.push(path);
            }

            let mut paths = Vec::with_capacity(order.len());
            for proposal_id in order {
                let scores = by_proposal.remove(&proposal_id).unwrap_or_default();
                let mut agg = aggregate(&scores);
                if let Some(delta) = deltas.remove(&proposal_id) {
                    // Clamp the final score so adversary fires can't
                    // push an evaluation below 0 or above 10.
                    let combined = (agg.score + delta).clamp(0.0, 10.0);
                    agg.adversary_delta = combined - agg.score;
                    agg.score = combined;
                }
                let out_path: PathBuf =
                    evaluations_dir.join(format!("{proposal_id}.json"));
                write_json(&out_path, &agg)?;
                paths.push(out_path);
            }
            Ok(PhaseOutput::Evaluations(paths))
        } else {
            let mut paths = Vec::with_capacity(order.len());
            for proposal_id in order {
                let scores = by_proposal.remove(&proposal_id).unwrap_or_default();
                let agg = aggregate(&scores);
                let out_path: PathBuf =
                    evaluations_dir.join(format!("{proposal_id}.json"));
                write_json(&out_path, &agg)?;
                paths.push(out_path);
            }
            Ok(PhaseOutput::Evaluations(paths))
        }
        .map(|out| match out {
            PhaseOutput::Evaluations(paths) => {
                if adversary_paths.is_empty() {
                    PhaseOutput::Evaluations(paths)
                } else {
                    // Surface adversary paths in a parallel field by
                    // logging + returning the Evaluations. We do NOT
                    // add a new PhaseOutput variant per call to avoid
                    // touching the `pipe::run` match; the adversaries
                    // are still written to disk and indexed.
                    tracing::info!(
                        adversary_paths = adversary_paths.len(),
                        stage = "judge.adversary.summary",
                        "Judge stage"
                    );
                    PhaseOutput::Evaluations(paths)
                }
            }
            other => other,
        })
    }
}

fn aggregate(scores: &[JudgeScore]) -> Aggregated {
    let n = scores.len() as f32;
    let avg = |f: &dyn Fn(&JudgeScore) -> f32| scores.iter().map(f).sum::<f32>() / n;
    Aggregated {
        score: avg(&|s| s.score),
        correctness: avg(&|s| s.criteria.correctness),
        completeness: avg(&|s| s.criteria.completeness),
        fit: avg(&|s| s.criteria.fit),
        evidence: avg(&|s| s.criteria.evidence),
        clarity: avg(&|s| s.criteria.clarity),
        judges: scores.len(),
        adversary_delta: 0.0,
    }
}

fn aggregate_no_delta(scores: &[JudgeScore]) -> Aggregated {
    let mut a = aggregate(scores);
    a.adversary_delta = 0.0;
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{JudgeCriteria, JudgeScore};

    fn js(score: f32) -> JudgeScore {
        JudgeScore {
            score,
            criteria: JudgeCriteria::default(),
            comments: String::new(),
        }
    }

    #[test]
    fn disagreement_score_unanimous_is_zero() {
        let s = vec![js(7.0), js(7.0), js(7.0)];
        assert_eq!(JudgePhase::disagreement_score(&s), Some(0.0));
    }

    #[test]
    fn disagreement_score_diverges() {
        let s = vec![js(5.0), js(9.0)];
        let d = JudgePhase::disagreement_score(&s).unwrap();
        // stddev of [5, 9] on a 0..=10 scale is 2.0.
        assert!((d - 2.0).abs() < 1e-3, "got {d}");
    }

    #[test]
    fn disagreement_score_single_sample_is_none() {
        let s = vec![js(7.0)];
        assert!(JudgePhase::disagreement_score(&s).is_none());
    }

    #[test]
    fn aggregate_includes_adversary_delta_zero() {
        let s = vec![js(6.0), js(8.0)];
        let agg = aggregate(&s);
        assert_eq!(agg.judges, 2);
        assert!((agg.score - 7.0).abs() < 1e-6);
        assert_eq!(agg.adversary_delta, 0.0);
    }

    #[test]
    fn default_judge_phase_has_adversary_enabled() {
        let phase = JudgePhase::default();
        assert!(phase.enable_adversary);
        assert!((phase.disagreement_threshold - DEFAULT_DISAGREEMENT_THRESHOLD).abs() < 1e-6);
    }
}
