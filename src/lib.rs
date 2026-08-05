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
}
