//! Intake phase. Reads the raw prompt from `RunContext`, calls the
//! provider with the intake role, parses the JSON, writes
//! `intake.json`.
//!
//! Phase D (V4 §5.1 + T01-06 §16.1): after persisting the intake, the
//! phase fires a yes/no human checkpoint when the run is interactive
//! and the brief looks risky (multiple non-goals, blocking ambiguity,
//! risk flagged by the LLM). The check is no-op in non-interactive
//! runs and `Mode::Batch`.
//!
//! Phase J (v0.3 «tercera etapa», sub-fase J): when `RunContext`
//! carries a `context_block` (because `moagan run --context <ref>`
//! was used), the phase prepends it to the LLM prompt in a fenced
//! `[context]...[/context]` block so the model sees the upstream
//! context before rephrasing the user's brief. The same block is
//! then written into `Intake.context_block` so the brief sidecar
//! roundtrips the verbatim text.

use async_trait::async_trait;

use crate::checkpoint::{Checkpoint, CheckpointKind, CheckpointOpts};
use crate::domain::Intake;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Intake phase.
pub struct IntakePhase;

#[async_trait]
impl Phase for IntakePhase {
    fn name(&self) -> &'static str {
        "intake"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let system = system_prompt(Role::Intake).to_owned();
        let user = build_user_message(ctx)?;
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
        write_json(&path, &intake)?;
        // The canonical `brief.json` is what `ClarifyPhase` reads
        // and overwrites with a proper `Brief`. Until then, it
        // carries the Intake. Phase J stamps `context_block` onto
        // the persisted JSON when `--context` was used so a
        // post-execution review can recover the exact prompt the
        // model saw without re-loading the context ref.
        if let Some(block) = ctx.context_block.as_ref() {
            let mut value = serde_json::to_value(&intake).map_err(crate::Error::from)?;
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "context_block".into(),
                    serde_json::Value::String(block.clone()),
                );
            }
            crate::atomic::writer::AtomicWriter::new().write(
                &brief_path,
                &serde_json::to_vec(&value).map_err(crate::Error::from)?,
            )?;
        } else {
            write_json(&brief_path, &intake)?;
        }

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
                telemetry: Some(ctx.telemetry.clone()),
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

/// Build the LLM user message. When `RunContext.context_block` is
/// `Some`, the block is prepended in a fenced `[context]...[/context]`
/// envelope so the model can recognise it as upstream context rather
/// than user-authored text.
fn build_user_message(ctx: &RunContext) -> Result<String> {
    let raw = ctx.raw_prompt.clone();
    let Some(block) = ctx.context_block.as_ref() else {
        return Ok(raw);
    };
    let envelope = format!(
        "[context]\n{block}\n[/context]\n\n[user prompt]\n{raw}\n[/user prompt]\n"
    );
    Ok(envelope)
}

/// Read the intake sidecar, applying the phase's `context_block`
/// stamp. Exposed for the integration tests so they don't have to
/// drive the LLM just to assert the round-trip.
#[doc(hidden)]
pub fn read_intake_with_context(path: &std::path::Path) -> Result<Intake> {
    read_json(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `build_user_message` wraps the raw prompt in a context block
    /// when `ctx.context_block` is `Some`, otherwise returns the
    /// raw prompt verbatim.
    #[test]
    fn build_user_message_with_context() {
        let home = std::sync::Arc::new(crate::fs_layout::MoaganHome::at(std::path::PathBuf::from(
            "/tmp/moagan-test",
        )));
        let ctx = RunContext::new(
            crate::ids::RunId::default(),
            home,
            std::sync::Arc::new(crate::llm::ProviderRegistry::default()),
            "mock".into(),
            "mock-model".into(),
            crate::execution::Parallelism::new(1),
            crate::telemetry::Telemetry::noop(),
            "hello".into(),
            "fast".into(),
        );
        let with = ctx
            .clone()
            .with_context(Some("# ctx".into()), None, None, Vec::new(), None);
        let msg = build_user_message(&with).unwrap();
        assert!(msg.contains("[context]"));
        assert!(msg.contains("# ctx"));
        assert!(msg.contains("[/context]"));
        assert!(msg.contains("[user prompt]"));
        assert!(msg.contains("hello"));

        let without = ctx;
        let msg = build_user_message(&without).unwrap();
        assert_eq!(msg, "hello");
    }
}
