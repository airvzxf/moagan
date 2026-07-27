//! Propose phase. Reads the brief, asks the model for N proposals in
//! parallel, writes `proposals/p_001.json`, `proposals/p_002.json`, …

use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;

use crate::domain::Proposal;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Propose phase. Generates `count` proposals concurrently, bounded
/// by `RunContext::parallelism` (default 4). The wall-clock cost of
/// this phase is `ceil(count / max_parallelism) * (model_latency)`,
/// not `count * model_latency`.
pub struct ProposePhase {
    /// Number of proposals to generate.
    pub count: u32,
}

#[async_trait]
impl Phase for ProposePhase {
    fn name(&self) -> &'static str {
        "propose"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let brief: serde_json::Value = read_json(&ctx.run_dir().brief())?;
        let user = serde_json::to_string(&brief).map_err(crate::Error::from)?;
        let system = system_prompt(Role::Propose).to_owned();
        let proposals_dir = ctx.run_dir().proposals();
        std::fs::create_dir_all(&proposals_dir)?;

        let count = self.count as usize;
        let _guard = ctx.parallelism.acquire_many(count).await?;
        let system_arc = std::sync::Arc::new(system);
        let user_arc = std::sync::Arc::new(user);

        let futures = (0..count).map(|i| {
            let id = format!("p_{i:03}");
            let user_with_id = format!("{}\n\nUse id=\"{id}\" in the output.", user_arc.as_str());
            let ctx = ctx.clone();
            let system_arc = std::sync::Arc::clone(&system_arc);
            let id_for_default = id.clone();
            async move {
                let mut proposal: Proposal = ctx
                    .call_with_retry_parse(
                        Role::Propose,
                        system_arc.as_str().to_owned(),
                        user_with_id,
                        "Proposal: {id, summary, approach, tradeoffs[], evidence[]}",
                        5,
                    )
                    .await?;
                if proposal.id.is_empty() {
                    proposal.id = id_for_default;
                }
                Ok::<(String, Proposal), crate::error::Error>((id, proposal))
            }
        });

        let results = join_all(futures).await;
        let mut paths = Vec::with_capacity(count);
        for r in results {
            let (id, proposal) = r?;
            let path: PathBuf = proposals_dir.join(format!("{id}.json"));
            write_json(&path, &proposal)?;
            paths.push(path);
        }
        Ok(PhaseOutput::Proposals(paths))
    }
}
