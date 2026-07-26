//! Critique phase. Two critics per proposal; writes
//! `critiques/p_*_critic_*.json`.

use std::path::{Path, PathBuf};

use crate::domain::{Critique, Proposal};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Critique phase. `critics_per_proposal` critics per proposal.
pub struct CritiquePhase {
    /// Number of critics to run per proposal.
    pub critics_per_proposal: u32,
}

impl Phase for CritiquePhase {
    fn name(&self) -> &'static str {
        "critique"
    }

    fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let proposals_dir = ctx.run_dir().proposals();
        let critiques_dir = ctx.run_dir().critiques();
        std::fs::create_dir_all(&critiques_dir)?;
        let system = system_prompt(Role::Critique).to_owned();
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(&proposals_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
                continue;
            }
            let proposal: Proposal = read_json(&path)?;
            for c in 0..self.critics_per_proposal {
                let id = format!("{}_critic_{c}", proposal.id);
                let user = serde_json::to_string(&proposal).map_err(crate::Error::from)?;
                let critique: Critique = ctx.call_with_retry_parse(
                    Role::Critique,
                    system.clone(),
                    user,
                    "Critique: {verdict, issues[], suggestions[]}",
                    5,
                )?;
                let out_path: PathBuf = critiques_dir.join(format!("{id}.json"));
                write_json(&out_path, &critique)?;
                paths.push(out_path);
            }
        }
        Ok(PhaseOutput::Critiques(paths))
    }
}

// `Proposal` is imported via the propose phase.
#[allow(dead_code)]
fn _proposal_marker(_: &Path) {}
