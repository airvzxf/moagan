//! Rank phase. Reads every `evaluations/p_*.json`, computes the
//! weighted score using the per-criterion weights in the
//! `Config`, and writes `rankings/ranking.json` with the
//! highest-scoring proposal as the winner.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::config::Config;
use crate::domain::{RankEntry, Ranking};
use crate::error::Result;
use crate::phases::judge::Aggregated;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Rank phase. The cardinality is the number of proposals emitted by
/// the previous phase; ordering is by the weighted score that
/// `Config::ranking_weights` produces.
pub struct RankPhase {
    /// Shared config so the rank phase can read the per-criterion
    /// weights without going through `RunContext`.
    pub config: std::sync::Arc<Config>,
}

#[async_trait]
impl Phase for RankPhase {
    fn name(&self) -> &'static str {
        "rank"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let evaluations_dir = ctx.run_dir().evaluations();
        let rankings_dir = ctx.run_dir().rankings();
        std::fs::create_dir_all(&rankings_dir)?;
        let mut entries = Vec::new();
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
            let score = self.config.ranking_weights.weighted_score(
                agg.correctness,
                agg.completeness,
                agg.fit,
                agg.evidence,
                agg.clarity,
                agg.score,
            );
            entries.push(RankEntry {
                id,
                score,
                reason: format!(
                    "weighted avg of {} judges (correctness {:.2}, completeness {:.2}, fit {:.2}, evidence {:.2}, clarity {:.2})",
                    agg.judges, agg.correctness, agg.completeness, agg.fit, agg.evidence, agg.clarity
                ),
            });
        }
        entries.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let winner = entries.first().map(|e| e.id.clone()).unwrap_or_default();
        let ranking = Ranking {
            ranked: entries,
            winner,
        };
        let out_path: PathBuf = rankings_dir.join("ranking.json");
        write_json(&out_path, &ranking)?;
        Ok(PhaseOutput::Ranking(out_path))
    }
}
