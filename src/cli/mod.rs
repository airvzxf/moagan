//! CLI surface. Subcommands are: `run`, `continue`, `resume`, `rerun`,
//! `inspect`, `refine`, `rerank`. v0.2 ships `run`, `inspect`, `refine`,
//! and `rerank`; `continue`/`resume`/`rerun` remain stubbed with the
//! v0.2-friendly error message.

use std::sync::Arc;

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

/// Pipeline mode. v0.2 ships `fast`, `standard`, `deep`, `explore`,
/// and `batch`. `discovery` is deferred to a later sub-phase: its
/// pipeline diverges so heavily (sibling CLI subcommand, separate
/// parser inputs, role prompts that have no analog in the linear
/// pipeline) that adding it to this same flag would muddle the
/// dispatcher. Callers that try `--mode discovery` today get a clap
/// parse error that points them at the upcoming `moagan discover`.
///
/// Cardinality ranges per spec §5.3:
/// - fast:    2-4 agents (~5 batches in parallel)
/// - standard: 6-12 agents (~10 batches)
/// - deep:    12-25 agents (5-6 sketches, 4-5 proposals, 2 repair rounds)
/// - explore: 8-12 sketches (no synthesis, just clustering + map)
/// - batch:   configurable, no human pauses, json-stable output
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Mode {
    /// Quick top-3 candidates (~5 batches in parallel).
    Fast,
    /// Balanced proposals + critics + judges (~10 batches).
    Standard,
    /// 5-6 sketches + 4-5 proposals + 2 repair rounds + multi-judge
    /// panel + adversarial review. Heaviest in-process path.
    Deep,
    /// High-diversity sketches only; no synthesis, ranking is
    /// secondary. Useful for mapping an unknown problem space.
    Explore,
    /// Configurable cardinality, no human pauses, ambiguous
    /// blockers become `NeedsInput` JSON output. CI/automation use.
    Batch,
}

impl Mode {
    /// Stable lowercase string for storage and telemetry.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Standard => "standard",
            Self::Deep => "deep",
            Self::Explore => "explore",
            Self::Batch => "batch",
        }
    }

    /// Whether this mode is allowed to run sketches before proposals.
    /// `fast` skips sketches; everything else runs a `SketchPhase`
    /// between route and propose. `batch` reuses the same answer as
    /// `standard` because batch determinism is a downstream concern.
    pub fn runs_sketches(&self) -> bool {
        !matches!(self, Self::Fast)
    }

    /// Cardinality ceiling on concurrent LLM calls for proposals in
    /// this mode. Spec §5.3 numbers. v0.2 only acts on the upper
    /// bound; the per-mode cardinality tuning lands in Sub-fase A
    /// commit "wire sketch_phase" once the `ProposePhase` accepts
    /// `desired_proposals` as input.
    pub fn desired_proposals(&self) -> usize {
        match self {
            Self::Fast => 3,
            Self::Standard => 3,
            Self::Deep => 5,
            Self::Explore => 0, // no full proposals; sketches only
            Self::Batch => 3,
        }
    }

    /// Cardinality ceiling for the sketches phase. `fast` returns 0
    /// so callers skip the phase entirely. `explore` returns the
    /// upper bound of its spec range; `deep` and `standard` cluster
    /// around 4-6.
    pub fn desired_sketches(&self) -> usize {
        match self {
            Self::Fast => 0,
            Self::Standard => 4,
            Self::Deep => 6,
            Self::Explore => 12,
            Self::Batch => 4,
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
        /// Pipeline mode. v0.2 ships `fast`, `standard`, `deep`,
        /// `explore`, `batch`. `discovery` is deferred and produces
        /// a clap parse error.
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
        /// Override the global cap on concurrent LLM calls. The
        /// value is parsed as `usize`; the constructor (`Parallelism::new`)
        /// clamps to `>= 1`. When omitted, the config-file value
        /// (`cfg.max_parallelism`, default 4) is used. Useful for
        /// `--mode deep` runs against `minimax` real, where 35 judge
        /// calls + 20 critiques + 6 sketches want much more headroom
        /// than the default 4 to amortise network latency.
        #[arg(long, value_name = "N")]
        max_parallelism: Option<usize>,
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

impl Cmd {
    /// The human description of what each subcommand does, used in
    /// `moagan --help` and in error messages.
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Run { .. } => "Start a new run",
            Self::Continue { .. } => "Continue a paused or failed run",
            Self::Resume { .. } => "Resume a run mid-phase",
            Self::Rerun { .. } => "Rerun an existing run with overrides",
            Self::Inspect { .. } => "Inspect runs",
            Self::Refine { .. } => "Re-run the deliver phase for one proposal",
            Self::Rerank { .. } => "Re-run the rank phase on existing evaluations",
            Self::Doctor => "Check the local environment",
        }
    }
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
            max_parallelism,
        } => {
            let run_id = run::run(
                run::RunOptions {
                    mode,
                    provider,
                    prompt,
                    home: runs_dir,
                    mock_dir,
                    non_interactive,
                    max_parallelism,
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
        Cmd::Refine { run_id, proposal } => {
            let home = Arc::new(MoaganHome::resolve()?);
            continue_cmd::run_refine(
                run_id
                    .parse()
                    .map_err(|e| Error::InvalidArgs(format!("{e}")))?,
                &proposal,
                &cfg,
                &home,
            )?;
            println!("refined proposal {proposal} for run {run_id}");
            Ok(0)
        }
        Cmd::Rerank { run_id } => {
            let home = Arc::new(MoaganHome::resolve()?);
            continue_cmd::run_rerank(
                run_id
                    .parse()
                    .map_err(|e| Error::InvalidArgs(format!("{e}")))?,
                &cfg,
                &home,
            )?;
            println!("reranked run {run_id}");
            Ok(0)
        }
        Cmd::Doctor => doctor::run(),
    }
}
