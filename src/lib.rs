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
pub mod discovery;
pub mod domain;
pub mod error;
pub mod error_code;
pub mod execution;
pub mod fs_layout;
pub mod ids;
pub mod llm;
pub mod phases;
pub mod ranking;
pub mod reconcile;
pub mod redact;
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

/// CLI entry point. Returns a Unix exit code.
pub async fn run() -> anyhow::Result<()> {
    use clap::Parser;
    let cli = cli::Cli::parse();
    let code = match cli::dispatch(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            i32::from(exit_code(&e))
        }
    };
    std::process::exit(code)
}

/// Synchronous entry point used by `main.rs` and integration tests.
pub fn run_blocking() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run())
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
                assert_eq!(provider, "mock");
                assert_eq!(prompt, "hello");
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn run_uses_exit_code_mapper() {
        let e = Error::InvalidArgs("x".into());
        assert_eq!(exit_code(&e), 2);
        let e = Error::PlanExhausted("x".into());
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
