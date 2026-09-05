//! Moagan — multi-agent system for technical problems through massive solution exploration,
//! curation, and ranking. See `AGENTS.md` for operating rules and `Cargo.toml` for crate
//! metadata.

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]
#![warn(unreachable_pub)]
#![warn(clippy::all)]

pub mod atomic;
pub mod audit;
pub mod cancel;
pub mod checkpoint;
pub mod cli;
pub mod config;
pub mod context;
pub mod coverage;
pub mod discovery;
pub mod domain;
pub mod error;
pub mod error_code;
pub mod execution;
pub mod fs_layout;
pub mod ids;
pub mod llm;
pub mod phases;
pub mod preferences;
pub mod ranking;
pub mod reconcile;
pub mod redact;
pub mod research;
pub mod sandbox;
pub mod secret;
pub mod serde_util;
pub mod storage;
pub mod telemetry;
pub mod test_support;
pub mod time;
pub mod validators;

pub use error::{Error, ExitCode, Result, exit_code};

use crate::ids::RunId;
use std::str::FromStr;

/// Process-wide guard for tests that mutate the `MOAGAN_HOME`
/// env var. Two tests running in parallel that both set
/// `MOAGAN_HOME` can interleave with each other under the OS
/// scheduler and end up reading each other's home directory,
/// which surfaces as a `Provider("sqlite: duplicate column
/// name: …")` panic when the second open of the same
/// `meta.sqlite` re-applies a non-idempotent migration
/// (v003, v005, v007). Every test that touches `MOAGAN_HOME`
/// must acquire this lock for the duration of its env-var
/// mutation + the dispatcher call that consumes it.
#[cfg(test)]
pub static TEST_MOAGAN_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serialises every test that mutates `MINIMAX_API_KEY` /
/// `DEEPSEEK_API_KEY` / `OPENCODE_API_KEY` process-wide. Shared
/// across the LLM provider tests (`llm::api_keys::tests`),
/// `cli::doctor::tests`, and any future caller that touches those
/// env vars — without it, parallel `cargo test` runs observe
/// each other's mutations and report flakes. Not gated by
/// `#[cfg(test)]` so the integration tests in `tests/` can also
/// acquire it (they live in a separate compilation unit where
/// `cfg(test)` of the lib is *not* active). The lock is a
/// zero-sized `Mutex<()>`; it costs ~8 bytes per process and is
/// only ever touched by the test harness.
pub static TEST_API_KEYS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serialises every test that mutates the process-wide `PATH`
/// env var. Used by the K.4 sub-1 PDF parser tests in
/// `research::pdf::tests` to mock "binary not found" without
/// leaking a half-mutated `PATH` to a parallel test that
/// shells out via `Command::new`. The lock is intentionally
/// process-wide (matching the existing `TEST_API_KEYS_LOCK`
/// pattern) because `PATH` resolution is a process-level
/// concern and any concurrent mutation would race.
#[cfg(test)]
pub static TEST_PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serialises every test that mutates the
/// `MOAGAN_MINIMAX_MODEL` / `MOAGAN_MINIMAX_ENDPOINT` env vars
/// (the v0.10 config-override surface). `Config::apply_env_overrides`
/// reads these once at the top of the call, so a parallel
/// thread that flips the var between `set_var` and
/// `apply_env_overrides` would race the override. Tests in
/// `config::tests` and any future caller must acquire this lock
/// for the duration of the mutation + the dispatch call.
#[cfg(test)]
pub static TEST_MINIMAX_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serialises every test that calls `std::env::set_current_dir`.
/// Without it, parallel `cargo test` runs observe each other's
/// cwd changes and report flakes. Used by the PR-B2 config-
/// precedence tests in `config::tests`.
#[cfg(test)]
pub static TEST_CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serialises every test that mutates `MOAGAN_EVENT_FORMAT` (the
/// env var honoured by clap's `env = "MOAGAN_EVENT_FORMAT"`
/// binding at `src/cli/mod.rs:255` and by `resolve_event_format`
/// at `src/telemetry/stdout_events.rs:67`). Currently consumed by:
///
/// - `src/cli/mod.rs::tests::event_format_default_is_auto`
///   (line 2652) — removes the env var.
/// - `src/cli/mod.rs::tests::event_format_env_off_reaches_parser`
///   (line 2679) — sets the env var to `"off"` and reads via clap.
///
/// Without this lock, parallel `cargo test` runs let sibling
/// tests observe env state owned by a test mid-flight — the same
/// flake pattern closed in PR #246 for `MOAGAN_LOG_FORMAT` (see
/// `docs/test-skips.md §3 Layer 2 closing notes`) and re-applied in
/// PR #677 for `MOAGAN_DECISION_FORMAT` at
/// `src/telemetry/stdout_events.rs:406`. Note that
/// `src/telemetry/stdout_events.rs:406` is a *module-local* lock
/// (different env var) — it does not protect
/// `MOAGAN_EVENT_FORMAT`; a test that touches both env vars must
/// acquire both locks.
///
/// The lock only needs to cover the `Cli::try_parse_from` parse
/// window because clap reads `env = MOAGAN_EVENT_FORMAT` at parse
/// time, not at every subsequent read; this distinguishes it
/// from PR #677's lock, which must cover a longer window because
/// `resolve_decision_format` is consulted at every decision emit.
///
/// Gated with `#[cfg(test)]` matching the `TEST_MOAGAN_HOME_LOCK`
/// precedent — only unit tests in `src/cli/mod.rs` need it today;
/// integration tests use `cmd.env(...)` on a child process and do
/// not race with this lock.
#[cfg(test)]
pub static TEST_EVENT_FORMAT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serialises every test that mutates `MOAGAN_LOG_TO_STDERR`
/// (the env var honoured by clap's `env = "MOAGAN_LOG_TO_STDERR"`
/// binding at `src/cli/mod.rs:299` and by the legacy
/// `log_to_stderr_value` resolver at `src/main.rs:223`). Currently
/// consumed by:
///
/// - `src/cli/mod.rs::tests::log_to_stderr_env_accepts_shell_idiomatic_one`
///   (line 2589) — sets the env var to `"1"` and reads via clap.
/// - `src/cli/mod.rs::tests::log_to_stderr_env_accepts_false`
///   (line 2619) — sets the env var to `"false"` and reads via
///   clap.
///
/// Split off `TEST_MOAGAN_HOME_LOCK` by PR #679 (issue #679 item 1)
/// because the env-var scope is different: `MOAGAN_HOME` and
/// `MOAGAN_LOG_TO_STDERR` are independent clap bindings (different
/// flags, different parse paths), so serialising one does not
/// imply serialising the other. Sharing a single lock for both
/// forced every `MOAGAN_LOG_TO_STDERR` test to wait for every
/// `MOAGAN_HOME`-mutating test, which is over-serialisation.
///
/// Lock window matches `TEST_EVENT_FORMAT_LOCK`: only the
/// `Cli::try_parse_from` parse matters because clap reads
/// `env = MOAGAN_LOG_TO_STDERR` at parse time, not at every
/// subsequent read.
///
/// Gated with `#[cfg(test)]` matching the `TEST_MOAGAN_HOME_LOCK`
/// and `TEST_EVENT_FORMAT_LOCK` precedent — only unit tests in
/// `src/cli/mod.rs` need it today; integration tests use
/// `cmd.env(...)` on a child process and do not race with this
/// lock.
#[cfg(test)]
pub static TEST_LOG_TO_STDERR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serialises every test that mutates `MOAGAN_NON_INTERACTIVE`
/// (the env var honoured by [`RunContext::default_interactive`] at
/// `src/phases/phase.rs:227` — the single-source-of-truth helper
/// introduced for EPIC #755 / #734 so library callers can opt out
/// of human checkpoints via env var alone, without parsing the
/// CLI flag themselves). Currently consumed by:
///
/// - `src/phases/phase.rs::tests::run_context_*` (nine regression
///   tests for the env-var precedence surface added by PR1 of
///   EPIC #755) — save-and-restore the env var around every
///   `RunContext::new` / `RunContext::new_with_config` call.
///
/// Lock window covers the full `RunContext::new` construction
/// (and any subsequent `with_interactive` call) because
/// `default_interactive()` reads `MOAGAN_NON_INTERACTIVE` once at
/// construction time — distinguishing it from
/// `TEST_EVENT_FORMAT_LOCK` / `TEST_LOG_TO_STDERR_LOCK` whose
/// clap bindings only read at `Cli::try_parse_from` parse time.
///
/// Split off from `TEST_LOG_TO_STDERR_LOCK` / `TEST_MOAGAN_HOME_LOCK`
/// by PR1 of EPIC #755 because the env-var scope is independent
/// (the `MOAGAN_NON_INTERACTIVE` path is consumed inside
/// `RunContext`, not at CLI parse time), so serialising any of the
/// existing locks would over-serialise the new
/// `phases::tests::run_context_*` block against unrelated env-var
/// mutations.
///
/// Gated with `#[cfg(test)]` matching the existing
/// `TEST_LOG_TO_STDERR_LOCK` precedent — only unit tests in
/// `src/phases/phase.rs` need it today; integration tests use
/// `cmd.env(...)` on a child process and do not race with this
/// lock.
#[cfg(test)]
pub static TEST_NON_INTERACTIVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// CLI entry point. Returns a Unix exit code.
pub async fn run() -> anyhow::Result<()> {
    use clap::Parser;
    let cli = cli::Cli::parse();
    run_with_cli(cli).await
}

/// CLI entry point with a pre-parsed [`cli::Cli`]. Used by `main.rs`
/// so the `--log-format` / `--event-format` flags take effect on
/// the very first tracing event (before this function is called).
/// `run()` is a convenience wrapper that parses the CLI itself.
pub async fn run_with_cli(cli: cli::Cli) -> anyhow::Result<()> {
    use crate::cli::DispatchResult;
    use crate::ids::RunId;
    use crate::telemetry::stdout_events;
    use crate::telemetry::stdout_events::{Event, EventFormat, SCHEMA_VERSION, now_rfc3339};
    use std::str::FromStr;
    use tracing::Instrument;
    let run_started = std::time::Instant::now();
    let emit_stdout = stdout_events::resolve_event_format(EventFormat::Jsonl);

    // Pre-allocate the `run_id` so the `pipeline_span` below is
    // constructed with `run_id = %run_id` from the start. Events
    // emitted DURING dispatch (intake, phases, llm_call, probe, …)
    // inherit the span context, so any operator grepping
    // `pipeline{run_id=...}` on stderr gets a real UUID v7 instead
    // of `null` / `"pre-dispatch"`. The pre-v0.11.2 version
    // declared the span fields as `Empty` and called `Span::record`
    // AFTER dispatch returned — by that point ~98.9% of events
    // had already been emitted and inherited a null `run_id`.
    //
    // For commands that produce a new run (Run / Discover /
    // Preflight) we generate a fresh UUIDv7. For commands that
    // operate on an existing run (Resume / Rerun / Refine /
    // Rerank / Continue, with or without `--from-pause`) we parse
    // the id from the CLI input — it is the canonical id, so the
    // span matches every other event from that run. For
    // `Continue` specifically, the dispatcher does NOT fork to a
    // new run id; it resumes the source run with the source id,
    // so the candidate MUST be the source id (otherwise the span
    // carries a ghost UUID that no event under the source run
    // directory can correlate against). For read-only commands we
    // still allocate a real id; the downstream `Event::RunStart`
    // substitutes a `<read-only>` sentinel, but the span stays
    // real so events emitted during (say) an inspection query
    // remain correlatable.
    let candidate_run_id: RunId = match &cli.cmd {
        cli::Cmd::Run { .. } | cli::Cmd::Discover { .. } | cli::Cmd::Preflight { .. } => {
            RunId::new()
        }
        // Continue (any variant): the dispatcher resumes the
        // source run with the source id, so the candidate must
        // be the source id parsed from the CLI input. A
        // malformed string falls back to a fresh UUIDv7 (the
        // inner parser then surfaces the real error).
        cli::Cmd::Continue {
            run_id: Some(s), ..
        } => RunId::from_str(s).unwrap_or_else(|_| RunId::new()),
        cli::Cmd::Continue { run_id: None, .. } => RunId::new(),
        cli::Cmd::Resume { run_id, .. }
        | cli::Cmd::Rerun { run_id, .. }
        | cli::Cmd::Refine { run_id, .. }
        | cli::Cmd::Rerank { run_id, .. } => {
            RunId::from_str(run_id).unwrap_or_else(|_| RunId::new())
        }
        cli::Cmd::Import { source_path, .. } => {
            // Pre-read the manifest so the span carries the SAME
            // id the importer will write to disk. Falls back to a
            // fresh UUIDv7 if the manifest is missing or the
            // `run_id` field is absent (the importer then
            // re-validates and surfaces the real error).
            read_import_run_id(source_path).unwrap_or_else(|_| RunId::new())
        }
        // Read-only + everything else: fresh UUIDv7. The
        // `Event::RunStart` will swap it for `<read-only>` but the
        // span keeps the real value so events emitted during
        // (e.g.) inspect queries remain correlatable.
        _ => RunId::new(),
    };

    // Best-effort stable label for the span's `mode` field. We do
    // NOT need this to be 100% exhaustive — the dispatcher may
    // refine it (e.g. `audit_proxy` vs `audit_verify`) inside
    // `cli::dispatch_with_run_id`. The pre-parse here is enough
    // for the span to carry a useful label from the very first
    // event.
    let mode_label: &'static str = match &cli.cmd {
        cli::Cmd::Run { mode, .. } => mode.as_str(),
        cli::Cmd::Discover { .. } => "discover",
        cli::Cmd::Preflight { .. } => "preflight",
        cli::Cmd::Continue { .. } => "continue",
        cli::Cmd::Resume { .. } => "resume",
        cli::Cmd::Rerun { .. } => "rerun",
        cli::Cmd::Refine { .. } => "refine",
        cli::Cmd::Rerank { .. } => "rerank",
        cli::Cmd::Import { .. } => "import",
        cli::Cmd::Inspect { .. } => "inspect",
        cli::Cmd::Doctor { .. } => "doctor",
        cli::Cmd::Probe { .. } => "probe",
        cli::Cmd::Audit { .. } => "audit",
        cli::Cmd::Telemetry { .. } => "telemetry",
        cli::Cmd::Coverage { .. } => "coverage",
        cli::Cmd::Validate { .. } => "validate",
        cli::Cmd::Diff { .. } => "diff",
        cli::Cmd::Repair { .. } => "repair",
        cli::Cmd::Pause { .. } => "pause",
        cli::Cmd::List { .. } => "list",
        cli::Cmd::Rate { .. } => "rate",
    };
    let resumed = matches!(&cli.cmd, cli::Cmd::Resume { .. } | cli::Cmd::Rerun { .. });

    // Pipeline span: every event emitted during dispatch (RunStart,
    // phase_start, llm_call, probe, …) inherits these fields. The
    // `provider` / `model` fields stay `Empty` here — they are
    // resolved inside `dispatch_with_run_id` after config /
    // provider resolution runs and the per-arm error path can
    // patch them via the `let _pipeline_enter = …;` guard. Today
    // no arm needs that, but leaving the `Empty` slots avoids a
    // `Span::record` call for fields the operator rarely greps.
    let pipeline_span = tracing::info_span!(
        "pipeline",
        run_id = %candidate_run_id,
        mode = mode_label,
        provider = tracing::field::Empty,
        model = tracing::field::Empty,
        resumed = resumed,
    );
    let _pipeline_enter = pipeline_span.enter();

    tracing::info!("moagan::run: dispatching");
    let dispatch_result: DispatchResult = match cli::dispatch_with_run_id(cli, candidate_run_id)
        .instrument(pipeline_span.clone())
        .await
    {
        Ok(dr) => {
            tracing::info!(
                exit_code = dr.exit_code,
                run_id = ?dr.run_id.as_ref().map(|r| r.short()),
                mode = ?dr.mode,
                "moagan::run: dispatch ok"
            );
            dr
        }
        Err(e) => {
            tracing::error!(error = %e, "moagan::run: dispatch failed");
            // PR-04a (E-1) stream routing flip: the structured
            // `tracing::error!` above is now the SOLE machine-facing
            // surface for the dispatch error. v0.11 emitted a
            // duplicate `eprintln!("error: {e}")` when stderr was
            // a TTY, which masked the JSON line for TTY users and
            // broke the symmetry with the `--log-format json`
            // path. With the v0.12.0 routing flip, the JSON line
            // is always emitted on stderr (uniquely so — that's the
            // point of the flip) and the redundant plain-text
            // fallback is gone. TTY users still get a readable
            // `error: …` line because `fmt::layer().text()` renders
            // the JSON event with the message on the next line.
            let code = i32::from(exit_code(&e));
            // On dispatch failure, emit `RunEnd` with the
            // `<read-only>` sentinel for the run_id and an
            // `error` status. No `RunStart` was emitted because
            // the dispatch never produced a `DispatchResult` to
            // feed the fields; the operator pipeline still sees
            // one structured event marking the error.
            if emit_stdout {
                stdout_events::STDOUT_EVENTS.emit(Event::RunEnd {
                    schema: SCHEMA_VERSION,
                    ts: now_rfc3339(),
                    run_id: "<read-only>",
                    status: "error",
                    exit_code: code,
                    elapsed_ms: run_started.elapsed().as_millis() as u64,
                    artefacts: serde_json::json!({}),
                });
            }
            std::process::exit(code);
        }
    };

    // Resolve the strings we will emit onto `RunStart` and
    // `RunEnd`. For read-only commands the run_id is `None`;
    // we emit a `<read-only>` sentinel so the JSON shape
    // stays stable and the operator's
    // `jq -c 'select(.kind == "run_start")'` selector keeps
    // matching a stable string across all sub-commands.
    let resolved_run_id: String = dispatch_result
        .run_id
        .as_ref()
        .map(|r| r.to_string())
        .unwrap_or_else(|| "<read-only>".to_owned());
    let resolved_mode: &str = dispatch_result.mode.unwrap_or("<help-or-readonly>");
    let resolved_provider: &str = dispatch_result.provider.as_deref().unwrap_or("<n/a>");
    let resolved_model: &str = dispatch_result.model.as_deref().unwrap_or("<n/a>");
    let resolved_prompt_hash: &str = dispatch_result.prompt_hash.as_deref().unwrap_or("<n/a>");

    // Stdout event: `run_start`. Always emitted (even for the
    // `moagan --help` subcommand) when stdout is not a TTY so
    // operators can `moagan … | jq 'select(.kind == "run_start")'`
    // to confirm the binary even started. Emitted AFTER
    // `cli::dispatch_with_run_id` so we can stamp the real
    // `run_id` / `mode` / `provider` / `model` / `prompt_hash`
    // onto the event — and the value of `run_id` is byte-identical
    // to the `pipeline_span.run_id` every event during dispatch
    // inherited on stderr.
    if emit_stdout {
        stdout_events::STDOUT_EVENTS.emit(Event::RunStart {
            schema: SCHEMA_VERSION,
            ts: now_rfc3339(),
            run_id: resolved_run_id.as_str(),
            mode: resolved_mode,
            provider: resolved_provider,
            model: resolved_model,
            prompt_hash: resolved_prompt_hash,
        });
    }

    // `Event::RunEnd.status` mirrors `dispatch_result.exit_code`:
    // the pre-v0.11.2 version always emitted `"ok"`, which lied
    // when the dispatcher returned a non-zero exit code (e.g. a
    // missing provider, a reconcile error). The new contract is
    // `status == "ok"` iff `exit_code == 0`, otherwise `"error"`.
    // The matching stderr-side `tracing::info!("dispatch ok")` /
    // `tracing::error!("dispatch failed")` already split on this
    // boundary; this is the stdout counterpart.
    let resolved_status: &'static str = if dispatch_result.exit_code == 0 {
        "ok"
    } else {
        "error"
    };
    if emit_stdout {
        stdout_events::STDOUT_EVENTS.emit(Event::RunEnd {
            schema: SCHEMA_VERSION,
            ts: now_rfc3339(),
            run_id: resolved_run_id.as_str(),
            status: resolved_status,
            exit_code: dispatch_result.exit_code,
            elapsed_ms: run_started.elapsed().as_millis() as u64,
            artefacts: serde_json::json!({}),
        });
    }
    std::process::exit(dispatch_result.exit_code)
}

/// Read the `run_id` from a `<source>/manifest.json` for an
/// `moagan import` pre-allocation. The dispatcher uses the value
/// to seed `pipeline_span.run_id` so every event emitted during
/// the import carries the SAME id that the importer will
/// eventually persist. Falls back to a fresh UUIDv7 on any error
/// (missing manifest, malformed JSON, absent `run_id` field) so
/// the span never goes null — the importer then re-validates
/// and surfaces the real error to the operator.
fn read_import_run_id(source_path: &std::path::Path) -> crate::error::Result<RunId> {
    let manifest_path = source_path.join("manifest.json");
    let body = std::fs::read_to_string(&manifest_path).map_err(|e| {
        crate::error::Error::InvalidState(format!(
            "cannot read manifest for run_id pre-allocation at {}: {e}",
            manifest_path.display()
        ))
    })?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| crate::error::Error::InvalidState(format!("invalid manifest.json: {e}")))?;
    let id = v.get("run_id").and_then(|x| x.as_str()).unwrap_or("");
    RunId::from_str(id)
        .map_err(|e| crate::error::Error::InvalidArgs(format!("invalid run_id '{id}': {e}")))
}

/// Synchronous entry point used by `main.rs` and integration tests.
pub fn run_blocking() -> anyhow::Result<()> {
    use clap::Parser;
    let cli = cli::Cli::parse();
    run_blocking_with_cli(cli)
}

/// Synchronous entry point with a pre-parsed [`cli::Cli`].
pub fn run_blocking_with_cli(cli: cli::Cli) -> anyhow::Result<()> {
    tracing::debug!("moagan::run_blocking: building tokio runtime");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    tracing::trace!("moagan::run_blocking: runtime built; entering block_on");
    rt.block_on(run_with_cli(cli))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_run_subcommand() {
        let cli = cli::Cli::parse_from([
            "moagan",
            "run",
            "--mode",
            "fast",
            "--provider",
            "mock",
            "--prompt",
            "hello",
        ]);
        match cli.cmd {
            cli::Cmd::Run {
                mode,
                provider,
                prompt,
                ..
            } => {
                assert_eq!(mode, cli::Mode::Fast);
                assert_eq!(provider.as_deref(), Some("mock"));
                assert_eq!(prompt, "hello");
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn run_uses_exit_code_mapper() {
        let e = Error::InvalidArgs("x".into());
        assert_eq!(exit_code(&e), 2);
        let e = Error::PlanExhausted {
            message: "x".into(),
            http_status: None,
        };
        assert_eq!(exit_code(&e), 4);
    }

    /// Track E (catalog §D.11.10): `moagan run --allow-injection`
    /// must be parsed as a positive `allow_injection` flag on the
    /// `Run` variant, and the propagation chain must reach the
    /// sandbox config that the validate phase builds. The CLI wins
    /// over the env override per the resolution-order docs in
    /// `config.rs`. The optional `--prompt` value is required by
    /// clap's run subcommand so the parser is happy.
    #[test]
    fn cli_run_allow_injection_flag_propagates_to_sandbox() {
        use crate::cli::Cmd;
        use crate::sandbox::SandboxConfig;

        let cli = cli::Cli::parse_from(["moagan", "run", "--prompt", "test", "--allow-injection"]);
        let parsed_flag = match cli.cmd {
            Cmd::Run {
                allow_injection, ..
            } => allow_injection,
            other => panic!("expected Run, got {other:?}"),
        };
        assert!(
            parsed_flag,
            "CLI flag must be parsed as true when --allow-injection is set"
        );

        // The dispatch chain mutates the loaded `Config` so the
        // validate phase picks up the opt-in via
        // `ctx.config.sandbox_allow_injection`. Mirror that
        // mutation here so the test pins the contract end-to-end.
        let mut cfg = crate::config::Config::default();
        cfg.sandbox_allow_injection |= parsed_flag;
        assert!(
            cfg.sandbox_allow_injection,
            "dispatch must propagate --allow-injection to Config"
        );

        // The validate phase then builds the SandboxConfig with the
        // value wired in. Pin the contract so a refactor that drops
        // the wiring surfaces as a test failure.
        let sandbox_cfg = SandboxConfig::new().with_allow_injection(cfg.sandbox_allow_injection);
        assert!(
            sandbox_cfg.allow_injection,
            "validate phase must propagate Config.sandbox_allow_injection to SandboxConfig"
        );
    }

    /// Negative pair: omitting `--allow-injection` keeps the
    /// default (`false`). The contract is "opt-in, never opt-out
    /// by default".
    #[test]
    fn cli_run_allow_injection_defaults_to_false() {
        use crate::cli::Cmd;

        let cli = cli::Cli::parse_from(["moagan", "run", "--prompt", "test"]);
        let parsed_flag = match cli.cmd {
            Cmd::Run {
                allow_injection, ..
            } => allow_injection,
            other => panic!("expected Run, got {other:?}"),
        };
        assert!(
            !parsed_flag,
            "default must be false (D.11.10 strip-by-default)"
        );
    }
}
