//! Intake phase. Reads the raw prompt from `RunContext`, calls the
//! provider with the intake role, parses the JSON, writes
//! `intake.json`.
//!
//! Phase D (V4 §5.1 + T01-06 §16.1): after persisting the intake, the
//! phase fires a yes/no human checkpoint when the run is interactive
//! and the brief looks risky (multiple non-goals, blocking ambiguity,
//! risk flagged by the LLM). The check is no-op in non-interactive
//! runs and `Mode::Batch`.

use async_trait::async_trait;

use crate::checkpoint::{Checkpoint, CheckpointKind, CheckpointOpts};
use crate::domain::Intake;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::write_json;

/// Intake phase.
pub struct IntakePhase;

#[async_trait]
impl Phase for IntakePhase {
    fn name(&self) -> &'static str {
        "intake"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let system = system_prompt(Role::Intake).to_owned();
        let user = ctx.raw_prompt.clone();
        let intake: Intake = ctx
            .call_with_retry_parse(
                Role::Intake,
                system,
                user,
                "Intake: {problem, objectives, constraints, non_goals, open_questions, raw_prompt}",
                5,
            )
            .await?;
        let path = ctx.run_dir().final_dir().join("intake.json");
        let brief_path = ctx.run_dir().brief();
        write_json(&brief_path, &intake)?;
        write_json(&path, &intake)?;

        // Phase D checkpoint: only when the intake surfaced something
        // that warrants a human pause. Two trigger conditions:
        //
        // 1. The model returned at least one open question (means it
        //    couldn't classify the request unambiguously).
        // 2. The model flagged a constraint + a non-goal (the brief
        //    will have a hard contradiction that the clarify phase
        //    will need to reconcile).
        //
        // The check is opt-out: non-interactive runs (`Mode::Batch`,
        // `--non-interactive`) skip the prompt entirely and persist a
        // `<skipped:non_interactive>` marker for auditability.
        if !intake.open_questions.is_empty()
            || (!intake.constraints.is_empty() && !intake.non_goals.is_empty())
        {
            let prompt = format!(
                "intake surfaced {} open question(s) and {} constraint(s); continue?",
                intake.open_questions.len(),
                intake.constraints.len()
            );
            let cp = Checkpoint::yes_no(CheckpointKind::Intake, prompt);
            let opts = CheckpointOpts {
                interactive: ctx.interactive,
                stdin_override: None,
            };
            let resolution = crate::checkpoint::ask(&cp, &ctx.run_dir().checkpoints(), &opts)?;
            if !resolution.is_approved() {
                tracing::info!(
                    resolution = ?resolution,
                    stage = "intake.checkpoint.rejected",
                    "Intake phase"
                );
            }
        }

        Ok(PhaseOutput::Intake(brief_path))
    }
}

#[cfg(test)]
mod tests {
    // The phase is exercised end-to-end in tests/integration_mvp.rs.
}
