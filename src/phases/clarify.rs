//! Clarify phase. Reads `brief.json` (the intake), asks the model to
//! produce the canonical brief, writes it back.

use crate::domain::Brief;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Clarify phase.
pub struct ClarifyPhase;

impl Phase for ClarifyPhase {
    fn name(&self) -> &'static str {
        "clarify"
    }

    fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let intake: serde_json::Value = read_json(&ctx.run_dir().brief())?;
        let user = serde_json::to_string(&intake).map_err(crate::Error::from)?;
        let system = system_prompt(Role::Clarify).to_owned();
        let brief: Brief = ctx.call_with_retry_parse(
            Role::Clarify,
            system,
            user,
            "Brief: {problem, objectives, deliverables, constraints, assumptions, non_goals, acceptance[], risks[]}",
            1,
        )?;
        write_json(&ctx.run_dir().brief(), &brief)?;
        Ok(PhaseOutput::Brief(ctx.run_dir().brief()))
    }
}
