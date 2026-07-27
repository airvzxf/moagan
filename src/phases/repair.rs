//! Repair phase. For each proposal that failed the gate, send it to
//! the model with the issues and ask for a revised proposal.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures::future::join_all;

use crate::domain::{Gate, Proposal, Repair};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Repair phase. Issues one repair per failed gate. Multiple failed
/// gates are repaired concurrently up to the global parallelism cap.
pub struct RepairPhase;

#[async_trait]
impl Phase for RepairPhase {
    fn name(&self) -> &'static str {
        "repair"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let validation_dir = ctx.run_dir().validation();
        let proposals_dir = ctx.run_dir().proposals();
        let revisions_dir = ctx.run_dir().revisions();
        std::fs::create_dir_all(&revisions_dir)?;
        let system = system_prompt(Role::Repair).to_owned();

        // First pass: collect every failed gate and its proposal.
        let mut entries: Vec<(PathBuf, Proposal, Gate)> = Vec::new();
        for entry in std::fs::read_dir(&validation_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
                continue;
            }
            let gate: Gate = read_json(&path)?;
            if gate.pass {
                continue;
            }
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let proposal_path: PathBuf = proposals_dir.join(format!("{stem}.json"));
            let proposal: Proposal = read_json(&proposal_path)?;
            entries.push((path, proposal, gate));
        }

        let total = entries.len();
        if total == 0 {
            return Ok(PhaseOutput::Repairs(Vec::new()));
        }
        let _guard = ctx.parallelism.acquire_many(total).await?;
        let system_arc = std::sync::Arc::new(system);

        let futures = entries.into_iter().map(|(_path, proposal, gate)| {
            let user = serde_json::to_string(&serde_json::json!({
                "proposal": proposal,
                "issues": gate.issues,
                "missing": gate.missing,
            }))
            .unwrap_or_default();
            let ctx = ctx.clone();
            let system_arc = std::sync::Arc::clone(&system_arc);
            let revisions_dir = revisions_dir.clone();
            let proposal_id = proposal.id.clone();
            async move {
                let mut repair: Repair = ctx
                    .call_with_retry_parse(
                        Role::Repair,
                        system_arc.as_str().to_owned(),
                        user,
                        "Repair: {id, summary, approach, tradeoffs[], evidence[], changes[]}",
                        5,
                    )
                    .await?;
                if repair.id.is_empty() {
                    repair.id = proposal_id.clone();
                }
                let out_path: PathBuf = revisions_dir.join(format!("{proposal_id}_rev_0.json"));
                write_json(&out_path, &repair)?;
                Ok::<PathBuf, crate::error::Error>(out_path)
            }
        });

        let results = join_all(futures).await;
        let mut paths = Vec::with_capacity(total);
        for r in results {
            paths.push(r?);
        }
        Ok(PhaseOutput::Repairs(paths))
    }
}

#[allow(dead_code)]
fn _proposal_marker(_: &Path) {}
