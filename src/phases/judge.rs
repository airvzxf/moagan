//! Judge phase. For each proposal (or repair if available), gather
//! `judges` judge scores and average them. Writes
//! `evaluations/p_*.json`.

use std::path::PathBuf;

use crate::domain::JudgeScore;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::write_json;

/// Judge phase. `judges` judge scores per proposal.
pub struct JudgePhase {
    /// Number of judges per proposal.
    pub judges: u32,
}

impl Phase for JudgePhase {
    fn name(&self) -> &'static str {
        "judge"
    }

    fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let proposals_dir = ctx.run_dir().proposals();
        let revisions_dir = ctx.run_dir().revisions();
        let evaluations_dir = ctx.run_dir().evaluations();
        std::fs::create_dir_all(&evaluations_dir)?;
        let system = system_prompt(Role::Judge).to_owned();
        let mut paths = Vec::new();
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
            // If a repair exists, use it; otherwise use the original.
            let revision_path: PathBuf = revisions_dir.join(format!("{proposal_id}_rev_0.json"));
            let subject: serde_json::Value = if revision_path.exists() {
                serde_json::from_slice(&std::fs::read(&revision_path)?)?
            } else {
                serde_json::from_slice(&std::fs::read(&path)?)?
            };
            let user = serde_json::to_string(&subject).map_err(crate::Error::from)?;
            let mut scores = Vec::new();
            for _ in 0..self.judges {
                let resp = pollster::block_on(ctx.call(Role::Judge, system.clone(), user.clone()))?;
                let score: JudgeScore = pollster::block_on(ctx.parse_model_json(
                    Role::Judge,
                    &resp.text,
                    "JudgeScore: {score, criteria{correctness,completeness,fit,evidence,clarity}, comments}",
                ))?;
                scores.push(score);
            }
            let aggregate = aggregate(&scores);
            let out_path: PathBuf = evaluations_dir.join(format!("{proposal_id}.json"));
            write_json(&out_path, &aggregate)?;
            paths.push(out_path);
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
    }
}

/// Aggregated judge score.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
}
