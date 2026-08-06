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
//!
//! E9 (catalog 10-integrada-v0 §D.20): the raw prompt is normalised
//! before it reaches the LLM. Three passes run in order:
//!
//!  1. Byte-cap at [`MAX_NORMALIZED_INPUT_BYTES`] (256 KiB).
//!     Oversized briefs are truncated with a warning; the rest of
//!     the pipeline keeps working without ballooning the LLM
//!     context.
//!  2. BOM strip. A leading `\u{FEFF}` is dropped so UTF-8 editors
//!     that save with BOM don't trip up the model.
//!  3. Control token strip. ASCII control bytes (< 0x20) except
//!     `\n` and `\t` are removed so accidentally-pasted terminal
//!     escapes or stray NULs don't leak into the prompt.
//!
//! The normalised string is fed both to the LLM call and persisted
//! in `Intake.raw_prompt` so a re-run with the same CLI prompt
//! reproduces the same downstream cache key.

use async_trait::async_trait;

use crate::checkpoint::{Checkpoint, CheckpointKind, CheckpointOpts};
use crate::domain::Intake;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// E9: hard byte cap for the normalised raw prompt. Briefs larger
/// than 256 KiB are truncated before the LLM call so the context
/// budget cannot be exhausted by a single paste. Matches
/// proposal-03 §D.20.4.
pub const MAX_NORMALIZED_INPUT_BYTES: usize = 256 * 1024;

/// Intake phase.
pub struct IntakePhase;

#[async_trait]
impl Phase for IntakePhase {
    fn name(&self) -> &'static str {
        "intake"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let system = system_prompt(Role::Intake).to_owned();
        // E9: normalise the raw prompt BEFORE the LLM call. The
        // normalised string is what the model sees AND what we
        // persist in `Intake.raw_prompt` so a re-run is
        // reproducible.
        let normalised = normalize_raw_prompt(&ctx.raw_prompt);
        let user = build_user_message(ctx, &normalised)?;
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
/// than user-authored text. The normalised raw prompt (E9) is the
/// version of the prompt that reaches the LLM.
fn build_user_message(ctx: &RunContext, normalised_raw: &str) -> Result<String> {
    let Some(block) = ctx.context_block.as_ref() else {
        return Ok(normalised_raw.to_owned());
    };
    let envelope = format!(
        "[context]\n{block}\n[/context]\n\n[user prompt]\n{normalised_raw}\n[/user prompt]\n"
    );
    Ok(envelope)
}

/// E9 (catalog 10-integrada-v0 §D.20): apply the three safety
/// passes to the raw prompt before it reaches the LLM. The order
/// matters: BOM strip first (so the BOM doesn't count against the
/// byte cap), control-token strip next (cheap, runs on the full
/// string), byte cap last (so the cap operates on the cleaned
/// text). The function is total and side-effect free, so callers
/// can run it without touching the run context.
pub(crate) fn normalize_raw_prompt(raw: &str) -> String {
    // 1. BOM strip. A leading BOM (U+FEFF) is invisible in most
    //    editors but breaks the LLM's tokeniser on the first
    //    token. Drop it unconditionally when present.
    let no_bom: &str = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);

    // 2. Control-token strip. Remove ASCII control bytes (< 0x20)
    //    except LF (`\n`) and tab (`\t`). Other whitespace (CR, FF,
    //    VT) and NULs do not belong in a natural-language prompt.
    let no_control: String = no_bom
        .chars()
        .filter(|c| {
            if *c == '\n' || *c == '\t' {
                true
            } else {
                let code = *c as u32;
                code >= 0x20
            }
        })
        .collect();

    // 3. Byte cap. When the cleaned text still exceeds the cap,
    //    truncate and surface a warning so the operator can see
    //    "this prompt was too big" in the telemetry stream. The
    //    truncation is character-aware (operates on the
    //    already-cleaned `String`) so we never slice mid-codepoint.
    if no_control.len() > MAX_NORMALIZED_INPUT_BYTES {
        let mut truncated = String::with_capacity(MAX_NORMALIZED_INPUT_BYTES);
        for c in no_control.chars() {
            if truncated.len() + c.len_utf8() > MAX_NORMALIZED_INPUT_BYTES {
                break;
            }
            truncated.push(c);
        }
        tracing::warn!(
            original_bytes = raw.len(),
            truncated_bytes = truncated.len(),
            cap_bytes = MAX_NORMALIZED_INPUT_BYTES,
            stage = "intake.normalized_truncated",
            "Intake phase"
        );
        truncated
    } else {
        no_control
    }
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
    /// raw prompt verbatim. E9: the supplied `normalised_raw` is
    /// the post-normalisation string, so the test pins the
    /// contract that normalisation happens before the `[context]`
    /// envelope is composed.
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
        let normalised = normalize_raw_prompt(&with.raw_prompt);
        let msg = build_user_message(&with, &normalised).unwrap();
        assert!(msg.contains("[context]"));
        assert!(msg.contains("# ctx"));
        assert!(msg.contains("[/context]"));
        assert!(msg.contains("[user prompt]"));
        assert!(msg.contains("hello"));

        let without = ctx;
        let normalised = normalize_raw_prompt(&without.raw_prompt);
        let msg = build_user_message(&without, &normalised).unwrap();
        assert_eq!(msg, "hello");
    }

    // -- E9: normalize_raw_prompt --------------------------------------

    /// E9: an oversized raw prompt is truncated at the byte cap.
    /// The cap is 256 KiB; the test pushes 300 KiB and expects the
    /// output to be at the cap, never larger. UTF-8 boundary safety
    /// is also pinned (the truncation walks chars, not bytes).
    #[test]
    fn intake_truncates_oversized_brief() {
        let big = "a".repeat(MAX_NORMALIZED_INPUT_BYTES + 50_000);
        let out = normalize_raw_prompt(&big);
        assert!(
            out.len() <= MAX_NORMALIZED_INPUT_BYTES,
            "truncated len {} > cap {}",
            out.len(),
            MAX_NORMALIZED_INPUT_BYTES
        );
        // Sanity: ASCII input means chars().count() == len().
        assert_eq!(out.chars().count(), out.len());
    }

    /// E9: a raw prompt that starts with the UTF-8 BOM (U+FEFF)
    /// has the BOM stripped. The rest of the text is preserved
    /// verbatim so the model's first token is meaningful. And:
    /// a prompt without a BOM is untouched (idempotent on the
    /// non-prefixed input).
    #[test]
    fn intake_strips_bom() {
        let mut with_bom = String::from("hello world");
        with_bom.insert(0, '\u{FEFF}');
        let out = normalize_raw_prompt(&with_bom);
        assert_eq!(out, "hello world");
        assert!(!out.starts_with('\u{FEFF}'));
        let without_bom = "hello world";
        let out = normalize_raw_prompt(without_bom);
        assert_eq!(out, "hello world");
    }

    /// E9: ASCII control bytes (< 0x20) except `\n` and `\t` are
    /// removed from the raw prompt. NULs, CRs, FF, VT, BEL, ESC,
    /// etc. — all dropped. Newlines and tabs preserved (they're
    /// legitimate whitespace in natural-language prompts). The
    /// check also covers that multi-byte UTF-8 characters with
    /// codepoints >= 0x80 are NOT touched (the `< 0x20` test only
    /// matches the ASCII control range).
    #[test]
    fn intake_strips_control_tokens_preserving_newlines() {
        // Build a string with a deliberate mix of control bytes
        // (NUL, SOH, STX, ETX, CR, SO, US) and legitimate
        // whitespace, plus a non-ASCII codepoint (`ñ`) to confirm
        // the filter does not touch multi-byte UTF-8 sequences.
        // Bare CR is constructed via `push('\r')` so the source
        // file stays CR-free (Rust strings forbid a literal 0x0D).
        let mut raw = String::from("line1\nline2\tcol2\n");
        for c in ['\0', '\u{01}', '\u{02}', '\u{03}', '\r', '\u{0E}', '\u{1F}'] {
            raw.push(c);
        }
        raw.push_str("acento: ñ");
        let out = normalize_raw_prompt(&raw);
        // Newline + tab preserved, control bytes gone.
        assert!(out.contains("line1\nline2\tcol2"));
        // Non-ASCII preserved.
        assert!(out.contains("ñ"));
        // No remaining control bytes (< 0x20) other than the
        // legitimate newline / tab.
        for c in out.chars() {
            let code = c as u32;
            if code < 0x20 {
                assert!(c == '\n' || c == '\t', "leaked control byte: {c:?}");
            }
        }
        // Sanity: the count of `\n` and `\t` matches the input.
        let newlines = out.chars().filter(|c| *c == '\n').count();
        let tabs = out.chars().filter(|c| *c == '\t').count();
        assert_eq!(newlines, 2, "expected 2 newlines preserved");
        assert_eq!(tabs, 1, "expected 1 tab preserved");
    }
}
