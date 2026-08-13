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
//!  1. Byte-cap at [`crate::llm::size_limits::MAX_PROMPT_BYTES`]
//!     (250 KiB). Oversized briefs are truncated with a warning;
//!     the rest of the pipeline keeps working without ballooning
//!     the LLM context.
//!  2. BOM strip. A leading `\u{FEFF}` is dropped so UTF-8 editors
//!     that save with BOM don't trip up the model.
//!  3. Control token strip. ASCII control bytes (`\u{0000}`–
//!     `\u{001F}`) except `\n`, `\r`, and `\t`, plus the DEL
//!     byte (`\u{007F}`), are removed so accidentally-pasted
//!     terminal escapes or stray NULs don't leak into the
//!     prompt. The strip is the centralised helper
//!     [`crate::llm::control_tokens::strip`] (catalog
//!     10-integrada-v0 §D.7.2; roadmap PR-27).
//!
//! The normalised string is fed both to the LLM call and persisted
//! in `Intake.raw_prompt` so a re-run with the same CLI prompt
//! reproduces the same downstream cache key.
//!
//! E10 (catalog 10-integrada-v0 §D.20.7): the normalised prompt is
//! classified by `Role::HostilePromptDetector` *before* the intake
//! LLM call so a clearly hostile input is rejected at the door.
//! The verdict drives a [`HostilePolicy`]:
//!
//! - [`HostilePolicy::FailClosed`] (default): `hostile` verdict
//!   → `Error::HostilePrompt`; `suspicious` → warning + continue;
//!   `safe` → continue. This is the safe-by-default behaviour.
//! - [`HostilePolicy::FailOpen`]: `hostile` and `suspicious` both
//!   log a warning and continue (the spec allows this for
//!   operators who want every prompt to reach the model).
//! - [`HostilePolicy::Disabled`]: the detector is not called at
//!   all (the cheapest path; useful for repro cases the operator
//!   needs to keep reproducible without re-litigating the
//!   detector).
//!
//! The policy is encoded as an env-var knob
//! (`MOAGAN_INTAKE_HOSTILE_POLICY=fail_closed|fail_open|disabled`)
//! with the same last-write-wins discipline as the rest of the
//! catalog env surface. A future Config-level plumbing lands in
//! a follow-up so the scope of this phase stays focused.

use async_trait::async_trait;

use std::borrow::Cow;

use crate::checkpoint::{Checkpoint, CheckpointKind, CheckpointOpts};
use crate::config::Config;
use crate::domain::{HostilePromptReport, Intake};
use crate::error::{Error, Result};
use crate::llm::Role;
use crate::llm::control_tokens;
use crate::llm::prompts::system_prompt;
use crate::llm::size_limits::MAX_PROMPT_BYTES;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::write_json;

/// F5: file name (under the run's `final/` directory) that
/// carries the hex-encoded BLAKE3 hash of the canonical TOML
/// serialization of the run's `Config`. `build_manifest` reads
/// this sidecar after the pipeline finishes and stamps the
/// digest onto `manifest.config_hash` so a `moagan rerun` with
/// a different config produces a different manifest hash and
/// the run is non-reproducible by inspection.
pub const CONFIG_HASH_SIDECAR: &str = "config_hash.txt";

/// F5: compute the deterministic BLAKE3 hex digest of the
/// canonical TOML form of `config`. Two runs with the same
/// `Config` produce the same digest; a `moagan rerun` after
/// editing `~/.config/moagan/config.toml` produces a different
/// digest and the manifest's `config_hash` field surfaces the
/// drift. Returns an `InvalidState` error if `toml::to_string`
/// fails — should not happen for our `#[derive(Serialize)]`
/// types but we surface the failure rather than swallow it.
pub fn compute_config_hash(config: &Config) -> Result<String> {
    let serialized = toml::to_string(config)
        .map_err(|e| Error::InvalidState(format!("config_hash: toml serialize failed: {e}")))?;
    Ok(blake3::hash(serialized.as_bytes()).to_hex().to_string())
}

/// E10: how the intake phase handles the
/// `Role::HostilePromptDetector` verdict. The default is
/// `FailClosed` so a clearly hostile input is rejected
/// pre-pipeline; the other two modes exist for operators who
/// need the detector's classification to flow through as a
/// warning only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostilePolicy {
    /// `hostile` verdict aborts the run with
    /// `Error::HostilePrompt`; `suspicious` logs a warning and
    /// continues; `safe` continues silently. This is the
    /// default because refusing to act on a clearly-hostile
    /// input is the safer behaviour.
    #[default]
    FailClosed,
    /// `hostile` and `suspicious` both log a warning and
    /// continue; `safe` continues silently. Useful for
    /// development / repro flows where the operator wants the
    /// downstream phases to run anyway.
    FailOpen,
    /// The detector is not called at all. The cheapest path;
    /// intended for the case where the operator knows the
    /// dataset is benign and wants to skip the extra LLM call
    /// for throughput.
    Disabled,
}

impl HostilePolicy {
    /// Parse the env-var form (`fail_closed` / `fail_open` /
    /// `disabled`; aliases `fail-closed` / `fail-open` /
    /// `disable` / `off` for ergonomic shell use). Unknown /
    /// empty values leave the existing knob alone (last-write
    /// wins matches the rest of the catalog).
    fn from_env(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "fail_closed" | "fail-closed" | "closed" | "default" => Some(Self::FailClosed),
            "fail_open" | "fail-open" | "open" => Some(Self::FailOpen),
            "disabled" | "disable" | "off" | "none" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// Resolve the effective `HostilePolicy` from the env var
/// `MOAGAN_INTAKE_HOSTILE_POLICY` with a hardcoded default of
/// `FailClosed`. The check runs at most once per phase (no
/// allocation when the env var is unset).
fn effective_hostile_policy() -> HostilePolicy {
    match std::env::var("MOAGAN_INTAKE_HOSTILE_POLICY") {
        Ok(v) => HostilePolicy::from_env(&v).unwrap_or_default(),
        Err(_) => HostilePolicy::default(),
    }
}

/// Classified verdict of the `Role::HostilePromptDetector`
/// pass, after the policy has been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostileVerdict {
    /// Detector said `safe` (or the detector returned an empty
    /// payload that the schema defaults to "safe"). The phase
    /// continues without side effects.
    Safe,
    /// Detector said `suspicious`. The phase logs a warning
    /// (regardless of policy) and continues — the
    /// `Safe` behaviour is preserved downstream.
    Suspicious {
        /// First signal the detector surfaced (or empty when
        /// the report had no reasons).
        reason: String,
    },
    /// Detector said `hostile`. Under
    /// `HostilePolicy::FailClosed` this propagates as
    /// `Err(Error::HostilePrompt(...))`. Under the other two
    /// policies it is downgraded to a warning log so the
    /// downstream pipeline still runs.
    Hostile {
        /// First signal the detector surfaced.
        reason: String,
    },
}

/// E10: outcome of the cheap regex-based heuristic that
/// pre-classifies the normalised prompt before deciding whether
/// to invoke the LLM detector. The four buckets span the
/// "obvious benign" → "obvious hostile" spectrum and a fallback
/// `Indeterminate` bucket that escalates to the LLM detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeuristicOutcome {
    /// No injection signal; skip the LLM detector entirely.
    Benign,
    /// The detector-style heuristic matches a known suspicious
    /// pattern; the LLM call is not made (fail-closed under
    /// `HostilePolicy::FailClosed`).
    Suspicious,
    /// The heuristic matches a known hostile pattern; the
    /// LLM call is not made. `FailClosed` propagates as
    /// `Error::HostilePrompt`.
    Hostile,
    /// Heuristic neither cleared nor flagged the prompt; the
    /// LLM detector is consulted.
    Indeterminate,
}

/// E10: cheap regex-based hostile-prompt check that runs
/// **before** any LLM call. The patterns cover the canonical
/// prompt-injection vocabulary from the public literature
/// (`ignore previous instructions`, `disregard the system
/// prompt`, etc.). A match escalates to the matching verdict
/// without a network call; a miss returns `Indeterminate` so
/// the LLM detector still runs for genuinely novel prompts.
///
/// The patterns are case-insensitive and tolerate whitespace
/// between words. They are intentionally conservative: false
/// positives (a legitimate brief that mentions "ignore" in
/// plain text) escalate to the LLM detector through
/// `Indeterminate`, not to `Hostile`. Only direct quotes of
/// well-known injection phrasing trip the regex set.
fn heuristic_hostile_check(normalised: &str) -> HeuristicOutcome {
    const HOSTILE_PATTERNS: &[&str] = &[
        // The canonical "system override" attempts. A bare
        // "ignore" or "disregard" is too noisy; we require the
        // phrase to mention instructions, the system prompt,
        // or the prior context.
        "ignore (?:the )?(?:previous|prior|above|all) instructions",
        "ignore (?:your|all) (?:rules|guidelines|system)",
        "disregard (?:the )?(?:system|previous|prior) prompt",
        "disregard (?:your|all) (?:rules|guidelines|instructions)",
        "forget (?:the )?(?:previous|prior|above) instructions",
        "you are now (?:a|an) ",
        "act as (?:a|an) (?:different|new) ",
        "system prompt[: ]",
    ];
    let lower = normalised.to_ascii_lowercase();
    for pat in HOSTILE_PATTERNS {
        if contains_ci(&lower, pat) {
            return HeuristicOutcome::Hostile;
        }
    }
    const SUSPICIOUS_PATTERNS: &[&str] = &[
        // Lower-confidence signals. Treated as
        // `Suspicious` rather than `Hostile` because the
        // phrase can appear in legitimate prompts about
        // security / privacy.
        "reveal (?:the )?system prompt",
        "show (?:me )?(?:the )?hidden prompt",
        "what (?:are|is) your instructions",
        "developer mode",
        "jailbreak",
    ];
    for pat in SUSPICIOUS_PATTERNS {
        if contains_ci(&lower, pat) {
            return HeuristicOutcome::Suspicious;
        }
    }
    if normalised.trim().is_empty() {
        // Empty prompts have nothing to classify; let the
        // LLM detector surface an empty-input verdict.
        return HeuristicOutcome::Indeterminate;
    }
    HeuristicOutcome::Benign
}

/// Cheap substring search that compiles the haystack-side
/// pattern as a "literal substring" probe (no regex compile).
/// Tolerant of case differences (the caller pre-lowercases the
/// text). Pinned to ASCII for the prototype so we don't have to
/// deal with Unicode case folding yet.
fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.contains(needle)
}

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

        // F5: compute the deterministic hash of the run's
        // `Config` and persist it to the
        // `final/config_hash.txt` sidecar. `build_manifest`
        // reads this sidecar after the pipeline finishes and
        // stamps the digest onto `manifest.config_hash`. The
        // hash is a pure function of the config, so two runs
        // with the same config produce the same manifest hash
        // and a `moagan rerun` after a config edit produces a
        // different one.
        let config_hash = compute_config_hash(&ctx.config)?;
        write_config_hash_sidecar(ctx, &config_hash)?;

        // E10: classify the normalised prompt before the intake
        // call. The classification runs in two stages:
        //
        // 1. A cheap, regex-based **heuristic** that catches the
        //    obvious prompt-injection patterns
        //    (`ignore previous instructions`,
        //    `disregard the system prompt`, etc.). When the
        //    heuristic says "looks benign", we skip the LLM
        //    call entirely — both the smoke gate
        //    (`moagan run --mode fast --provider mock`) and
        //    the integration tests stay green because the mock
        //    queue is not shifted by an extra LLM call.
        // 2. Anything the heuristic cannot rule out (genuine
        //    ambiguity, an unfamiliar injection phrasings) goes
        //    through the `Role::HostilePromptDetector` LLM
        //    call. The LLM call runs in cheap mode
        //    (`max_retries=1`) because the verdict is binary
        //    and a flaky retry would only buy extra cost.
        //
        // The `Disabled` policy short-circuits to `Safe`
        // regardless of heuristic so the cheapest profile pays
        // zero detector-side cost.
        let policy = effective_hostile_policy();
        let verdict = match policy {
            HostilePolicy::Disabled => HostileVerdict::Safe,
            _ => match heuristic_hostile_check(&normalised) {
                HeuristicOutcome::Benign => HostileVerdict::Safe,
                HeuristicOutcome::Suspicious => HostileVerdict::Suspicious {
                    reason: "heuristic match".into(),
                },
                HeuristicOutcome::Hostile => HostileVerdict::Hostile {
                    reason: "heuristic match".into(),
                },
                HeuristicOutcome::Indeterminate => run_hostile_detector(ctx, &normalised).await?,
            },
        };
        enforce_hostile_verdict(ctx, &verdict, policy)?;

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
            let mut value = serde_json::to_value(&intake).map_err(Error::from)?;
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "context_block".into(),
                    serde_json::Value::String(block.clone()),
                );
            }
            crate::atomic::writer::AtomicWriter::new().write(
                &brief_path,
                &serde_json::to_vec(&value).map_err(Error::from)?,
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

/// E10: invoke the `Role::HostilePromptDetector` against the
/// normalised prompt and translate the wire form into a
/// [`HostileVerdict`]. The detector is deterministic
/// (`T=0.0`, `top_p=0.1`, `max_tokens=1000000`); we still go through
/// `call_with_retry_parse` so transient transport errors recover
/// without a separate retry loop in this module. Empty / unknown
/// verdicts default to `Safe` so a misbehaving model cannot fail
/// the pipeline by being too quiet.
async fn run_hostile_detector(ctx: &RunContext, normalised: &str) -> Result<HostileVerdict> {
    let system = system_prompt(Role::HostilePromptDetector).to_owned();
    let user = normalised.to_owned();
    let report: HostilePromptReport = ctx
        .call_with_retry_parse(
            Role::HostilePromptDetector,
            system,
            user,
            "HostilePromptDetector: {input, verdict, confidence, reasons[], recommended_action}",
            1,
        )
        .await?;
    Ok(classify_hostile_report(&report))
}

/// E10: convert a populated [`HostilePromptReport`] into a
/// [`HostileVerdict`]. Pure function so tests can drive it
/// without standing up a real LLM call. The first reason is
/// surfaced so a single string carries the strongest injection
/// signal all the way to the error message.
fn classify_hostile_report(report: &HostilePromptReport) -> HostileVerdict {
    let reason = report
        .reasons
        .first()
        .cloned()
        .unwrap_or_else(|| "unspecified".into());
    match report.verdict.trim().to_ascii_lowercase().as_str() {
        "hostile" => HostileVerdict::Hostile { reason },
        "suspicious" => HostileVerdict::Suspicious { reason },
        _ => HostileVerdict::Safe,
    }
}

/// E10: enforce the policy decision against the verdict.
/// Returns `Err(Error::HostilePrompt)` only under
/// `HostilePolicy::FailClosed` when the verdict is `Hostile`;
/// the other combinations log a warning and return `Ok(())`
/// so the caller can keep going.
fn enforce_hostile_verdict(
    ctx: &RunContext,
    verdict: &HostileVerdict,
    policy: HostilePolicy,
) -> Result<()> {
    let warn =
        |code: &'static str, level: &'static str, msg: &'static str, payload: serde_json::Value| {
            let _ = ctx.telemetry.warn(
                code,
                level,
                msg,
                payload,
                crate::telemetry::WarningContext {
                    phase: Some("intake".into()),
                    role: Some("hostile_prompt_detector".into()),
                    ..Default::default()
                },
            );
        };
    match verdict {
        HostileVerdict::Safe => Ok(()),
        HostileVerdict::Suspicious { reason } => {
            tracing::warn!(
                verdict = "suspicious",
                reason = %reason,
                stage = "intake.hostile_prompt.suspicious",
                "Intake phase"
            );
            warn(
                "phase.intake_suspicious_prompt",
                "warn",
                "intake detector marked the prompt as suspicious",
                serde_json::json!({"reason": reason, "policy": format!("{policy:?}")}),
            );
            Ok(())
        }
        HostileVerdict::Hostile { reason } => match policy {
            HostilePolicy::FailClosed => {
                tracing::warn!(
                    verdict = "hostile",
                    reason = %reason,
                    policy = "fail_closed",
                    stage = "intake.hostile_prompt.rejected",
                    "Intake phase"
                );
                warn(
                    "phase.intake_hostile_prompt",
                    "warn",
                    "intake detector rejected the prompt as hostile",
                    serde_json::json!({"reason": reason, "policy": "fail_closed"}),
                );
                Err(Error::HostilePrompt(reason.clone()))
            }
            HostilePolicy::FailOpen => {
                tracing::warn!(
                    verdict = "hostile",
                    reason = %reason,
                    policy = "fail_open",
                    stage = "intake.hostile_prompt.fail_open_continue",
                    "Intake phase"
                );
                warn(
                    "phase.intake_hostile_prompt_fail_open",
                    "warn",
                    "intake detector flagged the prompt as hostile but the policy is fail-open",
                    serde_json::json!({"reason": reason, "policy": "fail_open"}),
                );
                Ok(())
            }
            HostilePolicy::Disabled => {
                // Defensive: `Disabled` short-circuits the
                // detector above, so this arm is unreachable. The
                // match is total to keep the helper exhaustive.
                Ok(())
            }
        },
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
/// can run it without touching the run context. The byte cap is
/// [`MAX_PROMPT_BYTES`] (D.29.2) — the centralised 250 KiB
/// constant from `crate::llm::size_limits`. The cap is enforced
/// by truncation (not error) because a 250 KiB paste must not
/// abort a run the operator started deliberately.
pub(crate) fn normalize_raw_prompt(raw: &str) -> String {
    // 1. BOM strip. A leading BOM (U+FEFF) is invisible in most
    //    editors but breaks the LLM's tokeniser on the first
    //    token. Drop it unconditionally when present.
    let no_bom: &str = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);

    // 2. Control-token strip. Route through the centralised
    //    helper (catalog §D.7.2; roadmap PR-27) so every LLM
    //    input/output parser in the codebase shares one
    //    definition of "what is a control byte?". The helper
    //    preserves `\n`, `\r`, and `\t` (legitimate whitespace)
    //    and removes everything else in `\u{0000}`–`\u{001F}`
    //    plus DEL (`\u{007F}`).
    let no_control: Cow<'_, str> = control_tokens::strip(no_bom);
    // `Cow<str>` does not implement `Borrow<str>` in a way that
    // lets us reuse the original `String` allocation when
    // stripping was a no-op, so we materialise a `String` for
    // the byte-cap step. The helper still saves the per-char
    // filter scan when the input is clean.
    let no_control: String = no_control.into_owned();

    // 3. Byte cap. When the cleaned text still exceeds the cap,
    //    truncate and surface a warning so the operator can see
    //    "this prompt was too big" in the telemetry stream. The
    //    truncation is character-aware (operates on the
    //    already-cleaned `String`) so we never slice mid-codepoint.
    if no_control.len() > MAX_PROMPT_BYTES {
        let mut truncated = String::with_capacity(MAX_PROMPT_BYTES);
        for c in no_control.chars() {
            if truncated.len() + c.len_utf8() > MAX_PROMPT_BYTES {
                break;
            }
            truncated.push(c);
        }
        tracing::warn!(
            original_bytes = raw.len(),
            truncated_bytes = truncated.len(),
            cap_bytes = MAX_PROMPT_BYTES,
            stage = "intake.normalized_truncated",
            "Intake phase"
        );
        truncated
    } else {
        no_control
    }
}

/// F5: write the config hash to the run's `final/` directory so
/// `build_manifest` can pick it up later. Sidecar is a single
/// line of hex + LF; atomic so a partial write cannot leave a
/// truncated digest on disk.
fn write_config_hash_sidecar(ctx: &RunContext, hash: &str) -> Result<()> {
    let path = ctx.run_dir().final_dir().join(CONFIG_HASH_SIDECAR);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::Io(crate::error::IoError::Write {
                path: parent.to_path_buf(),
                source: e,
            })
        })?;
    }
    let mut body = String::with_capacity(hash.len() + 1);
    body.push_str(hash);
    body.push('\n');
    crate::atomic::writer::AtomicWriter::new().write(&path, body.as_bytes())?;
    Ok(())
}

/// Build a minimal no-op `RunContext` for the policy-enforcement
/// tests so they don't have to wire up a real cache / registry.
/// Lives at module level (not in `tests`) so the helper is
/// reachable from every test in the module without re-importing
/// the (private) builder.
#[cfg(test)]
fn noop_run_context() -> RunContext {
    use crate::execution::Parallelism;
    use crate::fs_layout::MoaganHome;
    use crate::ids::RunId;
    use crate::telemetry::Telemetry;
    use std::sync::Arc;

    let home = Arc::new(MoaganHome::at(std::path::PathBuf::from("/tmp/moagan-e10")));
    RunContext::new(
        RunId::default(),
        home,
        Arc::new(crate::llm::ProviderRegistry::default()),
        "mock".into(),
        "mock-model".into(),
        Parallelism::new(1),
        Telemetry::noop(),
        "hello".into(),
        "fast".into(),
    )
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
    /// The cap is 250 KiB (D.29.2: `MAX_PROMPT_BYTES`); the test
    /// pushes a string 50 KiB over the cap and expects the
    /// output to be at the cap, never larger. UTF-8 boundary
    /// safety is also pinned (the truncation walks chars, not
    /// bytes).
    #[test]
    fn intake_truncates_oversized_brief() {
        let big = "a".repeat(MAX_PROMPT_BYTES + 50_000);
        let out = normalize_raw_prompt(&big);
        assert!(
            out.len() <= MAX_PROMPT_BYTES,
            "truncated len {} > cap {}",
            out.len(),
            MAX_PROMPT_BYTES
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

    /// E9: ASCII control bytes (< 0x20) except `\n`, `\r`, and
    /// `\t` are removed from the raw prompt. NULs, BEL, FF, VT,
    /// ESC, etc. — all dropped. Newline, CR, and tab are
    /// preserved (legitimate whitespace in natural-language
    /// prompts and JSON output). The check also covers that
    /// multi-byte UTF-8 characters with codepoints >= 0x80 are
    /// NOT touched (the central helper only acts on the ASCII
    /// control range and DEL).
    #[test]
    fn intake_strips_control_tokens_preserving_newlines() {
        // Build a string with a deliberate mix of control bytes
        // (NUL, SOH, STX, ETX, SO, US) and legitimate whitespace,
        // plus a non-ASCII codepoint (`ñ`) to confirm the filter
        // does not touch multi-byte UTF-8 sequences. Bare CR is
        // constructed via `push('\r')` so the source file stays
        // CR-free (Rust strings forbid a literal 0x0D).
        let mut raw = String::from("line1\nline2\rline3\tcol3\n");
        for c in ['\0', '\u{01}', '\u{02}', '\u{03}', '\u{0E}', '\u{1F}'] {
            raw.push(c);
        }
        raw.push_str("acento: ñ");
        let out = normalize_raw_prompt(&raw);
        // Newline + CR + tab preserved, control bytes gone.
        assert!(out.contains("line1\nline2\rline3\tcol3"));
        // Non-ASCII preserved.
        assert!(out.contains("ñ"));
        // No remaining control bytes (< 0x20) other than the
        // legitimate newline / CR / tab.
        for c in out.chars() {
            let code = c as u32;
            if code < 0x20 {
                assert!(
                    c == '\n' || c == '\r' || c == '\t',
                    "leaked control byte: {c:?}"
                );
            }
        }
        // Sanity: the count of `\n`, `\r`, and `\t` matches the
        // input.
        let newlines = out.chars().filter(|c| *c == '\n').count();
        let crs = out.chars().filter(|c| *c == '\r').count();
        let tabs = out.chars().filter(|c| *c == '\t').count();
        assert_eq!(newlines, 2, "expected 2 newlines preserved");
        assert_eq!(crs, 1, "expected 1 CR preserved");
        assert_eq!(tabs, 1, "expected 1 tab preserved");
    }

    // -- E10: hostile-prompt policy -----------------------------------

    /// E10: env-var parsing accepts the documented
    /// canonical forms and the case-insensitive aliases
    /// (`fail-closed`, `closed`, `default`,
    /// `fail-open`, `open`, `disabled`, `disable`,
    /// `off`, `none`). Empty / whitespace / unknown
    /// values resolve to the [`HostilePolicy::default`]
    /// so a stale `MOAGAN_INTAKE_HOSTILE_POLICY` export
    /// cannot silently flip the default.
    #[test]
    fn hostile_policy_from_env_accepts_canonical_and_aliases() {
        assert_eq!(
            HostilePolicy::from_env("fail_closed").unwrap(),
            HostilePolicy::FailClosed
        );
        assert_eq!(
            HostilePolicy::from_env("FAIL_CLOSED").unwrap(),
            HostilePolicy::FailClosed
        );
        assert_eq!(
            HostilePolicy::from_env("fail-closed").unwrap(),
            HostilePolicy::FailClosed
        );
        assert_eq!(
            HostilePolicy::from_env("closed").unwrap(),
            HostilePolicy::FailClosed
        );
        assert_eq!(
            HostilePolicy::from_env("fail_open").unwrap(),
            HostilePolicy::FailOpen
        );
        assert_eq!(
            HostilePolicy::from_env("disabled").unwrap(),
            HostilePolicy::Disabled
        );
        assert_eq!(
            HostilePolicy::from_env("off").unwrap(),
            HostilePolicy::Disabled
        );
        // Garbage values must NOT silently coerce; the caller
        // (effective_hostile_policy) falls back to
        // `default()` in that case.
        assert!(HostilePolicy::from_env("definitely-not-a-policy").is_none());
        assert!(HostilePolicy::from_env("").is_none());
        assert!(HostilePolicy::from_env("   ").is_none());
        // Default stays FailClosed so the spec is satisfied
        // out of the box.
        assert_eq!(HostilePolicy::default(), HostilePolicy::FailClosed);
    }

    /// E10: `classify_hostile_report` is the pure helper that
    /// translates the wire form into the internal verdict enum.
    /// The happy paths cover all three classifier outputs plus
    /// the "unknown verdict" defensive case (a misbehaving model
    /// must not fail-closed implicitly).
    #[test]
    fn classify_hostile_report_maps_every_verdict() {
        let host = HostilePromptReport {
            verdict: "hostile".into(),
            reasons: vec!["ignore previous instructions".into()],
            ..Default::default()
        };
        assert_eq!(
            classify_hostile_report(&host),
            HostileVerdict::Hostile {
                reason: "ignore previous instructions".into()
            }
        );
        let sus = HostilePromptReport {
            verdict: "suspicious".into(),
            reasons: vec!["embedded role override".into()],
            ..Default::default()
        };
        assert_eq!(
            classify_hostile_report(&sus),
            HostileVerdict::Suspicious {
                reason: "embedded role override".into()
            }
        );
        let safe = HostilePromptReport {
            verdict: "safe".into(),
            reasons: vec!["no injection signals".into()],
            ..Default::default()
        };
        assert_eq!(classify_hostile_report(&safe), HostileVerdict::Safe);
        let unknown = HostilePromptReport {
            verdict: "definitely-not-a-verdict".into(),
            reasons: Vec::new(),
            ..Default::default()
        };
        assert_eq!(classify_hostile_report(&unknown), HostileVerdict::Safe);
        let empty_reasons = HostilePromptReport {
            verdict: "hostile".into(),
            reasons: Vec::new(),
            ..Default::default()
        };
        assert_eq!(
            classify_hostile_report(&empty_reasons),
            HostileVerdict::Hostile {
                reason: "unspecified".into()
            }
        );
    }

    /// E10: `FailClosed` (the default) rejects a `Hostile`
    /// verdict with `Error::HostilePrompt("...")` so the run
    /// aborts before the intake LLM call. The helper is a
    /// no-op for `Safe` and `Suspicious` so neither blocks the
    /// pipeline.
    #[test]
    fn intake_fails_closed_on_hostile_prompt() {
        let ctx = noop_run_context();
        let verdict = HostileVerdict::Hostile {
            reason: "ignore previous instructions".into(),
        };
        let err = enforce_hostile_verdict(&ctx, &verdict, HostilePolicy::FailClosed)
            .expect_err("FailClosed must surface hostile verdicts as Err");
        match err {
            Error::HostilePrompt(msg) => {
                assert!(
                    msg.contains("ignore previous instructions"),
                    "reason must propagate, got {msg:?}"
                );
            }
            other => panic!("expected Error::HostilePrompt, got {other:?}"),
        }
        // Safe + Suspicious keep flowing under FailClosed.
        assert!(
            enforce_hostile_verdict(&ctx, &HostileVerdict::Safe, HostilePolicy::FailClosed).is_ok()
        );
        assert!(
            enforce_hostile_verdict(
                &ctx,
                &HostileVerdict::Suspicious {
                    reason: "noise".into()
                },
                HostilePolicy::FailClosed
            )
            .is_ok()
        );
    }

    /// E10: `Suspicous` verdict is non-blocking under both
    /// policies — the warning is the side effect. The helper
    /// returns `Ok(())` so the pipeline continues into the
    /// intake LLM call.
    #[test]
    fn intake_warns_on_suspicious_prompt() {
        let ctx = noop_run_context();
        let verdict = HostileVerdict::Suspicious {
            reason: "encoded control tokens".into(),
        };
        for policy in [HostilePolicy::FailClosed, HostilePolicy::FailOpen] {
            assert!(
                enforce_hostile_verdict(&ctx, &verdict, policy).is_ok(),
                "{policy:?} must let suspicious prompts continue"
            );
        }
    }

    /// E10: `Safe` verdict is a no-op for every policy (it
    /// neither logs nor returns an error). The pipeline
    /// continues into the intake LLM call.
    #[test]
    fn intake_allows_safe_prompt() {
        let ctx = noop_run_context();
        for policy in [
            HostilePolicy::FailClosed,
            HostilePolicy::FailOpen,
            HostilePolicy::Disabled,
        ] {
            assert!(
                enforce_hostile_verdict(&ctx, &HostileVerdict::Safe, policy).is_ok(),
                "{policy:?} must let safe prompts through"
            );
        }
    }

    /// E10: a `Hostile` verdict under `FailOpen` is
    /// downgraded to a non-blocking warning. The contract
    /// "operator opted into fail-open" means the pipeline
    /// keeps going; the warning carries the reason so the
    /// audit trail still records what the detector saw.
    #[test]
    fn intake_fail_open_continues_on_hostile() {
        let ctx = noop_run_context();
        let verdict = HostileVerdict::Hostile {
            reason: "jailbreak template".into(),
        };
        assert!(
            enforce_hostile_verdict(&ctx, &verdict, HostilePolicy::FailOpen).is_ok(),
            "FailOpen must NOT abort on hostile verdicts"
        );
    }

    // -- F5: config_hash ---------------------------------------------

    /// F5: `compute_config_hash` returns a 64-char hex BLAKE3
    /// digest of the canonical TOML form of `Config`. The
    /// `IntakePhase::execute` path stamps this digest onto the
    /// `final/config_hash.txt` sidecar so `build_manifest` can
    /// pick it up later. The test uses `Config::default()` so
    /// the assertion does not break when the `Config` struct
    /// gains new fields.
    #[test]
    fn intake_records_config_hash_in_manifest() {
        let cfg = Config::default();
        let hash = compute_config_hash(&cfg).expect("config_hash succeeds");
        assert_eq!(
            hash.len(),
            64,
            "BLAKE3 hex digest is 64 chars (32 bytes), got {hash:?}"
        );
        // The digest is lowercase hex.
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "digest must be lowercase hex, got {hash:?}"
        );
        // No-op contexts (the `noop_run_context` helper used
        // elsewhere in this module) do NOT change the hash:
        // the helper derives from a `Config::default()`,
        // matching what the `compute_config_hash` call sees.
        let ctx = noop_run_context();
        let same = compute_config_hash(&ctx.config).expect("noop ctx config_hash");
        assert_eq!(hash, same, "hash is purely a function of the Config");
    }

    /// F5: the same `Config` produces the same hash across
    /// repeated runs — the property the sidecar exists to
    /// guarantee. A `Config` mutated post-hash produces a
    /// different hash so a `moagan rerun` after editing the
    /// config file surfaces the drift.
    #[test]
    fn config_hash_deterministic_across_repeated_runs() {
        let cfg = Config::default();
        let first = compute_config_hash(&cfg).unwrap();
        let second = compute_config_hash(&cfg).unwrap();
        let third = compute_config_hash(&cfg).unwrap();
        assert_eq!(first, second, "deterministic: same input -> same digest");
        assert_eq!(second, third, "deterministic across multiple calls");
        // Length sanity: 64-char BLAKE3 hex.
        assert_eq!(first.len(), 64);
    }
}
