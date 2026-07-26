//! Repair phase. For each proposal that failed the gate, send it to
//! the model with the issues and ask for a revised proposal.

use std::path::{Path, PathBuf};

use crate::domain::{Gate, Proposal, Repair};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Repair phase. Issues one repair per failed gate.
pub struct RepairPhase;

impl Phase for RepairPhase {
    fn name(&self) -> &'static str {
        "repair"
    }

    fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let validation_dir = ctx.run_dir().validation();
        let proposals_dir = ctx.run_dir().proposals();
        let revisions_dir = ctx.run_dir().revisions();
        std::fs::create_dir_all(&revisions_dir)?;
        let system = system_prompt(Role::Repair).to_owned();
        let mut paths = Vec::new();
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
            // Look up the matching proposal.
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let proposal_path: PathBuf = proposals_dir.join(format!("{stem}.json"));
            let proposal: Proposal = read_json(&proposal_path)?;
            let user = serde_json::to_string(&serde_json::json!({
                "proposal": proposal,
                "issues": gate.issues,
                "missing": gate.missing,
            }))
            .map_err(crate::Error::from)?;
            let mut repair: Repair = ctx.call_with_retry_parse(
                Role::Repair,
                system.clone(),
                user,
                "Repair: {id, summary, approach, tradeoffs[], evidence[], changes[]}",
                5,
            )?;
            if repair.id.is_empty() {
                repair.id = proposal.id.clone();
            }
            let out_path: PathBuf = revisions_dir.join(format!("{}_rev_0.json", proposal.id));
            write_json(&out_path, &repair)?;
            paths.push(out_path);
        }
        Ok(PhaseOutput::Repairs(paths))
    }
}

#[allow(dead_code)]
fn _proposal_marker(_: &Path) {}
