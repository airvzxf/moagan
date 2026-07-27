//! Judge phase. For each proposal (or repair if available), gather
//! `judges` judge scores and average them. Writes
//! `evaluations/p_*.json`.

use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;

use crate::domain::JudgeScore;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::write_json;

/// Judge phase. `judges` judge scores per proposal. All proposals ×
/// judges are scheduled concurrently up to the global parallelism
/// cap; each proposal's individual scores are aggregated once its
/// `judges` calls complete.
pub struct JudgePhase {
    /// Number of judges per proposal.
    pub judges: u32,
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
        std::fs::create_dir_all(&evaluations_dir)?;
        let system = system_prompt(Role::Judge).to_owned();

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
        let _guard = ctx.parallelism.acquire_many(total).await?;
        let system_arc = std::sync::Arc::new(system);
        let evaluations_dir_arc = std::sync::Arc::new(evaluations_dir);

        let futures = subjects.iter().flat_map(|(proposal_id, subject)| {
            let user = serde_json::to_string(subject).unwrap_or_default();
            let prop_id = proposal_id.clone();
            let system_arc = std::sync::Arc::clone(&system_arc);
            let evaluations_dir_arc = std::sync::Arc::clone(&evaluations_dir_arc);
            (0..judges).map(move |_j| {
                let ctx = ctx.clone();
                let system_arc = std::sync::Arc::clone(&system_arc);
                let user = user.clone();
                let proposal_id = prop_id.clone();
                let evaluations_dir = std::sync::Arc::clone(&evaluations_dir_arc);
                async move {
                    let score: JudgeScore = ctx
                        .call_with_retry_parse(
                            Role::Judge,
                            system_arc.as_str().to_owned(),
                            user,
                            "JudgeScore: {score, criteria{correctness,completeness,fit,evidence,clarity}, comments}",
                            5,
                        )
                        .await?;
                    Ok::<(String, JudgeScore, std::sync::Arc<PathBuf>), crate::error::Error>((proposal_id, score, evaluations_dir))
                }
            })
        });

        let results = join_all(futures).await;

        // Aggregate per proposal.
        use std::collections::BTreeMap;
        let mut by_proposal: BTreeMap<String, Vec<JudgeScore>> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut first_dir: Option<std::sync::Arc<PathBuf>> = None;
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

        let mut paths = Vec::with_capacity(order.len());
        for proposal_id in order {
            let scores = by_proposal.remove(&proposal_id).unwrap_or_default();
            let agg = aggregate(&scores);
            let out_path: PathBuf = evaluations_dir.join(format!("{proposal_id}.json"));
            write_json(&out_path, &agg)?;
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
