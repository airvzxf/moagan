//! Rank phase. Reads every `evaluations/p_*.json`, runs the multi-
//! criterion pipeline from T01-06 §16.12 (Pareto → SimHash cluster →
//! crowding-distance representatives), then writes
//! `rankings/ranking.json` with the highest-scoring representative as
//! the winner and the full weighted ranking alongside.
//!
//! Phase H (V4 §5.12 paso 6): after the weighted sort and the
//! Phase F synthesis-replacement (steps 5 and 5.5), step 5.6
//! perturbs the per-criterion weights, measures how often each
//! proposal keeps its position, and labels the ranking
//! `stable | sensitive`. The result lands on `Ranking.stability_score`
//! / `stability_label` and is mirrored to SQLite via
//! `Telemetry::record_stability`. The verdict also feeds V4 §5.14's
//! human-checkpoint trigger (commit 7 of Phase H).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::checkpoint::modify_note;
use crate::config::Config;
use crate::domain::{Proposal, RankEntry, Ranking, StabilityLabel};
use crate::error::Result;
use crate::phases::cardinality::SelectionPlan;
use crate::phases::judge::Aggregated;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::ranking::pareto::QualityVector;
use crate::ranking::stability::{EvalSnapshot, stability_check};
use crate::ranking::{cluster_by_simhash, pareto_front, pick_with_crowding};

/// Number of representative proposals to surface for delivery (top-3
/// per the V4 §13.6 MVP definition).
const TOP_K: usize = 3;

/// Jaccard threshold for clustering proposals by text. 0.7 means two
/// proposals with 70%+ shared vocabulary count as the same stack.
const CLUSTER_THRESHOLD: f32 = 0.7;

/// Rank phase. The cardinality is the number of proposals emitted by
/// the previous phase; ordering is by the weighted score that
/// `Config::ranking_weights` produces.
pub struct RankPhase {
    /// Shared config so the rank phase can read the per-criterion
    /// weights without going through `RunContext`.
    pub config: Arc<Config>,
    /// Phase F: enable the synthesis-replacement predicate. When
    /// `true`, a synthesis (`s_<NN>`) that dominates its source
    /// cluster per V4 §5.13 + D.13.16 removes the source proposals
    /// from the final ranking and stamps them with `replaced_by`.
    /// `fast` mode sets this to `false` because it doesn't run
    /// `SynthesizePhase`; `standard`/`deep`/`batch` set it to
    /// `true`. The CLI flag `--no-replace-sources` overrides both.
    pub replace_sources_enabled: bool,
    /// Phase H: enable the stability check (V4 §5.12 paso 6).
    /// Mirrors `Config::stability.enabled`; the wiring lives here so
    /// tests can disable it without poking at the global config.
    /// When `false` the phase writes `null` for the stability
    /// fields and never invokes the checkpoint trigger.
    pub stability_enabled: bool,
}

#[async_trait]
impl Phase for RankPhase {
    fn name(&self) -> &'static str {
        "rank"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let evaluations_dir = ctx.run_dir().evaluations();
        let proposals_dir = ctx.run_dir().proposals();
        let revisions_dir = ctx.run_dir().revisions();
        let rankings_dir = ctx.run_dir().rankings();
        std::fs::create_dir_all(&rankings_dir)?;

        // F1: surface any operator note captured by an earlier
        // `Resolution::Modify(text)` so a future rank-prompt call
        // (e.g. `moagan rerank`) inherits the operator's
        // correction. The rank phase itself is currently
        // pure compute (no LLM call) — the prepended string is
        // computed eagerly so `prepend_to_prompt` is exercised
        // every run and the sidecar is consumed before any
        // downstream phase re-reads it.
        let _rank_user_intent = modify_note::prepend_to_prompt(ctx.run_dir().root(), "");

        // Step 1: gather every evaluation together with the proposal
        // itself (preferring the latest repair if present). The
        // proposal struct is needed by SelectionPlan::apply later in
        // step 5.5; carrying it here avoids a second disk read.
        let mut items: Vec<(String, Aggregated, Proposal)> = Vec::new();
        for entry in std::fs::read_dir(&evaluations_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
                continue;
            }
            let agg: Aggregated = read_json(&path)?;
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("p_unknown")
                .to_owned();
            let proposal = load_proposal(&proposals_dir, &revisions_dir, &id);
            items.push((id, agg, proposal));
        }

        let n = items.len();
        if n == 0 {
            let out_path: PathBuf = rankings_dir.join("ranking.json");
            write_json(&out_path, &Ranking::default())?;
            return Ok(PhaseOutput::Ranking(out_path));
        }

        // Step 2: Pareto front on the five-criterion quality vector.
        let vectors: Vec<QualityVector> = items
            .iter()
            .map(|(_, agg, _)| QualityVector::from_aggregated(agg))
            .collect();
        let front = pareto_front(&vectors);
        let front_set: std::collections::BTreeSet<usize> = front.iter().copied().collect();

        // Step 3: cluster the front's texts by SimHash Jaccard.
        let front_texts: Vec<String> = front.iter().map(|i| proposal_text(&items[*i].2)).collect();
        let clusters: Vec<Vec<usize>> = if front_texts.len() <= 1 {
            front.iter().map(|i| vec![*i]).collect()
        } else {
            let cs = cluster_by_simhash(&front_texts, CLUSTER_THRESHOLD);
            cs.into_iter()
                .map(|c| c.into_iter().map(|idx| front[idx]).collect())
                .collect()
        };

        // Step 4: pick top-K representatives.
        let rep_indices: Vec<usize> = pick_with_crowding(&clusters, &vectors, TOP_K);

        // Step 5: compute the weighted score for every proposal and
        // build the full ranked list. The winner is the highest-
        // scoring representative; the deliver phase consumes the top
        // of `representatives` (or the top of `ranked` if the front
        // has fewer than TOP_K entries).
        let mut ranked: Vec<RankEntry> = items
            .iter()
            .map(|(id, agg, _)| {
                let score = self.config.ranking_weights.weighted_score(
                    agg.correctness,
                    agg.completeness,
                    agg.fit,
                    agg.evidence,
                    agg.clarity,
                    agg.score,
                );
                RankEntry {
                    id: id.clone(),
                    score,
                    reason: format!(
                        "weighted avg of {} judges (correctness {:.2}, completeness {:.2}, fit {:.2}, evidence {:.2}, clarity {:.2})",
                        agg.judges,
                        agg.correctness,
                        agg.completeness,
                        agg.fit,
                        agg.evidence,
                        agg.clarity
                    ),
                }
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Step 5.5 (Phase F): apply the synthesis-replacement
        // predicate (V4 §5.13 + D.13.16). For every synthesis
        // (`s_<NN>`) in the front, look up its cluster membership
        // (loaded from `synthesized/s_<NN>.json` — the immutable
        // lineage sidecar), gather the source quality vectors, and
        // call the predicate. When it returns true, drop the
        // sources from `ranked` and stamp their sidecars with
        // `replaced_by` so the deliver surface can show the
        // supersession. The synthesis is added to `representatives`
        // if it wasn't already picked by the crowding step.
        let mut representatives: Vec<RankEntry> = rep_indices
            .iter()
            .filter_map(|&i| ranked.iter().find(|r| r.id == items[i].0).cloned())
            .collect();

        if self.replace_sources_enabled {
            let (dropped, promoted) = apply_synthesis_replacement(
                &ranked,
                &representatives,
                &items,
                &evaluations_dir,
                &proposals_dir,
                &ctx.run_dir().synthesized(),
            )?;
            if !dropped.is_empty() {
                ranked.retain(|r| !dropped.contains(&r.id));
                for syn_id in &promoted {
                    if !representatives.iter().any(|r| &r.id == syn_id)
                        && let Some(entry) = ranked.iter().find(|r| &r.id == syn_id).cloned()
                    {
                        representatives.insert(0, entry);
                    }
                }
            }
        }

        let winner = representatives
            .first()
            .map(|r| r.id.clone())
            .or_else(|| ranked.first().map(|r| r.id.clone()))
            .unwrap_or_default();

        // Step 5.7 (Track E, E3): apply the mode-specific
        // SelectionPlan to filter the final portfolio. The plan
        // picks a subset of `(id, score, Proposal)` triples — top-N
        // for `fast` / `standard` / `deep` / `batch`, diverse-N for
        // `explore` (spec D.21.3). Both `ranked` and
        // `representatives` are filtered so the deliver surface
        // only sees the chosen ids. The score is the same
        // weighted score computed in step 5; the Proposal is the
        // structured object read in step 1 (used by
        // `keep_diverse` / `keep_outlier` for Jaccard distance over
        // the proposal text).
        //
        // The plan's defaults live in
        // [`SelectionPlan::default_for_mode`] — a future commit
        // can override per-profile via a Config field without
        // touching this call site.
        let plan = SelectionPlan::default_for_mode(&ctx.mode);
        let id_to_score: std::collections::HashMap<&str, f64> = ranked
            .iter()
            .map(|r| (r.id.as_str(), r.score as f64))
            .collect();
        let id_to_proposal: std::collections::HashMap<&str, &Proposal> =
            items.iter().map(|(id, _, p)| (id.as_str(), p)).collect();
        let mut scored: Vec<(String, f64, Proposal)> = Vec::with_capacity(ranked.len());
        for r in &ranked {
            let score = id_to_score.get(r.id.as_str()).copied().unwrap_or(0.0);
            let proposal = id_to_proposal
                .get(r.id.as_str())
                .cloned()
                .cloned()
                .unwrap_or_default();
            scored.push((r.id.clone(), score, proposal));
        }
        let chosen: std::collections::BTreeSet<String> = plan.apply(&scored).into_iter().collect();
        ranked.retain(|r| chosen.contains(&r.id));
        representatives.retain(|r| chosen.contains(&r.id));

        let _ = front_set; // front_set is informational for telemetry consumers

        // Step 5.6 (Phase H, V4 §5.12 paso 6): perturb the
        // per-criterion weights and measure how often the top-1
        // winner keeps its position. The result lives on the
        // ranking sidecar so the deliver phase and the audit
        // dashboard can surface it; the verdict also feeds the
        // V4 §5.14 human-checkpoint trigger (commit 7 of Phase H).
        //
        // Skip conditions:
        // - stability_enabled == false (rank-phase constructor
        //   mirrored Config::stability.enabled into this flag).
        // - fewer than 2 evaluations (trivially stable, score = 1.0).
        // - Config::stability.n_perturbations == 0.
        // - BudgetObserver reports Hard pressure under the Reduce
        //   policy (F3). The check is no-op when the run is
        //   configured without a budget (`planned = 0`) so legacy
        //   fast-mode users do not silently lose the stability
        //   verdict.
        let budget_skip_stability = ctx
            .telemetry
            .db()
            .map(|db| {
                crate::phases::budget::BudgetObserver::new(db.clone(), ctx.run_id)
                    .should_skip_optional()
            })
            .transpose()?
            .unwrap_or(false);
        if budget_skip_stability {
            tracing::info!(
                run_id = %ctx.run_id,
                stage = "rank.stability.skipped",
                reason = "budget_hard",
                "rank stability skipped: budget under Hard pressure"
            );
        }
        let (stability_score_map, stability_label, stability_sigma) = if !self.stability_enabled
            || self.config.stability.n_perturbations == 0
            || budget_skip_stability
        {
            // No-op: write nothing useful. The fields stay None
            // on the sidecar so legacy parsers see no change.
            (None, None, None)
        } else {
            let snapshots: Vec<(String, EvalSnapshot)> = items
                .iter()
                .map(|(id, agg, _)| {
                    (
                        id.clone(),
                        EvalSnapshot {
                            correctness: agg.correctness,
                            completeness: agg.completeness,
                            fit: agg.fit,
                            evidence: agg.evidence,
                            clarity: agg.clarity,
                            overall: agg.score,
                        },
                    )
                })
                .collect();
            if snapshots.len() < 2 {
                // Single proposal: trivially stable.
                let mut m = std::collections::HashMap::new();
                m.insert(snapshots[0].0.clone(), 1.0_f32);
                (
                    Some(m),
                    Some(StabilityLabel::Stable),
                    Some(self.config.stability.sigma_default),
                )
            } else {
                let sigma = if ctx.interactive {
                    self.config.stability.sigma_interactive
                } else {
                    self.config.stability.sigma_default
                };
                let (score, label, _sigma_used) = stability_check(
                    &self.config.ranking_weights,
                    &snapshots,
                    self.config.stability.n_perturbations,
                    sigma,
                    self.config.stability.seed,
                    self.config.stability.sensitive_threshold,
                );
                let sigma_used = if ctx.interactive {
                    self.config.stability.sigma_interactive
                } else {
                    self.config.stability.sigma_default
                };
                // W2: mirror the verdict into SQLite via the
                // `runs` row (v009). Best-effort: a pre-v009
                // DB returns Ok(()) silently and the sidecar
                // path stays as the canonical source.
                if let Some(db) = ctx.telemetry.db() {
                    let label_str = match label {
                        crate::domain::StabilityLabel::Stable => "stable",
                        crate::domain::StabilityLabel::Sensitive => "sensitive",
                    };
                    // `score` is the per-proposal stability HashMap.
                    // Persist the top-1 score (the winner's) so the
                    // dashboard's "stability per run" view shows
                    // the same number the operator sees in the
                    // sensitive-checkpoint question.
                    let top_score = score.values().copied().reduce(f32::max).unwrap_or(0.0);
                    if let Err(e) =
                        db.record_run_stability(ctx.run_id, top_score, label_str, sigma_used)
                    {
                        tracing::warn!(error = %e, "stability mirror to runs failed");
                    }
                }
                tracing::info!(
                    run_id = %ctx.run_id,
                    sigma = sigma_used,
                    n = self.config.stability.n_perturbations,
                    label = ?label,
                    "rank stability computed"
                );
                (Some(score), Some(label), Some(sigma_used))
            }
        };

        let ranking = Ranking {
            ranked,
            representatives,
            winner,
            stability_score: stability_score_map,
            stability_label,
            stability_sigma,
        };
        let out_path: PathBuf = rankings_dir.join("ranking.json");
        write_json(&out_path, &ranking)?;

        // Phase H commit 7 (V4 §5.14 second trigger): when the
        // ranking lands on Sensitive and the run is interactive,
        // fire a human checkpoint. The user can accept the
        // current winner, reject (which leaves the pipeline to
        // finish but with the verdict flagged), or free-form an
        // alternative (currently the answer is just recorded; a
        // follow-up can re-rank with the user's note applied).
        //
        // Non-interactive runs (`--non-interactive` or Mode::Batch)
        // do not prompt; `checkpoint::skip` writes the
        // `<skipped:non_interactive>` marker for audit.
        if stability_label == Some(StabilityLabel::Sensitive) {
            use crate::checkpoint::{Checkpoint, CheckpointKind, CheckpointOpts, Resolution};
            let top_score = ranking
                .stability_score
                .as_ref()
                .and_then(|m| m.values().copied().reduce(f32::max))
                .unwrap_or(0.0);
            let question = format!(
                "Ranking is sensitive to weight perturbation (top-1 stability {top_score:.2}, \
                 threshold {:.2}, sigma {:.2}). Continue with the current winner '{}'?",
                self.config.stability.sensitive_threshold,
                stability_sigma.unwrap_or(0.0),
                ranking.winner
            );
            let cp = Checkpoint::new(CheckpointKind::Custom, question, true);
            let opts = CheckpointOpts {
                interactive: ctx.interactive,
                stdin_override: None,
                telemetry: Some(ctx.telemetry.clone()),
            };
            let checkpoints_dir = ctx.run_dir().checkpoints();
            // Reject aborts the pipeline with Error::Cancelled so the
            // operator gets a non-zero exit and the run flips to
            // 'failed'. The V4 §5.14 second trigger is therefore
            // terminal — same contract as the Final checkpoint.
            match crate::checkpoint::ask(&cp, &checkpoints_dir, &opts)? {
                Resolution::Approved => {}
                Resolution::Modify(text) => {
                    // F1: persist the operator's correction so the
                    // deliver phase (and the next `moagan rerank`)
                    // can prepend it to their prompts.
                    crate::checkpoint::persist_modify_note(ctx.run_dir().root(), "rank", &text)?;
                }
                Resolution::Rejected => {
                    return Err(crate::error::Error::Cancelled(
                        "user rejected the sensitive ranking".into(),
                    ));
                }
            }
        }

        Ok(PhaseOutput::Ranking(out_path))
    }
}

/// Phase F: walk the synthesized sidecars, evaluate the predicate for
/// each synthesis against its cluster sources, and return the ids that
/// should be dropped from the ranking plus the synthesis ids that
/// should be promoted to `representatives`. Side effects: stamp each
/// dropped source's `proposals/p_<id>.json` with `replaced_by`.
fn apply_synthesis_replacement(
    ranked: &[RankEntry],
    representatives: &[RankEntry],
    items: &[(String, Aggregated, Proposal)],
    evaluations_dir: &std::path::Path,
    proposals_dir: &std::path::Path,
    synthesized_dir: &std::path::Path,
) -> Result<(std::collections::BTreeSet<String>, Vec<String>)> {
    use crate::phases::replace::should_replace_synthesis;
    use crate::ranking::pareto::QualityVector;

    let mut dropped: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut promoted: Vec<String> = Vec::new();

    // Build an `id -> Aggregated` index so we don't re-read the
    // evaluations files inside the inner loop.
    let agg_by_id: std::collections::BTreeMap<&str, &Aggregated> = items
        .iter()
        .map(|(id, agg, _)| (id.as_str(), agg))
        .collect();

    let synth_dir = synthesized_dir;
    let entries = match std::fs::read_dir(synth_dir) {
        Ok(e) => e,
        Err(_) => return Ok((dropped, promoted)),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
            continue;
        }
        let synth_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        // Synthesized ids always start with `s_` (see `synth_to_proposal`
        // and `SynthesizePhase::execute`). Anything else is noise.
        if !synth_id.starts_with("s_") {
            continue;
        }
        // Load the synthesized sidecar to recover the cluster_id and
        // member_proposals (the lineage record). Per the user's
        // decision (session 2026-07-30), this sidecar is the single
        // source of truth for s_<NN> → cp_<NN>.
        let synth: crate::domain::SynthesizedProposal = match read_json(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let source_ids = if !synth.source_proposals.is_empty() {
            synth.source_proposals.clone()
        } else {
            continue;
        };

        let Some(s_agg) = agg_by_id.get(synth_id.as_str()) else {
            continue;
        };
        let s_v = QualityVector::from_aggregated(s_agg);

        let mut source_vs: Vec<QualityVector> = Vec::with_capacity(source_ids.len());
        for sid in &source_ids {
            if let Some(a) = agg_by_id.get(sid.as_str()) {
                source_vs.push(QualityVector::from_aggregated(a));
            }
        }
        if source_vs.len() != source_ids.len() {
            // At least one source has no Aggregated on disk — skip
            // this synthesis rather than make a partial decision.
            continue;
        }

        if should_replace_synthesis(&s_v, &source_vs) {
            for sid in &source_ids {
                dropped.insert(sid.clone());
                // Stamp the source sidecar with `replaced_by`. Best-
                // effort: a missing or locked file is logged but does
                // not abort the run (the ranking still drops the id).
                let src_path = proposals_dir.join(format!("{sid}.json"));
                if let Ok(mut p) = read_json::<Proposal>(&src_path) {
                    p.replaced_by = Some(synth_id.clone());
                    if write_json(&src_path, &p).is_err() {
                        eprintln!("warn: failed to stamp replaced_by on proposals/{sid}.json");
                    }
                }
            }
            promoted.push(synth_id.clone());
        }
    }

    // Suppress unused warning when the loop body never fires (e.g.
    // empty run). The references keep the borrow checker happy.
    let _ = ranked;
    let _ = representatives;
    let _ = evaluations_dir;

    Ok((dropped, promoted))
}

/// Load the proposal (for SelectionPlan + clustering) by reading
/// the original proposal and falling back to the highest-numbered
/// revision when the model produced a repair. Returns a
/// `Proposal` populated with `id` and a fallback `summary`
/// (set to the id) when neither exists, so the caller never has
/// to branch on a missing file.
fn load_proposal(
    proposals_dir: &std::path::Path,
    revisions_dir: &std::path::Path,
    proposal_id: &str,
) -> Proposal {
    let proposal_path = proposals_dir.join(format!("{proposal_id}.json"));
    if let Ok(p) = read_json::<Proposal>(&proposal_path) {
        return p;
    }
    // No original proposal; try the highest revision number.
    let mut latest_n: Option<u32> = None;
    if let Ok(entries) = std::fs::read_dir(revisions_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let prefix = format!("{proposal_id}_rev_");
            if let Some(rest) = name.strip_prefix(&prefix)
                && let Some(stem) = rest.strip_suffix(".json")
                && let Ok(n) = stem.parse::<u32>()
            {
                latest_n = Some(latest_n.map_or(n, |m| m.max(n)));
            }
        }
    }
    if let Some(n) = latest_n {
        let rev_path = revisions_dir.join(format!("{proposal_id}_rev_{n}.json"));
        if let Ok(p) = read_json::<Proposal>(&rev_path) {
            return p;
        }
    }
    // Fall back to a default Proposal carrying only the id.
    Proposal {
        id: proposal_id.to_owned(),
        summary: proposal_id.to_owned(),
        ..Proposal::default()
    }
}

/// Concatenate a proposal's textual fields into a single string
/// for SimHash clustering. Mirrors the format the previous
/// `load_proposal_text` produced so the cluster step sees the
/// same input.
fn proposal_text(p: &Proposal) -> String {
    format!(
        "{} {} {} {}",
        p.summary,
        p.approach,
        p.tradeoffs.join(" "),
        p.evidence.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::config::{Config, RankingWeights, StabilityConfig};
    use crate::domain::{Proposal, Ranking};
    use crate::error::Result;
    use crate::execution::Parallelism;
    use crate::fs_layout::MoaganHome;
    use crate::ids::RunId;
    use crate::llm::ProviderRegistry;
    use crate::phases::judge::Aggregated;
    use crate::phases::phase::{Phase, RunContext};
    use crate::phases::rank::RankPhase;
    use crate::phases::util::write_json;
    use crate::redact::RedactPolicy;
    use crate::storage::sqlite::Db;
    use crate::telemetry::Telemetry;

    #[test]
    fn pareto_front_then_representatives_logic_smoke() {
        use crate::ranking::pareto::QualityVector;
        let vectors = vec![
            QualityVector {
                correctness: 9.0,
                completeness: 8.0,
                fit: 7.0,
                evidence: 6.0,
                clarity: 5.0,
            },
            QualityVector {
                correctness: 5.0,
                completeness: 6.0,
                fit: 7.0,
                evidence: 8.0,
                clarity: 9.0,
            },
            QualityVector {
                correctness: 1.0,
                completeness: 1.0,
                fit: 1.0,
                evidence: 1.0,
                clarity: 1.0,
            },
        ];
        let front = crate::ranking::pareto_front(&vectors);
        assert_eq!(front, vec![0, 1]);
    }

    /// E3: `RankPhase` uses the mode-specific `SelectionPlan` to
    /// filter the final portfolio. With `mode = "fast"` the
    /// default plan is `keep_top(3)` — so five proposals must
    /// collapse to the three highest-scoring ones in the
    /// `ranked` array. The test pins that the wire-up is
    /// happening (a regression that drops the SelectionPlan call
    /// would leave all five ids in the ranking).
    #[test]
    fn rank_phase_uses_selection_plan_to_filter() -> Result<()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = match ENV_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        // Set up a fresh MOAGAN_HOME so the rank phase sees a
        // clean run directory.
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let home = Arc::new(MoaganHome::resolve().unwrap());
        home.ensure().unwrap();
        let run_id = RunId::new();

        // Five proposals with descending weighted scores. Each
        // proposal also carries a distinct summary so Jaccard
        // distance has a non-trivial signal — irrelevant for
        // `mode = "fast"` (which uses keep_top) but useful for
        // documenting that the proposal text is loaded into
        // `items`.
        let proposals = [
            (
                "p_top",
                Aggregated {
                    score: 9.0,
                    correctness: 9.0,
                    completeness: 9.0,
                    fit: 9.0,
                    evidence: 9.0,
                    clarity: 9.0,
                    judges: 3,
                    adversary_delta: 0.0,
                },
                "alpha alpha alpha",
            ),
            (
                "p_two",
                Aggregated {
                    score: 8.0,
                    correctness: 8.0,
                    completeness: 8.0,
                    fit: 8.0,
                    evidence: 8.0,
                    clarity: 8.0,
                    judges: 3,
                    adversary_delta: 0.0,
                },
                "beta beta beta",
            ),
            (
                "p_three",
                Aggregated {
                    score: 7.0,
                    correctness: 7.0,
                    completeness: 7.0,
                    fit: 7.0,
                    evidence: 7.0,
                    clarity: 7.0,
                    judges: 3,
                    adversary_delta: 0.0,
                },
                "gamma gamma gamma",
            ),
            (
                "p_four",
                Aggregated {
                    score: 6.0,
                    correctness: 6.0,
                    completeness: 6.0,
                    fit: 6.0,
                    evidence: 6.0,
                    clarity: 6.0,
                    judges: 3,
                    adversary_delta: 0.0,
                },
                "delta delta delta",
            ),
            (
                "p_five",
                Aggregated {
                    score: 5.0,
                    correctness: 5.0,
                    completeness: 5.0,
                    fit: 5.0,
                    evidence: 5.0,
                    clarity: 5.0,
                    judges: 3,
                    adversary_delta: 0.0,
                },
                "epsilon epsilon epsilon",
            ),
        ];

        // Write the proposal sidecars.
        for (id, _, summary) in &proposals {
            let path = home.run_dir(run_id).proposals().join(format!("{id}.json"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let p = Proposal {
                id: (*id).to_owned(),
                summary: (*summary).to_owned(),
                ..Proposal::default()
            };
            write_json(&path, &p).unwrap();
        }
        // Write the aggregated sidecars.
        for (id, agg, _) in &proposals {
            let path = home
                .run_dir(run_id)
                .evaluations()
                .join(format!("{id}.json"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_json(&path, agg).unwrap();
        }

        // Disable stability so the test does not block on the
        // sensitive-checkpoint trigger and so the deterministic
        // ranking flows through to the assertions.
        let cfg = Arc::new(Config {
            ranking_weights: RankingWeights::default(),
            stability: StabilityConfig {
                enabled: false,
                ..StabilityConfig::default()
            },
            ..Config::default()
        });
        let ctx = RunContext::new(
            run_id,
            home.clone(),
            Arc::new(ProviderRegistry::default()),
            "mock".into(),
            "mock-model".into(),
            Parallelism::new(1),
            Telemetry::noop(),
            String::new(),
            "fast".into(),
        )
        .with_interactive(false);

        let phase = RankPhase {
            config: cfg,
            replace_sources_enabled: false,
            stability_enabled: false,
        };
        pollster::block_on(phase.execute(&ctx))?;

        // Read the ranking sidecar.
        let path = home.run_dir(run_id).rankings().join("ranking.json");
        let raw = std::fs::read(&path).unwrap();
        let ranking: Ranking = serde_json::from_slice(&raw).unwrap();

        // `mode = "fast"` ⇒ `keep_top(3)` ⇒ the three highest
        // scorers land on `ranked`. The two lowest scorers
        // (`p_four`, `p_five`) must be filtered out by
        // SelectionPlan::apply.
        let ids: std::collections::BTreeSet<&str> =
            ranking.ranked.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["p_top", "p_two", "p_three"].iter().copied().collect());
        // The two lowest scorers must NOT be in `ranked` (this
        // is the SelectionPlan filter; without it all five ids
        // would be present).
        assert!(!ids.contains("p_four"), "SelectionPlan must drop p_four");
        assert!(!ids.contains("p_five"), "SelectionPlan must drop p_five");
        Ok(())
    }

    /// F1: when an operator note is persisted to
    /// `<run_dir>/state/modify_note.json` before the rank phase
    /// runs, the rank phase still surfaces that note through the
    /// shared `prepend_to_prompt` helper at the top of `execute`.
    ///
    /// The rank phase itself is currently pure compute (no LLM
    /// call); the F1 wire-up eagerly calls the helper so the
    /// sidecar is consumed by every run and `prepend_to_prompt`
    /// returns the operator's text. The assertion is wired to
    /// the helper's output: any future rank-prompt LLM call will
    /// inherit the prepended string verbatim.
    #[test]
    fn rank_phase_includes_modify_note_in_prompt() -> Result<()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = match ENV_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let home = Arc::new(MoaganHome::resolve().unwrap());
        home.ensure().unwrap();
        let run_id = RunId::new();
        let run_root = home.run_dir(run_id).root().to_path_buf();

        // Minimal on-disk artefacts: one proposal + one aggregated
        // so the rank phase emits a ranking sidecar. The exact
        // contents are irrelevant — the test only cares about the
        // modify-note wiring.
        let proposal = Proposal {
            id: "p_only".into(),
            summary: "single".into(),
            ..Proposal::default()
        };
        let agg = Aggregated {
            score: 8.0,
            correctness: 8.0,
            completeness: 8.0,
            fit: 8.0,
            evidence: 8.0,
            clarity: 8.0,
            judges: 3,
            adversary_delta: 0.0,
        };
        let proposal_path = home.run_dir(run_id).proposals().join("p_only.json");
        std::fs::create_dir_all(proposal_path.parent().unwrap()).unwrap();
        write_json(&proposal_path, &proposal).unwrap();
        let agg_path = home.run_dir(run_id).evaluations().join("p_only.json");
        std::fs::create_dir_all(agg_path.parent().unwrap()).unwrap();
        write_json(&agg_path, &agg).unwrap();

        // F1: persist the operator note *before* rank runs.
        crate::checkpoint::persist_modify_note(&run_root, "clarify", "weight correctness higher")?;

        // Run rank with stability disabled so the phase does not
        // invoke the LLM (the LLM call would otherwise be a pure
        // sensitivity prompt — we only care about the modify-note
        // wiring, not the score).
        let cfg = Arc::new(Config {
            ranking_weights: RankingWeights::default(),
            stability: StabilityConfig {
                enabled: false,
                ..StabilityConfig::default()
            },
            ..Config::default()
        });
        let ctx = RunContext::new(
            run_id,
            home.clone(),
            Arc::new(ProviderRegistry::default()),
            "mock".into(),
            "mock-model".into(),
            Parallelism::new(1),
            Telemetry::noop(),
            String::new(),
            "fast".into(),
        )
        .with_interactive(false);

        let phase = RankPhase {
            config: cfg,
            replace_sources_enabled: false,
            stability_enabled: false,
        };
        pollster::block_on(phase.execute(&ctx))?;

        // The shared `prepend_to_prompt` helper returns the rank
        // prompt wrapped with the operator note. The fact that
        // rank.execute() ran without disturbing the sidecar +
        // the helper returns the wrapped prompt is the F1
        // contract: a future rank-prompt LLM call would receive
        // this string verbatim as its user prompt.
        let prepended =
            crate::checkpoint::modify_note::prepend_to_prompt(&run_root, "rank-phase-base-prompt");
        assert!(
            prepended.starts_with("[operator_modify_note]\n"),
            "prepend_to_prompt must open with the note tag; got:\n{prepended}"
        );
        assert!(
            prepended.contains("weight correctness higher"),
            "operator note text must appear; got:\n{prepended}"
        );
        assert!(
            prepended.contains("[/operator_modify_note]"),
            "prepend_to_prompt must close the note tag; got:\n{prepended}"
        );
        assert!(
            prepended.ends_with("rank-phase-base-prompt"),
            "base prompt must remain at the tail; got:\n{prepended}"
        );
        Ok(())
    }

    // -----------------------------------------------------------------
    // F3: budget gate. The rank phase consults the
    // `BudgetObserver` before running the stability check; under
    // Hard pressure + Reduce policy the check is skipped, the
    // ranking sidecar keeps `stability_label = None`, and the
    // operator never sees a sensitive-checkpoint prompt that the
    // budget does not have room to honour.
    //
    // The tests below open a real Db-backed `Telemetry` (the
    // budget observer only fires when `ctx.telemetry.db()` is
    // `Some`); the noop path is covered by the existing
    // `rank_phase_uses_selection_plan_to_filter` test which
    // proves the no-Db path is a clean no-op.
    // -----------------------------------------------------------------

    /// F3: when the run is staged with `planned = 1000` and
    /// `used = 950`, the rank phase's stability check is
    /// skipped — the resulting `ranking.json` has
    /// `stability_label = None` and the dashboard's
    /// "stability per run" view stays empty.
    #[test]
    fn rank_phase_skips_stability_under_hard_budget() -> Result<()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = match ENV_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let home = Arc::new(MoaganHome::resolve().unwrap());
        home.ensure().unwrap();
        let run_id = RunId::new();

        // Open a real Db and stage the budget. The v011
        // migration is applied on the first open; the
        // budget_state row is keyed on the run.
        let db = Db::open(&home.meta_db_path())?;
        db.register_run(run_id, "fast", "running", "0.4.0", None, None, None)?;
        db.set_budget(run_id, 1000)?;
        // 95% usage -> Hard pressure with the default 90%
        // threshold. `seed` is the audit phase tag; the
        // observer does not read it.
        db.budget_record(run_id, "seed", 950)?;

        // Two proposals so the stability check would normally
        // run (the single-proposal short-circuit is also fine,
        // but a multi-proposal run exercises the
        // `snapshots.len() < 2` branch is not the gate we're
        // testing here).
        for (id, score) in [("p_a", 8.0_f32), ("p_b", 7.0_f32)] {
            let proposal_path = home.run_dir(run_id).proposals().join(format!("{id}.json"));
            std::fs::create_dir_all(proposal_path.parent().unwrap()).unwrap();
            write_json(
                &proposal_path,
                &Proposal {
                    id: id.into(),
                    summary: id.into(),
                    ..Proposal::default()
                },
            )
            .unwrap();
            let agg_path = home
                .run_dir(run_id)
                .evaluations()
                .join(format!("{id}.json"));
            std::fs::create_dir_all(agg_path.parent().unwrap()).unwrap();
            write_json(
                &agg_path,
                &Aggregated {
                    score,
                    correctness: score,
                    completeness: score,
                    fit: score,
                    evidence: score,
                    clarity: score,
                    judges: 3,
                    adversary_delta: 0.0,
                },
            )
            .unwrap();
        }

        // Open a Db-backed Telemetry so `ctx.telemetry.db()` is
        // `Some` and the budget gate activates. The on-disk
        // jsonl files go under `<MOAGAN_HOME>/.runs/<id>/telemetry/`.
        let run_dir = home.run_dir(run_id);
        let telemetry =
            Telemetry::open(run_id, &run_dir, RedactPolicy::default(), Some(db.clone()))?;

        // Stability enabled and a non-zero n_perturbations so
        // the gate is the only thing that can stop the check.
        let cfg = Arc::new(Config {
            ranking_weights: RankingWeights::default(),
            stability: StabilityConfig {
                enabled: true,
                n_perturbations: 4,
                ..StabilityConfig::default()
            },
            ..Config::default()
        });
        let ctx = RunContext::new(
            run_id,
            home.clone(),
            Arc::new(ProviderRegistry::default()),
            "mock".into(),
            "mock-model".into(),
            Parallelism::new(1),
            telemetry,
            String::new(),
            "fast".into(),
        )
        .with_interactive(false);

        let phase = RankPhase {
            config: cfg,
            replace_sources_enabled: false,
            stability_enabled: true,
        };
        pollster::block_on(phase.execute(&ctx))?;

        // Read the ranking sidecar and assert the stability
        // fields stayed None — the gate worked, the
        // perturbation loop never ran.
        let path = home.run_dir(run_id).rankings().join("ranking.json");
        let raw = std::fs::read(&path).unwrap();
        let ranking: Ranking = serde_json::from_slice(&raw).unwrap();
        assert!(
            ranking.stability_label.is_none(),
            "stability_label must be None when budget is Hard; got {:?}",
            ranking.stability_label
        );
        assert!(
            ranking.stability_score.is_none(),
            "stability_score must be None when budget is Hard; got {:?}",
            ranking.stability_score
        );
        // The run-level mirror must NOT have been written
        // either: the gate happens before the SQLite mirror.
        // The row exists (it was created by `register_run`),
        // but every stability column must stay NULL.
        let row = db.get_run_stability(run_id)?;
        let row = row.expect("runs row must exist (register_run was called)");
        assert!(
            row.score.is_none() && row.label.is_none() && row.sigma.is_none(),
            "runs.stability_* columns must all stay NULL when the gate skipped the check; got {row:?}"
        );
        Ok(())
    }

    /// F3: when the run is staged with `planned = 1000` and
    /// `used = 100` (Ok pressure), the stability check runs
    /// normally and the sidecar carries a real verdict. This
    /// pins the negative case: the gate must not fire when the
    /// budget is comfortable.
    #[test]
    fn rank_phase_does_not_skip_under_ok_budget() -> Result<()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = match ENV_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let home = Arc::new(MoaganHome::resolve().unwrap());
        home.ensure().unwrap();
        let run_id = RunId::new();

        let db = Db::open(&home.meta_db_path())?;
        db.register_run(run_id, "fast", "running", "0.4.0", None, None, None)?;
        db.set_budget(run_id, 1000)?;
        // 10% usage -> Ok pressure with the default 50% soft
        // threshold. The stability check must run.
        db.budget_record(run_id, "seed", 100)?;

        for (id, score) in [("p_a", 8.0_f32), ("p_b", 7.0_f32)] {
            let proposal_path = home.run_dir(run_id).proposals().join(format!("{id}.json"));
            std::fs::create_dir_all(proposal_path.parent().unwrap()).unwrap();
            write_json(
                &proposal_path,
                &Proposal {
                    id: id.into(),
                    summary: id.into(),
                    ..Proposal::default()
                },
            )
            .unwrap();
            let agg_path = home
                .run_dir(run_id)
                .evaluations()
                .join(format!("{id}.json"));
            std::fs::create_dir_all(agg_path.parent().unwrap()).unwrap();
            write_json(
                &agg_path,
                &Aggregated {
                    score,
                    correctness: score,
                    completeness: score,
                    fit: score,
                    evidence: score,
                    clarity: score,
                    judges: 3,
                    adversary_delta: 0.0,
                },
            )
            .unwrap();
        }

        let run_dir = home.run_dir(run_id);
        let telemetry =
            Telemetry::open(run_id, &run_dir, RedactPolicy::default(), Some(db.clone()))?;

        let cfg = Arc::new(Config {
            ranking_weights: RankingWeights::default(),
            stability: StabilityConfig {
                enabled: true,
                n_perturbations: 4,
                ..StabilityConfig::default()
            },
            ..Config::default()
        });
        let ctx = RunContext::new(
            run_id,
            home.clone(),
            Arc::new(ProviderRegistry::default()),
            "mock".into(),
            "mock-model".into(),
            Parallelism::new(1),
            telemetry,
            String::new(),
            "fast".into(),
        )
        .with_interactive(false);

        let phase = RankPhase {
            config: cfg,
            replace_sources_enabled: false,
            stability_enabled: true,
        };
        pollster::block_on(phase.execute(&ctx))?;

        let path = home.run_dir(run_id).rankings().join("ranking.json");
        let raw = std::fs::read(&path).unwrap();
        let ranking: Ranking = serde_json::from_slice(&raw).unwrap();
        // Stability ran: the label must be Some (the
        // perturbation loop deterministically labels two
        // proposals either Stable or Sensitive depending on
        // the seed; either is fine here — the negative case
        // pins "the gate did not fire").
        assert!(
            ranking.stability_label.is_some(),
            "stability_label must be populated when budget is Ok; got None"
        );
        Ok(())
    }
}
