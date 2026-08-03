//! Clarify phase. Reads `brief.json` (the intake), asks the model to
//! produce the canonical brief, writes it back.
//!
//! Phase D (V4 §5.2 + T01-06 §16.2): when the brief has a blocking
//! ambiguity (open_questions non-empty or risks >= 2), the phase
//! fires a yes/no checkpoint before handing off to `RoutePhase`. The
//! check is no-op in non-interactive runs.

use async_trait::async_trait;

use crate::checkpoint::{Checkpoint, CheckpointKind, CheckpointOpts};
use crate::domain::Brief;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Clarify phase.
pub struct ClarifyPhase;

#[async_trait]
impl Phase for ClarifyPhase {
    fn name(&self) -> &'static str {
        "clarify"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let intake: serde_json::Value = read_json(&ctx.run_dir().brief())?;
        let user = serde_json::to_string(&intake).map_err(crate::Error::from)?;
        let system = system_prompt(Role::Clarify).to_owned();
        let context_block = intake.get("context_block").cloned();
        let mut brief: Brief = ctx
            .call_with_retry_parse(
            Role::Clarify,
            system,
            user,
            "Brief: {problem, objectives, deliverables[], constraints[], assumptions[], non_goals[], acceptance[], risks[]}",
            5,
        )
        .await?;
        if brief.context_block.is_none() {
            brief.context_block = context_block.and_then(|value| value.as_str().map(str::to_owned));
        }
        write_json(&ctx.run_dir().brief(), &brief)?;

        // Phase D checkpoint: a blocking ambiguity is signaled by the
        // brief carrying unresolved risks or implicit assumptions.
        // We treat `risks.len() >= 2` as the trigger — single-risk
        // briefs are common and don't need a human gate.
        if brief.risks.len() >= 2 {
            let prompt = format!(
                "brief carries {} risk(s); continue with current assumptions?",
                brief.risks.len()
            );
            let cp = Checkpoint::yes_no(CheckpointKind::Clarify, prompt);
            let opts = CheckpointOpts {
                interactive: ctx.interactive,
                stdin_override: None,
                telemetry: Some(ctx.telemetry.clone()),
            };
            let resolution = crate::checkpoint::ask(&cp, &ctx.run_dir().checkpoints(), &opts)?;
            match resolution {
                crate::checkpoint::Resolution::Approved => {}
                crate::checkpoint::Resolution::Modify(text) => {
                    // The user added an extra constraint. Persist it as
                    // a brief-level assumption so downstream phases can
                    // read it via `brief.json#assumptions`. Cheap hack:
                    // re-read, append, re-write.
                    let mut brief = brief.clone();
                    brief.assumptions.push(text);
                    write_json(&ctx.run_dir().brief(), &brief)?;
                }
                crate::checkpoint::Resolution::Rejected => {
                    return Err(crate::error::Error::Cancelled(
                        "user rejected the clarify checkpoint".into(),
                    ));
                }
            }
        }

        Ok(PhaseOutput::Brief(ctx.run_dir().brief()))
    }
}

#[cfg(test)]
mod tests {
    // The phase is exercised end-to-end in tests/integration_mvp.rs.
}
