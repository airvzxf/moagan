//! CLI surface. Subcommands are: `run`, `continue`, `resume`, `rerun`,
//! `inspect`, `refine`, `rerank`, `telemetry`. v0.2 ships `run`,
//! `inspect`, `refine`, and `rerank`; `continue`/`resume`/`rerun` remain
//! stubbed with the v0.2-friendly error message. `telemetry` lands in
//! v0.3 sub-fase I (T01-06 §10.7 + V4 §8.7).

use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::storage::sqlite::Db;

pub mod audit;
pub mod continue_cmd;
pub mod discover;
pub mod doctor;
pub mod forbidden;
pub mod inspect;
pub mod run;
pub mod telemetry_cmd;

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
        /// Phase F: opt-out of the synthesis-replacement predicate
        /// (V4 §5.13). When set, the synthesis `s_<NN>` and all its
        /// sources stay in the final ranking together. Default
        /// behaviour (`flag omitted`) is to replace sources when the
        /// synthesis dominates per D.13.16 — `standard`/`deep`/`batch`
        /// get the replacement; `fast` doesn't synthesize, so the
        /// flag is a no-op there.
        #[arg(long, default_value_t = false)]
        no_replace_sources: bool,
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
    /// External, transparent HTTP recorder and verifier. The
    /// `proxy` subcommand is a separate process; the `verify`
    /// subcommand cross-checks the recorded JSONL against Moagan's
    /// internal calls.
    Audit {
        /// Audit subcommand (`proxy` or `verify`).
        #[command(subcommand)]
        sub: AuditCmd,
    },
    /// Discovery mode (Plan B sub-phase B). Generates a knowledge
    /// base by category instead of a winning proposal. See
    /// `docs/proposal-01-concept.md` §6 and `docs/v0.2-status.md`
    /// for the spec.
    Discover {
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
        #[arg(long)]
        mock_dir: Option<std::path::PathBuf>,
        /// Minimum number of sketches to generate. Must be >= 80.
        #[arg(long, default_value_t = 80, value_name = "N")]
        cardinality: usize,
        /// Override the global concurrent-LLM cap.
        #[arg(long, value_name = "N")]
        max_parallelism: Option<usize>,
        /// Number of dimensions in the exploration matrix. Default 4.
        #[arg(long, default_value_t = 4, value_name = "N")]
        dimensions: usize,
        /// Number of facets per dimension. Default 2.
        #[arg(long, default_value_t = 2, value_name = "N")]
        facets_per_dimension: usize,
        /// SimHash threshold for clustering (0..=1). Default 0.7.
        #[arg(long, default_value_t = 0.7)]
        cluster_threshold: f32,
    },
    /// `moagan telemetry` — read-only inspection, dashboard, export,
    /// verify, and retention. v0.3 sub-fase I (T01-06 §10.7 + §10.8
    /// + §10.9 + §10.10; V4 §8.7 + §8.8).
    Telemetry {
        /// Subcommand (`list`, `summary`, `compare`, `provider`,
        /// `view`, `export`, `cleanup`, `verify`).
        #[command(subcommand)]
        sub: telemetry_cmd::TelemetryCmd,
    },
}

/// Subcommands of `moagan audit`.
#[derive(Debug, Subcommand)]
pub enum AuditCmd {
    /// Run the sidecar proxy. Listens on 127.0.0.1 and forwards
    /// traffic to `--upstream`, appending every request/response
    /// to `<run_dir>/telemetry/external_audit.jsonl.gz`.
    Proxy {
        /// Override MOAGAN_HOME.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Target run id. Defaults to the most recent run.
        #[arg(long)]
        run_id: Option<String>,
        /// Bind host.
        #[arg(long, default_value = "127.0.0.1")]
        listen_host: String,
        /// Bind port. `0` means kernel-assigned.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Upstream base URL.
        #[arg(long)]
        upstream: String,
        /// Drop `body_canonical` from the log; keep only `body_sha256`.
        #[arg(long, default_value_t = false)]
        exclude_bodies: bool,
        /// Hard cap on the request body size in bytes.
        #[arg(long, default_value_t = 32 * 1024 * 1024)]
        max_body_bytes: usize,
        /// Upstream HTTP timeout in seconds.
        #[arg(long, default_value_t = 180)]
        timeout_secs: u64,
    },
    /// Cross-check the sidecar JSONL against Moagan's internal
    /// `calls.jsonl.gz` + SQLite. Writes a TSV summary and returns
    /// 0/1/2 according to the audit contract.
    Verify {
        /// Override MOAGAN_HOME.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Target run id. Defaults to the most recent run.
        #[arg(long)]
        run_id: Option<String>,
    },
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
            Self::Audit { .. } => "External, transparent audit trail",
            Self::Discover { .. } => "Discovery mode (knowledge base by category)",
            Self::Telemetry { .. } => "Inspect, export, and serve telemetry dashboards",
        }
    }
}

/// Dispatch the parsed CLI.
pub async fn dispatch(cli: Cli) -> Result<i32> {
    // Run the hard-incompatibilities guard on every entry.
    forbidden::check_local_cargo_toml()?;
    match cli.cmd {
        Cmd::Run {
            mode,
            provider,
            prompt,
            runs_dir,
            mock_dir,
            non_interactive,
            max_parallelism,
            no_replace_sources,
        } => {
            let cfg = Config::load()?;
            let run_id = run::run(
                run::RunOptions {
                    mode,
                    provider,
                    prompt,
                    home: runs_dir,
                    mock_dir,
                    non_interactive,
                    max_parallelism,
                    no_replace_sources,
                },
                &cfg,
            )
            .await?;
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
            let cfg = Config::load()?;
            let home = Arc::new(MoaganHome::resolve()?);
            continue_cmd::run_refine(
                run_id
                    .parse()
                    .map_err(|e| Error::InvalidArgs(format!("{e}")))?,
                &proposal,
                &cfg,
                &home,
            )
            .await?;
            println!("refined proposal {proposal} for run {run_id}");
            Ok(0)
        }
        Cmd::Rerank { run_id } => {
            let cfg = Config::load()?;
            let home = Arc::new(MoaganHome::resolve()?);
            continue_cmd::run_rerank(
                run_id
                    .parse()
                    .map_err(|e| Error::InvalidArgs(format!("{e}")))?,
                &cfg,
                &home,
            )
            .await?;
            println!("reranked run {run_id}");
            Ok(0)
        }
        Cmd::Doctor => doctor::run(),
        Cmd::Audit { sub } => match sub {
            AuditCmd::Proxy {
                runs_dir,
                run_id,
                listen_host,
                port,
                upstream,
                exclude_bodies,
                max_body_bytes,
                timeout_secs,
            } => {
                let args = audit::ProxyArgs {
                    runs_dir,
                    run_id,
                    listen_host,
                    port,
                    upstream,
                    exclude_bodies,
                    max_body_bytes,
                    timeout_secs,
                };
                audit::proxy_cmd(args).await?;
                Ok(0)
            }
            AuditCmd::Verify { runs_dir, run_id } => {
                let args = audit::VerifyArgs { runs_dir, run_id };
                let code = audit::verify_cmd(args).await?;
                Ok(code)
            }
        },
        Cmd::Discover {
            provider,
            prompt,
            runs_dir,
            mock_dir,
            cardinality,
            max_parallelism,
            dimensions,
            facets_per_dimension,
            cluster_threshold,
        } => {
            if cardinality < 80 {
                return Err(Error::InvalidArgs(format!(
                    "cardinality {cardinality} below the discovery minimum of 80"
                )));
            }
            let cfg = Config::load()?;
            let run_id = discover::run(
                discover::DiscoverOptions {
                    provider,
                    prompt,
                    home: runs_dir,
                    mock_dir,
                    cardinality,
                    max_parallelism,
                    dimensions,
                    facets_per_dimension,
                    cluster_threshold,
                    out_dir: None,
                },
                &cfg,
            )
            .await?;
            println!("discovery run id: {run_id}");
            Ok(0)
        }
        Cmd::Telemetry { sub } => telemetry_cmd::TelemetryCmd::dispatch(sub),
    }
}
