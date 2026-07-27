//! Rank phase. Reads every `evaluations/p_*.json`, builds the
//! `rankings/ranking.json` with the highest-scoring proposal as the
//! winner.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::domain::{RankEntry, Ranking};
use crate::error::Result;
use crate::phases::judge::Aggregated;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Rank phase.
pub struct RankPhase;

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
            entries.push(RankEntry {
                id,
                score: agg.score,
                reason: format!("avg of {} judges", agg.judges),
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
