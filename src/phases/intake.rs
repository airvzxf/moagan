//! Intake phase. Reads the raw prompt from `RunContext`, calls the
//! provider with the intake role, parses the JSON, writes
//! `intake.json`.

use crate::domain::Intake;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{parse_model_json, write_json};

/// Intake phase.
pub struct IntakePhase;

impl Phase for IntakePhase {
    fn name(&self) -> &'static str {
        "intake"
    }

    fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let system = system_prompt(Role::Intake).to_owned();
        let user = ctx.raw_prompt.clone();
        // The intake response is synchronous for the MVP. Async call
        // would be added in v0.2; here we rely on the synchronous
        // mock path so the smoke test runs deterministically.
        let resp = pollster::block_on(ctx.call(Role::Intake, system, user))?;
        let intake: Intake = parse_model_json(&resp.text)?;
        let path = ctx.run_dir().final_dir().join("intake.json");
        // intake is technically a phase artefact; for v0.1 we keep it
        // in the brief slot of the run directory.
        let brief_path = ctx.run_dir().brief();
        write_json(&brief_path, &intake)?;
        // Also write a sidecar named intake.json under final_dir for
        // downstream tools.
        write_json(&path, &intake)?;
        Ok(PhaseOutput::Intake(brief_path))
    }
}

#[cfg(test)]
mod tests {
    // The phase is exercised end-to-end in tests/integration_mvp.rs.
}
