//! Repair phase. For each proposal that failed the gate, send it to
//! the model with the issues and ask for a revised proposal. Up to
//! `max_rounds` repair passes per failed proposal (T01-06 §16.10).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join_all;

use crate::config::Config;
use crate::domain::{Gate, Proposal, Repair};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Repair phase. Up to `max_rounds` repair passes per failed proposal
/// (T01-06 §16.10). Multiple failed proposals are repaired in
/// parallel up to the global parallelism cap; multiple rounds for
/// the same proposal run sequentially so each round sees the output
/// of the previous one.
pub struct RepairPhase {
    /// Maximum repair rounds per failed proposal. Default 5 per the
    /// v0.1 operator preference (overrides the spec §16.10 baseline
    /// of 0..2).
    pub max_rounds: u32,
}

impl Default for RepairPhase {
    fn default() -> Self {
        Self { max_rounds: 5 }
    }
}

impl RepairPhase {
    /// Build a `RepairPhase` from the active `Config`.
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            max_rounds: cfg.repair_max_rounds,
        }
    }
}

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
            // Skip sidecar files written by sibling phases. The
            // validate phase persists `p_<id>.evidence.json` here
            // and the gate phase persists `p_<id>.json`. Anything
            // else (e.g. `p_<id>.evidence.json` or future audit
            // files) is not a Gate artefact and must not be parsed
            // as one.
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if stem.contains('.') {
                continue;
            }
            let gate: Gate = read_json(&path)?;
            if gate.pass {
                continue;
            }
            let proposal_path: PathBuf = proposals_dir.join(format!("{stem}.json"));
            let proposal: Proposal = read_json(&proposal_path)?;
            entries.push((path, proposal, gate));
        }

        let total = entries.len();
        if total == 0 {
            return Ok(PhaseOutput::Repairs(Vec::new()));
        }
        let system_arc = Arc::new(system);
        let max_rounds = self.max_rounds.max(1);

        let futures = entries.into_iter().map(|(_path, proposal, gate)| {
            let user = serde_json::to_string(&serde_json::json!({
                "proposal": proposal,
                "issues": gate.issues,
                "missing": gate.missing,
            }))
            .unwrap_or_default();
            let ctx = ctx.clone();
            let system_arc = Arc::clone(&system_arc);
            let revisions_dir = revisions_dir.clone();
            let proposal_id = proposal.id.clone();
            async move {
                let _permit = ctx.parallelism.acquire().await?;
                let mut paths: Vec<PathBuf> = Vec::with_capacity(max_rounds as usize);
                for round in 0..max_rounds {
                    let user_payload = if round == 0 {
                        user.clone()
                    } else {
                        // Re-feed the previous repair output with the
                        // accumulated context so the model sees its own
                        // prior attempt and the original issues.
                        let last = paths
                            .last()
                            .and_then(|p| read_json::<Repair>(p).ok())
                            .unwrap_or_default();
                        serde_json::to_string(&serde_json::json!({
                            "proposal": last,
                            "issues": gate.issues,
                            "missing": gate.missing,
                            "round": round + 1,
                        }))
                        .unwrap_or_default()
                    };
                    let mut repair: Repair = ctx
                        .call_with_retry_parse(
                            Role::Repair,
                            system_arc.as_str().to_owned(),
                            user_payload,
                            "Repair: {id, summary, approach, tradeoffs[], evidence[], changes[]}",
                            5,
                        )
                        .await?;
                    if repair.id.is_empty() {
                        repair.id = proposal_id.clone();
                    }
                    let out_path: PathBuf =
                        revisions_dir.join(format!("{proposal_id}_rev_{round}.json"));
                    write_json(&out_path, &repair)?;
                    paths.push(out_path);
                }
                Ok::<Vec<PathBuf>, crate::error::Error>(paths)
            }
        });

        let results = join_all(futures).await;
        let mut flat_paths: Vec<PathBuf> = Vec::with_capacity(total * max_rounds as usize);
        for r in results {
            flat_paths.extend(r?);
        }
        Ok(PhaseOutput::Repairs(flat_paths))
    }
}

#[allow(dead_code)]
fn _proposal_marker(_: &Path) {}
