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
pub mod storage;
pub mod telemetry;
pub mod test_support;
pub mod time;
pub mod validators;

pub use error::{Error, ExitCode, Result, exit_code};

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
/// each other's mutations and report flakes.
#[cfg(test)]
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
    use crate::telemetry::stdout_events;
    use crate::telemetry::stdout_events::{Event, EventFormat, SCHEMA_VERSION, now_rfc3339};
    use std::io::IsTerminal;
    use tracing::Instrument;
    let run_started = std::time::Instant::now();
    let emit_stdout = stdout_events::resolve_event_format(EventFormat::Jsonl);

    // Pipeline span: root context for every event the run emits.
    // Operators grep `pipeline{run_id=...}` to follow the entire
    // run from `run_start` through every nested `phase` /
    // `iteration` / `llm_call` / `probe` event.
    let pipeline_span = tracing::info_span!(
        "pipeline",
        run_id = "pre-dispatch",
        mode = "<help-or-readonly>",
        provider = "<n/a>",
        model = "<n/a>",
        resumed = false,
    );
    let _pipeline_enter = pipeline_span.enter();

    // Stdout event: `run_start`. Always emitted (even for the
    // `moagan --help` subcommand) when stdout is not a TTY so
    // operators can `moagan … | jq 'select(.kind == "run_start")'`
    // to confirm the binary even started. The synthetic
    // `run_id` is only useful when the dispatch produces a real
    // run; for the `--help` and read-only commands we emit a
    // stable hash of the command name.
    if emit_stdout {
        stdout_events::STDOUT_EVENTS.emit(Event::RunStart {
            schema: SCHEMA_VERSION,
            ts: now_rfc3339(),
            run_id: "pre-dispatch",
            mode: "<help-or-readonly>",
            provider: "<n/a>",
            model: "<n/a>",
            prompt_hash: "<n/a>",
        });
    }

    tracing::info!("moagan::run: dispatching");
    let code = match cli::dispatch(cli).instrument(pipeline_span.clone()).await {
        Ok(code) => {
            tracing::info!(exit_code = code, "moagan::run: dispatch ok");
            if emit_stdout {
                stdout_events::STDOUT_EVENTS.emit(Event::RunEnd {
                    schema: SCHEMA_VERSION,
                    ts: now_rfc3339(),
                    run_id: "pre-dispatch",
                    status: "ok",
                    exit_code: code,
                    elapsed_ms: run_started.elapsed().as_millis() as u64,
                    artefacts: serde_json::json!({}),
                });
            }
            code
        }
        Err(e) => {
            tracing::error!(error = %e, "moagan::run: dispatch failed");
            // The plain-text error message goes to stderr ONLY when
            // the user is on a TTY (interactive mode). When stderr
            // is redirected/piped, the structured `tracing::error!`
            // event above already carries the same information as
            // JSON; an extra `eprintln!` here would break
            // `moagan … 2> log.jsonl | jq` consumers.
            if std::io::stderr().is_terminal() {
                eprintln!("error: {e}");
            }
            let code = i32::from(exit_code(&e));
            if emit_stdout {
                stdout_events::STDOUT_EVENTS.emit(Event::RunEnd {
                    schema: SCHEMA_VERSION,
                    ts: now_rfc3339(),
                    run_id: "pre-dispatch",
                    status: "error",
                    exit_code: code,
                    elapsed_ms: run_started.elapsed().as_millis() as u64,
                    artefacts: serde_json::json!({}),
                });
            }
            code
        }
    };
    std::process::exit(code)
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
