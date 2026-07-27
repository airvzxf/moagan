//! Critique phase. Two critics per proposal; writes
//! `critiques/p_*_critic_*.json`.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures::future::join_all;

use crate::domain::{Critique, Proposal};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Critique phase. `critics_per_proposal` critics per proposal, all
/// proposals × critics running concurrently up to the global
/// parallelism cap.
pub struct CritiquePhase {
    /// Number of critics to run per proposal.
    pub critics_per_proposal: u32,
}

#[async_trait]
impl Phase for CritiquePhase {
    fn name(&self) -> &'static str {
        "critique"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let proposals_dir = ctx.run_dir().proposals();
        let critiques_dir = ctx.run_dir().critiques();
        std::fs::create_dir_all(&critiques_dir)?;
        let system = system_prompt(Role::Critique).to_owned();

        // Pre-load all proposals serially (disk I/O is cheap and
        // happens concurrently with the LLM calls below).
        let mut proposals: Vec<Proposal> = Vec::new();
        for entry in std::fs::read_dir(&proposals_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
                continue;
            }
            proposals.push(read_json(&path)?);
        }

        let critics = self.critics_per_proposal as usize;
        let total = proposals.len() * critics;
        let _guard = ctx.parallelism.acquire_many(total).await?;
        let system_arc = std::sync::Arc::new(system);
        let critiques_dir_arc = std::sync::Arc::new(critiques_dir);

        let futures = proposals.iter().flat_map(|p| {
            let user_base = serde_json::to_string(p).unwrap_or_default();
            let prop_id = p.id.clone();
            let system_arc = std::sync::Arc::clone(&system_arc);
            let critiques_dir_arc = std::sync::Arc::clone(&critiques_dir_arc);
            (0..critics).map(move |c| {
                let id = format!("{}_critic_{c}", prop_id);
                // Differentiate each critic's prompt so the cross-run
                // cache treats them as distinct calls (otherwise the
                // second critic on a given proposal would always
                // return the first critic's cached response).
                let user = format!("[critic_index={c}]\n{user_base}");
                let ctx = ctx.clone();
                let system_arc = std::sync::Arc::clone(&system_arc);
                let critiques_dir = std::sync::Arc::clone(&critiques_dir_arc);
                let id_clone = id.clone();
                async move {
                    let critique: Critique = ctx
                        .call_with_retry_parse(
                            Role::Critique,
                            system_arc.as_str().to_owned(),
                            user,
                            "Critique: {verdict, issues[], suggestions[]}",
                            5,
                        )
                        .await?;
                    let out_path: PathBuf = critiques_dir.join(format!("{id_clone}.json"));
                    write_json(&out_path, &critique)?;
                    Ok::<PathBuf, crate::error::Error>(out_path)
                }
            })
        });

        let results = join_all(futures).await;
        let mut paths = Vec::with_capacity(total);
        for r in results {
            paths.push(r?);
        }
        Ok(PhaseOutput::Critiques(paths))
    }
}

// `Proposal` is imported via the propose phase.
#[allow(dead_code)]
fn _proposal_marker(_: &Path) {}
