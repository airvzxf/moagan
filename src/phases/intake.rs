//! Intake phase. Reads the raw prompt from `RunContext`, calls the
//! provider with the intake role, parses the JSON, writes
//! `intake.json`.

use crate::domain::Intake;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::write_json;

/// Intake phase.
pub struct IntakePhase;

impl Phase for IntakePhase {
    fn name(&self) -> &'static str {
        "intake"
    }

    fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let system = system_prompt(Role::Intake).to_owned();
        let user = ctx.raw_prompt.clone();
        let intake: Intake = ctx.call_with_retry_parse(
            Role::Intake,
            system,
            user,
            "Intake: {problem, objectives, constraints, non_goals, open_questions, raw_prompt}",
            5,
        )?;
        let path = ctx.run_dir().final_dir().join("intake.json");
        let brief_path = ctx.run_dir().brief();
        write_json(&brief_path, &intake)?;
        write_json(&path, &intake)?;
        Ok(PhaseOutput::Intake(brief_path))
    }
}

#[cfg(test)]
mod tests {
    // The phase is exercised end-to-end in tests/integration_mvp.rs.
}
