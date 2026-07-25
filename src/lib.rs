//! Moagan — multi-agent system for technical problems through massive solution exploration,
//! curation, and ranking. See `AGENTS.md` for operating rules and `Cargo.toml` for crate
//! metadata.

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]
#![warn(unreachable_pub)]
#![warn(clippy::all)]

pub mod atomic;
pub mod config;
pub mod error;
pub mod fs_layout;
pub mod ids;
pub mod secret;
pub mod storage;
pub mod time;

pub use error::{Error, Result};

/// Reserved entry point exported for `main.rs`. Real implementation lands in module
/// `cli::run` after the CLI subcommands are wired (commit 10).
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("moagan::run is not implemented yet; CLI lands in commit 10")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_returns_not_implemented() {
        assert!(run().is_err());
    }
}
