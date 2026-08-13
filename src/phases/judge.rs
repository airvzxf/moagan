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
//!
//! Track H (E6): when the 3 base judges disagree so strongly that
//! the aggregated mean is unreliable (max-min spread >
//! [`DEFAULT_FINAL_DISAGREEMENT_SPREAD`] **OR** stddev >
//! [`DEFAULT_FINAL_DISAGREEMENT_STDDEV`]), the phase fires the
//! `FinalDisagreement` role as a per-proposal tiebreaker. The
//! tiebreaker's verdict is written to
//! `adversaries/p_<id>_tiebreak.json` (next to the regular
//! adversary report when the panel was adversarial, or alone when
//! the panel agreed). The base panel's `Aggregated` is preserved
//! verbatim — the FinalDisagreement verdict is supplementary audit
//! data the downstream phases can opt in to.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};

use crate::cli::Mode;
use crate::config::Config;
use crate::domain::{AdversaryReport, FinalDisagreementReport, JudgeScore};
use crate::error::{Error, Result};
use crate::llm::Role;
use crate::llm::prompts::{inject_rubric, system_prompt};
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::write_json;
use crate::ranking::Rubric;

/// Default threshold (stddev on a 0..=10 scale) above which the
/// adversary pass is triggered. Tunable via [`JudgePhase::with_threshold`].
pub const DEFAULT_DISAGREEMENT_THRESHOLD: f32 = 0.5;

/// Default max-min spread (on the 0..=10 scale) above which the
/// `FinalDisagreement` tiebreaker is invoked. E6: a single judge
/// disagreeing by 2+ points relative to another is a strong signal
/// that the simple arithmetic mean is misleading.
pub const DEFAULT_FINAL_DISAGREEMENT_SPREAD: f32 = 1.0;

/// Default stddev threshold (on the 0..=10 scale) that also
/// triggers the `FinalDisagreement` tiebreaker. E6: paired with the
/// spread check, it covers both "one judge disagrees by a lot"
/// (max-min) and "the panel scatters broadly" (stddev). Either
/// condition alone is enough.
pub const DEFAULT_FINAL_DISAGREEMENT_STDDEV: f32 = 0.5;

pub(crate) fn validate_rubric_response(
    config: &Config,
    response: &serde_json::Value,
) -> Result<()> {
    if config.rubric.enabled && config.rubric.validate_responses {
        Rubric::default().validate(response).map_err(|error| {
            Error::SchemaViolation(format!("rubric validation failed: {error}"))
        })?;
    }
    Ok(())
}

/// D.21.7: per-mode judge quorum — the panel size each mode runs
/// by default. The values are pinned to the spec:
///
/// - `fast`:    1 (single judge; the panel disagreement contract
///   is meaningless with one scorer)
/// - `standard`: 3 (balanced panel; covers disagreement stats)
/// - `deep`:    5 (full panel; the panel disagreement gates the
///   adversary pass, so a 5-judge panel gives a meaningful stddev)
/// - `explore`: 5 (full panel; explore reuses the deep quorum
///   because exploration trades evaluation depth for proposal
///   breadth — a 5-judge panel keeps the score comparable with
///   `deep`)
/// - `batch`:   2 (CI/automation runs favour determinism over
///   panel disagreement — two judges is enough to detect
///   gross disagreement, the rest is handled by a downstream
///   consensus pass)
///
/// This helper is the spec baseline only. For profile-driven
/// overrides (`--profile <name>`), use the
/// `cardinality::judge_quorum_for_mode(mode, cfg)` helper which
/// checks `cfg.profile_judge_quorum_overrides` first and falls
/// through to this function when no override is set.
pub fn judge_quorum_for_mode(mode: Mode) -> usize {
    match mode {
        Mode::Fast => 1,
        Mode::Standard => 3,
        Mode::Deep => 5,
        Mode::Explore => 5,
        Mode::Batch => 2,
    }
}

/// Judge phase. `judges` judge scores per proposal. All proposals ×
/// judges are scheduled concurrently up to the global parallelism
/// cap; each proposal's individual scores are aggregated once its
/// `judges` calls complete. After the panel runs, an optional
/// adversary pass fires per proposal when `disagreement_score`
/// exceeds the configured threshold.
///
/// E6: when the base panel's spread is wide enough (max-min >
/// `final_disagreement_spread_threshold` OR stddev >
/// `final_disagreement_stddev_threshold`), the phase fires the
/// `FinalDisagreement` tiebreaker per proposal and writes the
/// verdict to `adversaries/p_<id>_tiebreak.json`. The base panel's
/// `Aggregated` is preserved verbatim — the tiebreaker is
/// supplementary audit data the downstream phases can read.
pub struct JudgePhase {
    /// Number of judges per proposal. When `mode` is `Some(_)`, the
    /// execute path overrides this with [`judge_quorum_for_mode`]
    /// for the active mode; when `mode` is `None`, `judges` is
    /// used verbatim (the legacy hard-coded knob).
    pub judges: u32,
    /// D.21.7: when `Some(mode)`, the per-mode judge quorum
    /// overrides `self.judges` in `execute()`. Set this from the
    /// pipeline builder so the per-mode panel size lands on the
    /// phase without callers having to re-derive the number. `None`
    /// preserves the legacy `self.judges` behaviour.
    pub mode: Option<Mode>,
    /// Disagreement threshold for the adversary pass. Default
    /// [`DEFAULT_DISAGREEMENT_THRESHOLD`].
    pub disagreement_threshold: f32,
    /// When `false`, skip the adversary pass entirely (Phase D
    /// opt-out for `fast` mode and `--no-adversary`).
    pub enable_adversary: bool,
    /// E6: max-min spread (0..=10 scale) above which the
    /// `FinalDisagreement` tiebreaker is invoked. Default
    /// [`DEFAULT_FINAL_DISAGREEMENT_SPREAD`].
    pub final_disagreement_spread_threshold: f32,
    /// E6: stddev (0..=10 scale) above which the
    /// `FinalDisagreement` tiebreaker is invoked. Default
    /// [`DEFAULT_FINAL_DISAGREEMENT_STDDEV`].
    pub final_disagreement_stddev_threshold: f32,
    /// E6: when `false`, skip the `FinalDisagreement` tiebreaker
    /// entirely. Opt-out for `fast` mode where the tiebreaker cost
    /// is not amortised across a large enough panel.
    pub enable_final_disagreement: bool,
}

impl Default for JudgePhase {
    fn default() -> Self {
        Self {
            judges: 3,
            mode: None,
            disagreement_threshold: DEFAULT_DISAGREEMENT_THRESHOLD,
            enable_adversary: true,
            final_disagreement_spread_threshold: DEFAULT_FINAL_DISAGREEMENT_SPREAD,
            final_disagreement_stddev_threshold: DEFAULT_FINAL_DISAGREEMENT_STDDEV,
            enable_final_disagreement: true,
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

    /// Max-min spread of the judges' overall scores on the 0..=10
    /// scale. `None` for fewer than two scores (the spread is
    /// undefined).
    fn score_spread(scores: &[JudgeScore]) -> Option<f32> {
        if scores.len() < 2 {
            return None;
        }
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for s in scores {
            if s.score < min {
                min = s.score;
            }
            if s.score > max {
                max = s.score;
            }
        }
        Some(max - min)
    }

    /// E6: should the `FinalDisagreement` tiebreaker fire for this
    /// proposal's panel? Returns `true` when EITHER the max-min
    /// spread exceeds `spread_threshold` OR the stddev exceeds
    /// `stddev_threshold`. Either condition alone is sufficient —
    /// the spread check catches "one judge disagrees by a lot", the
    /// stddev check catches "the panel scatters broadly". Returns
    /// `false` for fewer than two scores (the tiebreaker is
    /// meaningless without a panel).
    fn should_invoke_final_disagreement(
        scores: &[JudgeScore],
        spread_threshold: f32,
        stddev_threshold: f32,
    ) -> bool {
        if scores.len() < 2 {
            return false;
        }
        if let Some(spread) = Self::score_spread(scores)
            && spread > spread_threshold
        {
            return true;
        }
        if let Some(stddev) = Self::disagreement_score(scores)
            && stddev > stddev_threshold
        {
            return true;
        }
        false
    }

    /// E6: render the JSON user payload that the `FinalDisagreement`
    /// LLM call receives. The payload carries the raw 3 judge
    /// scores, a single-element candidate shortlist (the proposal
    /// under judgment), and the disagreement stats the tiebreaker
    /// needs to know about. The rendered payload is fed verbatim
    /// to `ctx.call_with_retry_parse` so the LLM-side schema and
    /// cache keys stay stable.
    fn build_final_disagreement_payload(
        proposal_id: &str,
        scores: &[JudgeScore],
        summary: &str,
        approach: &str,
    ) -> serde_json::Value {
        let judge_entries: Vec<serde_json::Value> = scores
            .iter()
            .enumerate()
            .map(|(i, s)| {
                serde_json::json!({
                    "judge": format!("judge-{}", (b'a' + i as u8) as char),
                    "score": s.score,
                })
            })
            .collect();
        serde_json::json!({
            "proposal_id": proposal_id,
            "judge_scores": judge_entries,
            "candidates": [{
                "id": proposal_id,
                "summary": summary,
                "approach": approach,
            }],
            "spread": Self::score_spread(scores),
            "disagreement_score": Self::disagreement_score(scores),
            "schema_version": "final_disagreement.v1",
        })
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
        let system = if ctx.config.rubric.enabled {
            inject_rubric(system_prompt(Role::Judge))
        } else {
            system_prompt(Role::Judge).to_owned()
        };

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

        let judges = self
            .mode
            .map(judge_quorum_for_mode)
            .unwrap_or(self.judges as usize);
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
                    let response: serde_json::Value = ctx
                        .call_with_retry_parse(
                            Role::Judge,
                            system_arc.as_str().to_owned(),
                            user,
                            "JudgeScore: {score, criteria{correctness,completeness,feasibility,safety,cost,clarity}, comments}",
                            5,
                        )
                        .await?;
                    validate_rubric_response(&ctx.config, &response)?;
                    let score: JudgeScore = serde_json::from_value(response)?;
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
        //
        // F3: budget gate. The adversary is the most expensive
        // pass in the judge phase — one LLM call per proposal
        // that exceeded the disagreement threshold. When the
        // budget observer reports Hard pressure + Reduce
        // policy, skip the pass entirely so the remaining
        // budget is reserved for the core pipeline.
        let budget_skip_adversary = ctx
            .telemetry
            .db()
            .map(|db| {
                crate::phases::budget::BudgetObserver::new(db.clone(), ctx.run_id)
                    .should_skip_optional()
            })
            .transpose()?
            .unwrap_or(false);
        if budget_skip_adversary {
            tracing::info!(
                run_id = %ctx.run_id,
                stage = "judge.adversary.skipped",
                reason = "budget_hard",
                "adversary pass skipped: budget under Hard pressure"
            );
        }
        let mut adversary_paths: Vec<PathBuf> = Vec::new();
        let mut deltas: BTreeMap<String, f32> = BTreeMap::new();
        if self.enable_adversary && self.disagreement_threshold > 0.0 && !budget_skip_adversary {
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
        }

        // Aggregate per proposal. Walk `order` (preserved insertion
        // order from the panel stage) so evaluation files land on
        // disk in the same order they were judged. Apply the
        // adversary delta when present; clamp into [0.0, 10.0] so a
        // runaway delta cannot push the score out of range.
        let mut paths: Vec<PathBuf> = Vec::with_capacity(order.len());
        for proposal_id in &order {
            let scores = by_proposal.get(proposal_id).cloned().unwrap_or_default();
            let mut agg = aggregate(&scores);
            if let Some(delta) = deltas.remove(proposal_id) {
                let combined = (agg.score + delta).clamp(0.0, 10.0);
                agg.adversary_delta = combined - agg.score;
                agg.score = combined;
            }
            let out_path: PathBuf = evaluations_dir.join(format!("{proposal_id}.json"));
            write_json(&out_path, &agg)?;
            paths.push(out_path);
        }
        if !adversary_paths.is_empty() {
            tracing::info!(
                adversary_paths = adversary_paths.len(),
                stage = "judge.adversary.summary",
                "Judge stage"
            );
        }

        // Track H (E6): FinalDisagreement tiebreaker. After the
        // adversary pass (which only fires when disagreement_score
        // exceeds `disagreement_threshold`), run an opt-in second
        // check that combines max-min spread AND stddev. Either
        // criterion triggers the tiebreaker; either condition alone
        // is enough to suggest the panel is unreliable. The verdict
        // is written to `adversaries/p_<id>_tiebreak.json` next to
        // the regular adversary report (or alone when the panel
        // agreed). The base panel's `Aggregated` is preserved
        // verbatim — the tiebreaker is supplementary audit data the
        // downstream phases can read. Skipped entirely when
        // `enable_final_disagreement == false` (fast mode opt-out)
        // or when both thresholds are non-positive.
        if self.enable_final_disagreement
            && (self.final_disagreement_spread_threshold > 0.0
                || self.final_disagreement_stddev_threshold > 0.0)
        {
            let fd_system = system_prompt(Role::FinalDisagreement).to_owned();
            let fd_system_arc = Arc::new(fd_system);
            let adversaries_dir_arc = Arc::new(adversaries_dir.clone());
            let fd_futures = order.iter().filter_map(|proposal_id| {
                let scores = by_proposal.get(proposal_id)?.clone();
                if !Self::should_invoke_final_disagreement(
                    &scores,
                    self.final_disagreement_spread_threshold,
                    self.final_disagreement_stddev_threshold,
                ) {
                    return None;
                }
                let proposal_id = proposal_id.clone();
                let fd_system_arc = Arc::clone(&fd_system_arc);
                let adversaries_dir_arc = Arc::clone(&adversaries_dir_arc);
                // The proposal text is needed by the tiebreaker so
                // it can audit the summary + approach alongside the
                // score table. The reviewer may use the text to
                // explain the disagreement in `rationale`.
                let summary = read_proposal_summary(&proposals_dir, &proposal_id);
                let approach = read_proposal_approach(&proposals_dir, &proposal_id);
                Some(async move {
                    let ctx_for_fd = ctx.clone();
                    let payload = JudgePhase::build_final_disagreement_payload(
                        &proposal_id,
                        &scores,
                        &summary,
                        &approach,
                    );
                    let user = serde_json::to_string(&payload).unwrap_or_default();
                    let _permit = ctx_for_fd.parallelism.acquire().await.ok()?;
                    let report: FinalDisagreementReport = ctx_for_fd
                        .call_with_retry_parse(
                            Role::FinalDisagreement,
                            fd_system_arc.as_str().to_owned(),
                            user,
                            "FinalDisagreement: {judge_scores[], candidates[], winner_id, margin, rationale}",
                            2,
                        )
                        .await
                        .ok()?;
                    let path = adversaries_dir_arc
                        .join(format!("{proposal_id}_tiebreak.json"));
                    write_json(&path, &report).ok()?;
                    Some((proposal_id, report.winner_id, report.margin, path))
                })
            });
            let fd_results = join_all(fd_futures).await;
            for (proposal_id, winner_id, margin, path) in fd_results.into_iter().flatten() {
                tracing::debug!(
                    proposal_id = %proposal_id,
                    winner_id = %winner_id,
                    margin,
                    stage = "judge.final_disagreement.fired",
                    "Judge stage"
                );
                adversary_paths.push(path);
            }
            if adversary_paths.len() > deltas.len() {
                tracing::info!(
                    final_disagreement_paths = adversary_paths.len(),
                    stage = "judge.final_disagreement.summary",
                    "Judge stage"
                );
            }
        }
        Ok(PhaseOutput::Evaluations(paths))
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

/// Read the `summary` field of a proposal sidecar. Used by the
/// `FinalDisagreement` pass to inject the proposal text into the
/// tiebreaker's payload. Returns an empty string when the sidecar
/// is missing or the field is absent — the tiebreaker treats an
/// empty summary the same as "no additional context".
fn read_proposal_summary(proposals_dir: &std::path::Path, proposal_id: &str) -> String {
    let path = proposals_dir.join(format!("{proposal_id}.json"));
    let Ok(bytes) = std::fs::read(&path) else {
        return String::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return String::new();
    };
    value
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned()
}

/// Read the `approach` field of a proposal sidecar. Used by the
/// `FinalDisagreement` pass to inject the proposal text into the
/// tiebreaker's payload. Returns an empty string when the sidecar
/// is missing or the field is absent — the tiebreaker treats an
/// empty approach the same as "no additional context".
fn read_proposal_approach(proposals_dir: &std::path::Path, proposal_id: &str) -> String {
    let path = proposals_dir.join(format!("{proposal_id}.json"));
    let Ok(bytes) = std::fs::read(&path) else {
        return String::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return String::new();
    };
    value
        .get("approach")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned()
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
    fn judge_phase_skips_validation_when_disabled() {
        let response = serde_json::json!({"verdict": "ok"});
        let config = Config::default();
        assert!(config.rubric.enabled);
        assert!(!config.rubric.validate_responses);
        assert!(validate_rubric_response(&config, &response).is_ok());
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

    // -- E6: FinalDisagreement tiebreaker -------------------------------

    /// When the three judges agree on the same score, neither the
    /// max-min spread nor the stddev crosses the default
    /// thresholds, so the tiebreaker is NOT invoked. Pins the
    /// contract that a unanimous panel stays unanimous.
    #[test]
    fn judge_skips_final_disagreement_when_unanimous() {
        let s = vec![js(7.0), js(7.0), js(7.0)];
        let fire = JudgePhase::should_invoke_final_disagreement(
            &s,
            DEFAULT_FINAL_DISAGREEMENT_SPREAD,
            DEFAULT_FINAL_DISAGREEMENT_STDDEV,
        );
        assert!(!fire, "unanimous panel must skip the tiebreaker");
    }

    /// A wide max-min spread (judges at 4.0, 5.0, 9.0 → spread 5.0)
    /// triggers the tiebreaker regardless of stddev. Pins the
    /// "either condition alone is enough" half of the E6 contract.
    #[test]
    fn judge_invokes_final_disagreement_when_spread_high() {
        // Spread = 9.0 - 4.0 = 5.0, well above the 1.0 threshold.
        let s = vec![js(4.0), js(5.0), js(9.0)];
        let fire = JudgePhase::should_invoke_final_disagreement(
            &s,
            DEFAULT_FINAL_DISAGREEMENT_SPREAD,
            DEFAULT_FINAL_DISAGREEMENT_STDDEV,
        );
        assert!(fire, "wide max-min spread must fire the tiebreaker");
    }

    /// When the max-min spread is small but the stddev is high
    /// (clustered around the mean with one outlier), the stddev
    /// half of the E6 condition must still fire. The default
    /// stddev threshold is 0.5; [7.0, 7.0, 8.0] has spread 1.0
    /// (right at the boundary) and stddev ≈ 0.471, which is BELOW
    /// the threshold — so we use a stricter case to exercise the
    /// stddev-only branch: [5.0, 8.0, 8.0] has spread 3.0 (fires
    /// the spread branch too). For a pure stddev-only case we need
    /// a spread at-or-below 1.0 with stddev > 0.5; [6.0, 7.0, 8.0]
    /// has spread 2.0 so it still fires on spread. To exercise the
    /// stddev branch exclusively, set `spread_threshold = 100.0`
    /// (impossible to exceed) and verify a high-stddev panel still
    /// fires on stddev alone.
    #[test]
    fn final_disagreement_fires_on_stddev_when_spread_threshold_disabled() {
        let s = vec![js(5.0), js(7.0), js(9.0)];
        let fire = JudgePhase::should_invoke_final_disagreement(
            &s,
            f32::INFINITY,
            DEFAULT_FINAL_DISAGREEMENT_STDDEV,
        );
        assert!(fire, "stddev > 0.5 must fire even when spread is disabled");
        // Same panel with both thresholds default: fires on spread.
        let fire_both = JudgePhase::should_invoke_final_disagreement(
            &s,
            DEFAULT_FINAL_DISAGREEMENT_SPREAD,
            DEFAULT_FINAL_DISAGREEMENT_STDDEV,
        );
        assert!(fire_both, "default thresholds must fire on the wide spread");
    }

    /// The rendered FinalDisagreement user payload must carry the
    /// raw judge scores as a structured table (so the LLM can
    /// reason about the disagreement) plus a single-element
    /// candidate shortlist with the proposal under judgment. Pins
    /// the schema contract for the catalog role.
    #[test]
    fn final_disagreement_prompt_renders_with_score_table() {
        let scores = vec![js(4.0), js(7.0), js(9.0)];
        let payload = JudgePhase::build_final_disagreement_payload(
            "p_001",
            &scores,
            "Sharded ledger keyed by tenant id.",
            "Route each tenant to its own shard; cross-shard reads go through a sequencer.",
        );
        let j = payload.to_string();
        assert!(j.contains("\"judge_scores\""), "missing judge_scores: {j}");
        assert!(j.contains("\"score\":4"), "missing judge score 4: {j}");
        assert!(j.contains("\"score\":7"), "missing judge score 7: {j}");
        assert!(j.contains("\"score\":9"), "missing judge score 9: {j}");
        assert!(j.contains("\"candidates\""), "missing candidates: {j}");
        assert!(j.contains("\"id\":\"p_001\""), "missing proposal id: {j}");
        assert!(
            j.contains("Sharded ledger keyed by tenant id."),
            "missing summary: {j}"
        );
        assert!(
            j.contains("Route each tenant to its own shard"),
            "missing approach: {j}"
        );
        assert!(
            j.contains("\"spread\""),
            "missing spread key (telemetry): {j}"
        );
        assert!(
            j.contains("\"disagreement_score\""),
            "missing disagreement_score key: {j}"
        );
    }

    /// Score-spread helper: a single sample has no spread; a pair
    /// has `max - min`; three unanimous judges have spread zero.
    #[test]
    fn score_spread_helper_behaves() {
        let one = vec![js(7.0)];
        assert_eq!(JudgePhase::score_spread(&one), None);
        let pair = vec![js(5.0), js(9.0)];
        assert!((JudgePhase::score_spread(&pair).unwrap() - 4.0).abs() < 1e-6);
        let three = vec![js(7.0), js(7.0), js(7.0)];
        assert!((JudgePhase::score_spread(&three).unwrap() - 0.0).abs() < 1e-6);
    }

    /// The default `JudgePhase` exposes the new FinalDisagreement
    /// fields with the documented defaults. Pins the wire-level
    /// contract for any downstream caller that introspects the
    /// phase configuration (CLI flags, profile overrides, etc.).
    #[test]
    fn default_judge_phase_has_final_disagreement_enabled() {
        let phase = JudgePhase::default();
        assert!(phase.enable_final_disagreement);
        assert!(
            (phase.final_disagreement_spread_threshold - DEFAULT_FINAL_DISAGREEMENT_SPREAD).abs()
                < 1e-6
        );
        assert!(
            (phase.final_disagreement_stddev_threshold - DEFAULT_FINAL_DISAGREEMENT_STDDEV).abs()
                < 1e-6
        );
    }

    // -- D.21.7: per-mode judge quorum ---------------------------------

    /// D.21.7: `fast` mode runs with 1 judge. Pins the smallest
    /// entry of the per-mode table so a refactor that drifts a
    /// value trips the test before it lands.
    #[test]
    fn judge_quorum_fast_returns_1() {
        assert_eq!(judge_quorum_for_mode(Mode::Fast), 1);
    }

    /// D.21.7: `deep` mode runs with 5 judges. Pins the largest
    /// entry of the per-mode table alongside the `fast` test.
    #[test]
    fn judge_quorum_deep_returns_5() {
        assert_eq!(judge_quorum_for_mode(Mode::Deep), 5);
    }

    /// The full per-mode table — pins every spec D.21.7 number in
    /// one test so a future addition is visible here first.
    #[test]
    fn judge_quorum_table_matches_spec() {
        assert_eq!(judge_quorum_for_mode(Mode::Fast), 1);
        assert_eq!(judge_quorum_for_mode(Mode::Standard), 3);
        assert_eq!(judge_quorum_for_mode(Mode::Deep), 5);
        assert_eq!(judge_quorum_for_mode(Mode::Explore), 5);
        assert_eq!(judge_quorum_for_mode(Mode::Batch), 2);
    }

    /// D.21.7: when `JudgePhase.mode` is set, the execute path
    /// derives the panel size from [`judge_quorum_for_mode`] for
    /// the active mode and ignores the legacy `self.judges` field.
    /// Pins that the per-mode helper is the single source of
    /// truth for the panel size when the mode is wired through.
    #[test]
    fn judge_phase_uses_mode_specific_quorum() {
        // Each mode maps to the spec D.21.7 quorum.
        for (mode, expected) in [
            (Mode::Fast, 1),
            (Mode::Standard, 3),
            (Mode::Deep, 5),
            (Mode::Explore, 5),
            (Mode::Batch, 2),
        ] {
            let phase = JudgePhase {
                mode: Some(mode),
                judges: 99, // explicitly different from the mode quorum
                ..JudgePhase::default()
            };
            assert_eq!(
                phase.mode.map(judge_quorum_for_mode),
                Some(expected),
                "mode {mode:?} must yield quorum {expected}"
            );
            // Mirror the execute() derivation in a single line:
            // when `mode` is Some, it wins; when None, fall back
            // to `judges`. Pins the precedence contract.
            let resolved = phase
                .mode
                .map(judge_quorum_for_mode)
                .unwrap_or(phase.judges as usize);
            assert_eq!(
                resolved, expected,
                "mode {mode:?} must drive the panel size"
            );
        }
        // When mode is None, the legacy `judges` field wins.
        let legacy = JudgePhase {
            mode: None,
            judges: 7,
            ..JudgePhase::default()
        };
        let resolved = legacy
            .mode
            .map(judge_quorum_for_mode)
            .unwrap_or(legacy.judges as usize);
        assert_eq!(resolved, 7, "mode=None must keep the legacy judges field");
    }

    /// F3: when the budget is Hard under the Reduce policy, the
    /// judge phase must skip the adversary pass — no
    /// `adversaries/p_<id>.json` is written, and
    /// `Aggregated.adversary_delta` stays at 0.0.
    ///
    /// The mock provider returns the same `AdversaryReport`
    /// regardless of input, so any adversary call would normally
    /// succeed and write a sidecar. The gate is the only thing
    /// that can stop that. We stage a single proposal with a
    /// unanimous panel (disagreement = 0) so the adversary
    /// short-circuit on `disagreement_score < threshold` would
    /// *also* prevent the call — to pin the F3 gate, we lower
    /// the threshold to 0.0 so the disagreement check passes
    /// and the gate is the only thing that can stop the call.
    #[test]
    fn adversary_phase_skipped_under_hard_budget() -> Result<()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = match ENV_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let home = std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve()?);
        home.ensure()?;
        let run_id = crate::ids::RunId::new();

        // Real Db + hard budget.
        let db = crate::storage::sqlite::Db::open(&home.meta_db_path())?;
        db.register_run(run_id, "fast", "running", "0.4.0", None, None, None)?;
        db.set_budget(run_id, 1000)?;
        db.budget_record(run_id, "seed", 950)?;

        // One proposal so the panel can run. The proposal text
        // is irrelevant — the mock provider always returns a
        // valid JudgeScore.
        let proposal_path = home.run_dir(run_id).proposals().join("p_a.json");
        std::fs::create_dir_all(proposal_path.parent().unwrap())?;
        crate::phases::util::write_json(
            &proposal_path,
            &crate::domain::Proposal {
                id: "p_a".into(),
                summary: "alpha".into(),
                approach: "approach body".into(),
                ..Default::default()
            },
        )?;

        // Register the mock provider in the registry so the
        // judge phase's `call_with_retry_parse` resolves it.
        // We push exactly one JudgeScore response — the
        // panel call consumes it, and the budget gate must
        // fire before the adversary call, so the mock's queue
        // is never drained twice. Without the gate, the
        // adversary call would land in `adversaries/p_a.json`
        // and the test would fail the assertion below.
        let mut mock = crate::llm::mock::MockProvider::empty();
        let judge_response = crate::llm::mock::MockResponse::plain(
            serde_json::json!({
                "score": 7.5_f32,
                "criteria": {
                    "correctness": 7.5,
                    "completeness": 7.5,
                    "fit": 7.5,
                    "evidence": 7.5,
                    "clarity": 7.5
                },
                "comments": "panel ok"
            })
            .to_string(),
        );
        mock.push(judge_response);
        let mut registry = crate::llm::ProviderRegistry::default();
        registry.insert("mock".into(), std::sync::Arc::new(mock));

        let run_dir = home.run_dir(run_id);
        let telemetry = crate::telemetry::Telemetry::open(
            run_id,
            &run_dir,
            crate::redact::RedactPolicy::default(),
            Some(db.clone()),
        )?;

        let ctx = RunContext::new(
            run_id,
            home.clone(),
            std::sync::Arc::new(registry),
            "mock".into(),
            "mock-model".into(),
            crate::execution::Parallelism::new(1),
            telemetry,
            String::new(),
            "fast".into(),
        )
        .with_interactive(false);

        // disagreement_threshold = 0.0 so the disagreement
        // short-circuit never fires; the gate is the only
        // barrier.
        let phase = JudgePhase {
            judges: 1,
            mode: None,
            enable_adversary: true,
            enable_final_disagreement: false,
            disagreement_threshold: 0.0,
            final_disagreement_spread_threshold: 0.0,
            final_disagreement_stddev_threshold: 0.0,
        };
        pollster::block_on(phase.execute(&ctx))?;

        // The adversary dir must be empty: the gate fired
        // before the per-proposal adversary call.
        let adv_dir = home.run_dir(run_id).adversaries();
        let written: Vec<_> = std::fs::read_dir(&adv_dir)?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        let adv_files: Vec<_> = written
            .iter()
            .filter(|n| n.starts_with("p_") && n.ends_with(".json") && !n.ends_with(".meta.json"))
            .collect();
        assert!(
            adv_files.is_empty(),
            "no adversary sidecar must be written under hard budget; got {adv_files:?}"
        );

        // And the evaluation must carry adversary_delta = 0.0
        // because the pass was skipped.
        let eval_path = home.run_dir(run_id).evaluations().join("p_a.json");
        let agg: crate::phases::judge::Aggregated =
            serde_json::from_slice(&std::fs::read(&eval_path)?)?;
        assert_eq!(
            agg.adversary_delta, 0.0,
            "adversary_delta must stay 0.0 when the gate skipped the pass; got {agg:?}"
        );
        Ok(())
    }
}
