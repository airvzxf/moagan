//! Propose phase. Reads the brief, asks the model for N proposals in
//! parallel, writes `proposals/p_001.json`, `proposals/p_002.json`, …

use std::path::PathBuf;

use crate::domain::Proposal;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{parse_model_json, read_json, write_json};

/// Propose phase. Generates `count` proposals in parallel.
pub struct ProposePhase {
    /// Number of proposals to generate.
    pub count: u32,
}

impl Phase for ProposePhase {
    fn name(&self) -> &'static str {
        "propose"
    }

    fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let brief: serde_json::Value = read_json(&ctx.run_dir().brief())?;
        let user = serde_json::to_string(&brief).map_err(crate::Error::from)?;
        let system = system_prompt(Role::Propose).to_owned();
        let proposals_dir = ctx.run_dir().proposals();
        std::fs::create_dir_all(&proposals_dir)?;
        let mut paths = Vec::new();
        for i in 0..self.count {
            let id = format!("p_{i:03}");
            // Force the model to use this id in the response. We inject
            // a constraint into the user message rather than relying on
            // the model to choose.
            let user_with_id = format!("{user}\n\nUse id=\"{id}\" in the output.");
            let resp = pollster::block_on(ctx.call(Role::Propose, system.clone(), user_with_id))?;
            let mut proposal: Proposal = parse_model_json(&resp.text)?;
            if proposal.id.is_empty() {
                proposal.id = id.clone();
            }
            let path: PathBuf = proposals_dir.join(format!("{id}.json"));
            write_json(&path, &proposal)?;
            paths.push(path);
        }
        Ok(PhaseOutput::Proposals(paths))
    }
}
