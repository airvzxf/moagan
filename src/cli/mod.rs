//! CLI surface. Subcommands are: `run`, `continue`, `resume`, `rerun`,
//! `inspect`, `refine`, `rerank`. v0.1 ships `run` and `inspect`; the
//! others are stubbed with friendly errors.

use clap::{Parser, Subcommand, ValueEnum};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::storage::sqlite::Db;

pub mod continue_cmd;
pub mod doctor;
pub mod forbidden;
pub mod inspect;
pub mod run;

/// Pipeline mode. The MVP v0.1 ships only `fast` and `standard`.
/// `deep`, `explore`, `batch`, and `discovery` are explicitly
/// rejected at the CLI surface so we don't silently fall back to a
/// `fast` pipeline as the previous `default` branch did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Mode {
    /// Quick top-3 candidates (~5 batches in parallel).
    Fast,
    /// Balanced proposals + critics + judges (~10 batches).
    Standard,
}

impl Mode {
    /// Stable lowercase string for storage and telemetry.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Standard => "standard",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Top-level CLI.
#[derive(Debug, Parser)]
#[command(
    name = "moagan",
    version,
    about = "Multi-agent system for technical problems through massive solution exploration, curation, and ranking.",
    long_about = None
)]
pub struct Cli {
    /// Subcommand.
    #[command(subcommand)]
    pub cmd: Cmd,
}

impl Cli {
    /// Parse from an iterator (useful in tests).
    #[allow(clippy::should_implement_trait)]
    pub fn from_iter_args<I, T>(iter: I) -> Result<Self>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        <Self as Parser>::try_parse_from(iter).map_err(|e| Error::InvalidArgs(e.to_string()))
    }
}

/// All subcommands.
#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Start a new run.
    Run {
        /// Pipeline mode. Only `fast` and `standard` are implemented
        /// in v0.1; other values are rejected at parse time.
        #[arg(long, value_enum, default_value_t = Mode::Fast)]
        mode: Mode,
        /// Provider name (must be in config).
        #[arg(long, default_value = "minimax")]
        provider: String,
        /// User prompt.
        #[arg(long)]
        prompt: String,
        /// Override the home directory.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Load mock responses from this directory (provider=mock only).
        /// Files are read in alphabetical order; each file is a JSON
        /// object with `text` (required), `usage` (optional), and
        /// `finish_reason` (optional). When omitted, the mock returns
        /// an empty queue and the call fails on the first request.
        #[arg(long)]
        mock_dir: Option<std::path::PathBuf>,
        /// Non-interactive: no prompts.
        #[arg(long, default_value_t = false)]
        non_interactive: bool,
    },
    /// Continue a paused or failed run.
    Continue {
        /// Run id (defaults to the most recent run).
        #[arg(long)]
        run_id: Option<String>,
    },
    /// Resume a run mid-phase (continue without switch flags).
    Resume {
        /// Run id.
        #[arg(long)]
        run_id: String,
    },
    /// Rerun an existing run with optional overrides.
    Rerun {
        /// Source run id.
        #[arg(long)]
        run_id: String,
        /// Partial JSON matrix of overrides.
        #[arg(long)]
        override_json: Option<String>,
    },
    /// Inspect runs.
    Inspect {
        /// List the N most recent runs.
        #[arg(long, default_value_t = 10)]
        limit: u32,
        /// Drill into one run by id and print its warnings
        /// summary. When set, `--limit` is ignored.
        run_id: Option<String>,
        /// Print every individual warning event (in addition to
        /// the per-code summary).
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },
    /// Localised refinement: re-run one phase for one proposal.
    Refine {
        /// Run id.
        #[arg(long)]
        run_id: String,
        /// Proposal id.
        #[arg(long)]
        proposal: String,
    },
    /// Re-rank using the current judges and existing proposals.
    Rerank {
        /// Run id.
        #[arg(long)]
        run_id: String,
    },
    /// Check the local environment (API key, writability).
    Doctor,
}

/// Dispatch the parsed CLI.
pub fn dispatch(cli: Cli) -> Result<i32> {
    // Run the hard-incompatibilities guard on every entry.
    forbidden::check_local_cargo_toml()?;
    let cfg = Config::load()?;
    match cli.cmd {
        Cmd::Run {
            mode,
            provider,
            prompt,
            runs_dir,
            mock_dir,
            non_interactive,
        } => {
            let run_id = run::run(
                run::RunOptions {
                    mode,
                    provider,
                    prompt,
                    home: runs_dir,
                    mock_dir,
                    non_interactive,
                },
                &cfg,
            )?;
            println!("run id: {run_id}");
            Ok(0)
        }
        Cmd::Continue { run_id } => {
            let id =
                run_id.ok_or_else(|| Error::InvalidArgs("--run-id is required in v0.1".into()))?;
            let parsed = id.parse().map_err(|e| Error::InvalidArgs(format!("{e}")))?;
            continue_cmd::run_continue(parsed)?;
            Ok(0)
        }
        Cmd::Resume { run_id } => {
            let parsed: crate::ids::RunId = run_id
                .parse()
                .map_err(|e| Error::InvalidArgs(format!("{e}")))?;
            continue_cmd::run_resume(parsed)?;
            Ok(0)
        }
        Cmd::Rerun { run_id, .. } => {
            let parsed: crate::ids::RunId = run_id
                .parse()
                .map_err(|e| Error::InvalidArgs(format!("{e}")))?;
            continue_cmd::run_rerun(parsed)?;
            Ok(0)
        }
        Cmd::Inspect {
            limit,
            run_id,
            verbose,
        } => {
            let home = MoaganHome::resolve()?;
            let db = Db::open(&home.meta_db_path())?;
            if let Some(id) = run_id {
                let parsed = id.parse().map_err(|e| Error::InvalidArgs(format!("{e}")))?;
                match inspect::summarize_run(&db, parsed)? {
                    Some(summary) => {
                        inspect::print_run_summary(&summary, verbose);
                    }
                    None => {
                        return Err(Error::InvalidState(format!(
                            "run {id} not found in the index"
                        )));
                    }
                }
            } else {
                let entries = inspect::list_recent(&db, limit)?;
                for e in entries {
                    println!(
                        "{}  {}  {:>16}  created_unix={}  updated_unix={}",
                        e.run_id.short(),
                        e.mode,
                        e.status,
                        e.created_unix,
                        e.updated_unix
                    );
                }
            }
            Ok(0)
        }
        Cmd::Refine { .. } | Cmd::Rerank { .. } => Err(Error::InvalidState(
            "refine and rerank land in v0.2; tracked in the integrated catalog".into(),
        )),
        Cmd::Doctor => doctor::run(),
    }
}
