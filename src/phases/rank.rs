//! Rank phase. Reads every `evaluations/p_*.json`, runs the multi-
//! criterion pipeline from T01-06 §16.12 (Pareto → SimHash cluster →
//! crowding-distance representatives), then writes
//! `rankings/ranking.json` with the highest-scoring representative as
//! the winner and the full weighted ranking alongside.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::Config;
use crate::domain::{Proposal, RankEntry, Ranking};
use crate::error::Result;
use crate::phases::judge::Aggregated;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::ranking::pareto::QualityVector;
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

        // Step 1: gather every evaluation together with the proposal
        // text (preferring the latest repair if present).
        let mut items: Vec<(String, Aggregated, String)> = Vec::new();
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
            let text = load_proposal_text(&proposals_dir, &revisions_dir, &id)
                .unwrap_or_else(|| id.clone());
            items.push((id, agg, text));
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
        let front_texts: Vec<String> = front.iter().map(|i| items[*i].2.clone()).collect();
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

        let _ = front_set; // front_set is informational for telemetry consumers
        let ranking = Ranking {
            ranked,
            representatives,
            winner,
        };
        let out_path: PathBuf = rankings_dir.join("ranking.json");
        write_json(&out_path, &ranking)?;
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
    items: &[(String, Aggregated, String)],
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

/// Load the proposal text (for clustering) by reading the original
/// proposal and falling back to the highest-numbered revision when
/// the model produced a repair. Returns `None` when neither exists.
fn load_proposal_text(
    proposals_dir: &std::path::Path,
    revisions_dir: &std::path::Path,
    proposal_id: &str,
) -> Option<String> {
    let proposal_path = proposals_dir.join(format!("{proposal_id}.json"));
    let proposal: Proposal = match read_json(&proposal_path) {
        Ok(p) => p,
        Err(_) => {
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
            let n = latest_n?;
            let rev_path = revisions_dir.join(format!("{proposal_id}_rev_{n}.json"));
            read_json(&rev_path).ok()?
        }
    };
    Some(format!(
        "{} {} {} {}",
        proposal.summary,
        proposal.approach,
        proposal.tradeoffs.join(" "),
        proposal.evidence.join(" ")
    ))
}

#[cfg(test)]
mod tests {
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
}
