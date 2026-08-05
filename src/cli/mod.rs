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
pub mod diff;
pub mod discover;
pub mod doctor;
pub mod forbidden;
pub mod inspect;
pub mod repair;
pub mod run;
pub mod telemetry_cmd;
pub mod validate;

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
    /// Override the home directory. Globally available; defaults to
    /// the resolved `MOAGAN_HOME` env var (`MOAGAN_RUNS_DIR` is also
    /// honoured as a clap-level alias). Provided globally so
    /// `continue`, `resume`, `rerun`, `refine`, `rerank`, `inspect`,
    /// and `import` all share a single override path (D.14.5).
    #[arg(long, global = true, env = "MOAGAN_RUNS_DIR")]
    pub runs_dir: Option<std::path::PathBuf>,
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
        /// Phase J: reference to an upstream context. Accepts a
        /// UUID v7 (a previous `moagan run` id) or a filesystem
        /// path to a `.md` file or a directory of them. The
        /// resolved contents are prepended to the LLM prompt and
        /// persisted on `manifest.json#parent_run_id` /
        /// `#shared_brief_hash` / `#context_refs`. See T01-06
        /// §3.4–§3.5 and §4.4.
        #[arg(long, value_name = "REF")]
        context: Option<String>,
        /// Phase J: when `--context <run_id>` is set, request a
        /// `SummaryFull` scope (final + sketches). When neither
        /// `--context-summary` nor `--context-full` are set, the
        /// default scope is `Summary` (final only). Errors out if
        /// the flag is set without `--context`.
        #[arg(long, default_value_t = false)]
        context_summary: bool,
        /// Phase J: when `--context <ref>` is set, request a `Full`
        /// scope (every text-like file under the source). Cap 4 MiB
        /// per file. Errors out if the flag is set without
        /// `--context`.
        #[arg(long, default_value_t = false)]
        context_full: bool,
        /// Override the model on the resolved provider. Useful when
        /// the 4 canonical MiniMax models (M3, M2.7, M2.7-highspeed,
        /// M2.5) are already registered as provider entries but
        /// the operator wants to point at a different alias without
        /// editing `config.toml`. Equivalent to
        /// `MOAGAN_MINIMAX_MODEL` and applied after env overrides
        /// (CLI wins on conflict). Empty / whitespace values are
        /// ignored, matching `MOAGAN_MINIMAX_ENDPOINT`.
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
        /// Track E (catalog §D.11.10): opt out of the
        /// secret-stripping pass inside the sandbox. Useful for
        /// debugging / repro cases where the operator wants to see
        /// exactly what bytes were passed to the subprocess. The
        /// default behaviour (`flag omitted`) is to strip secrets
        /// before spawn; this flag forwards to
        /// `Config::sandbox_allow_injection` so the validate-phase
        /// sandbox inherits the opt-in. Equivalent to the env var
        /// `MOAGAN_SANDBOX_ALLOW_INJECTION=true` (CLI wins on
        /// conflict).
        #[arg(long, default_value_t = false)]
        allow_injection: bool,
    },
    /// Continue a paused or failed run.
    Continue {
        /// Run id (defaults to the most recent run).
        #[arg(long)]
        run_id: Option<String>,
        /// Phase J: switch the provider mid-run (e.g. `minimax` →
        /// `mock`). The change is recorded in `provider_changes`
        /// and on `manifest.json#provider`; the in-flight
        /// pipeline picks up the new registry on the next phase.
        #[arg(long)]
        switch_provider: Option<String>,
        /// Phase J: switch the API key the providers read at
        /// startup. Accepted forms:
        ///   - `env:VAR`     — read env var VAR (e.g. `env:OPENAI_API_KEY`)
        ///   - `file:path`   — read first line of file (e.g. `file:~/.openai_key`)
        ///   - literal       — the value itself (least safe; logged with a warning)
        ///
        /// Interactive (`prompt:`) is unavailable without
        /// `dialoguer` and the AGENTS no-go list forbids it; the
        /// spec calls for `dialoguer` but the runtime restriction
        /// wins.
        #[arg(long)]
        switch_api_key: Option<String>,
        /// Phase J: skip the resume checkpoint (the "are you sure?"
        /// gate that prompts before re-running). Records a synthetic
        /// provider-change event so the skip remains auditable.
        #[arg(long, default_value_t = false)]
        skip_checkpoint: bool,
        /// Non-interactive: every checkpoint is a
        /// `<skipped:non_interactive>` marker instead of blocking
        /// on stdin. Useful for CI runs that drive `continue` from
        /// a non-TTY stdin.
        #[arg(long, default_value_t = false)]
        non_interactive: bool,
    },
    /// Resume a run mid-phase (continue without switch flags).
    Resume {
        /// Run id.
        #[arg(long)]
        run_id: String,
        /// Non-interactive: every checkpoint is a
        /// `<skipped:non_interactive>` marker instead of blocking
        /// on stdin. Useful for CI runs.
        #[arg(long, default_value_t = false)]
        non_interactive: bool,
    },
    /// Rerun an existing run with optional overrides.
    Rerun {
        /// Source run id.
        #[arg(long)]
        run_id: String,
        /// Partial JSON matrix of overrides (deep-merged on top of
        /// the original `manifest.execution_policy` + `manifest.brief`).
        /// Alias of `--matrix-override`.
        #[arg(long)]
        override_json: Option<String>,
        /// Alias of `--override-json`. Preferred name (T01-06 §10.4).
        #[arg(long, value_name = "JSON")]
        matrix_override: Option<String>,
        /// Re-run with the original config (default). When this flag
        /// is set the original `manifest.execution_policy` is
        /// carried over verbatim; `--matrix-override` may still
        /// patch specific fields without rebuilding the whole
        /// pipeline.
        #[arg(long, default_value_t = true)]
        same_config: bool,
    },
    /// Import a run directory from another `MOAGAN_HOME` into
    /// the current one. Phase J (T01-06 §10.6).
    Import {
        /// Source directory containing the `manifest.json` of the
        /// run to import. The `run_id` is read from the manifest;
        /// the destination is `<MOAGAN_HOME>/.runs/<run_id>`.
        #[arg(long)]
        source_path: std::path::PathBuf,
        /// Optional destination runs directory. It must be the
        /// current `<MOAGAN_HOME>/.runs` directory so imported runs
        /// remain addressable by later commands.
        #[arg(long)]
        target_runs_dir: Option<std::path::PathBuf>,
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
        /// Mock responses directory. Required when the original run
        /// used `--provider mock` so the deliver re-run can replay
        /// the canned responses (the cache layer covers cache hits
        /// but a cache miss still needs the mock fixtures).
        #[arg(long)]
        mock_dir: Option<std::path::PathBuf>,
    },
    /// Re-rank using the current judges and existing proposals.
    Rerank {
        /// Run id.
        #[arg(long)]
        run_id: String,
    },
    /// Validate a pre-existing brief against hard constraints
    /// without invoking the LLM. Exits 0 on pass, 1 on hard
    /// failure, 2 on bad arguments (missing file / malformed
    /// JSON), 8 on I/O errors. Useful as a CI pre-flight gate.
    /// Spec D.14.4.
    Validate {
        /// Path to the brief JSON file.
        #[arg(value_name = "BRIEF_PATH")]
        brief_path: std::path::PathBuf,
        /// Pipeline mode hint. Currently informational; the
        /// structural check does not depend on it.
        #[arg(long)]
        mode: Option<Mode>,
    },
    /// `moagan diff <run_a> <run_b>` — cross-run comparison (D.14.2).
    /// Wraps `telemetry compare` with filesystem-aware metrics
    /// (proposals / evaluations / phases_visited / ranking delta)
    /// and three output formats (`text`, `md`, `json`). Useful for
    /// `continue` + original or two reruns under different modes
    /// without opening SQLite by hand.
    Diff {
        /// First run id (UUID v7).
        #[arg(value_name = "RUN_A")]
        run_a: String,
        /// Second run id (UUID v7).
        #[arg(value_name = "RUN_B")]
        run_b: String,
        /// Output format. Defaults to `text` when omitted.
        #[arg(long, value_enum)]
        format: Option<diff::DiffFormat>,
        /// Emit per-proposal breakdown for the ranking delta.
        /// Defaults to a one-line summary without it.
        #[arg(long, default_value_t = false)]
        include_proposals: bool,
    },
    /// `moagan repair` — reconcile filesystem vs SQLite (D.14.3 +
    /// D.28.1/3/4/5). Three orthogonal operations, each gated by
    /// its own flag:
    ///   --cleanup-orphans    D.28.3 — remove `*.tmp.<uuid>` and
    ///                                stale `*.lock` files.
    ///   --reindex-artifacts  D.28.5 — sync the per-kind artefact
    ///                                count cache.
    ///   --recover-zombies    D.28.4 — mark stale `running` runs
    ///                                as `interrupted`.
    /// At least one of the three is required. `--run <id>` scopes
    /// every operation to a single run; without it, the dispatch
    /// walks every run in the SQLite index. `--dry-run` prints the
    /// plan without touching disk or SQLite. `--yes` confirms a
    /// destructive plan; without it, a non-empty plan returns
    /// `Error::NeedsInput` (exit 10).
    Repair {
        /// D.28.3: clean `*.tmp.<uuid>` and stale `*.lock` files.
        #[arg(long, default_value_t = false)]
        cleanup_orphans: bool,
        /// D.28.5: reconcile the per-kind artefact count cache.
        #[arg(long, default_value_t = false)]
        reindex_artifacts: bool,
        /// D.28.4: mark stale `running` runs as `interrupted`.
        #[arg(long, default_value_t = false)]
        recover_zombies: bool,
        /// Confirm a destructive plan; without it the dispatcher
        /// returns `Error::NeedsInput`.
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Optional single-run scope (defaults to every run).
        #[arg(long, value_name = "RUN_ID")]
        run: Option<String>,
        /// Print the plan without touching disk or SQLite.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
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
        /// Non-interactive: no prompts. Every checkpoint becomes a
        /// `<skipped:non_interactive>` marker. Required for CI / smoke
        /// runs where stdin is not a TTY (otherwise `discover` would
        /// hang on `intake`'s yes/no prompt).
        #[arg(long, default_value_t = false)]
        non_interactive: bool,
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

/// Whether the current subcommand opens the pipeline. The startup
/// reconcile (Track F) only runs for these — read-only commands like
/// `inspect` / `diff` / `validate` / `doctor` / `telemetry` skip the
/// boot pass so their latency stays deterministic.
fn should_reconcile_at_startup(cmd: &Cmd) -> bool {
    matches!(
        cmd,
        Cmd::Run { .. }
            | Cmd::Continue { .. }
            | Cmd::Resume { .. }
            | Cmd::Rerun { .. }
            | Cmd::Import { .. }
            | Cmd::Refine { .. }
            | Cmd::Rerank { .. }
            | Cmd::Discover { .. }
    )
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
            Self::Import { .. } => "Import a run directory from another MOAGAN_HOME",
            Self::Inspect { .. } => "Inspect runs",
            Self::Refine { .. } => "Re-run the deliver phase for one proposal",
            Self::Rerank { .. } => "Re-run the rank phase on existing evaluations",
            Self::Doctor => "Check the local environment",
            Self::Audit { .. } => "External, transparent audit trail",
            Self::Discover { .. } => "Discovery mode (knowledge base by category)",
            Self::Telemetry { .. } => "Inspect, export, and serve telemetry dashboards",
            Self::Validate { .. } => "Validate a brief without running the pipeline",
            Self::Diff { .. } => "Compare two runs side-by-side (params, artefacts, scores)",
            Self::Repair { .. } => "Reconcile filesystem vs SQLite",
        }
    }
}

/// Dispatch the parsed CLI and convert domain errors into process exit codes.
pub async fn dispatch(cli: Cli) -> Result<i32> {
    match dispatch_inner(cli).await {
        Ok(rc) => Ok(rc),
        Err(e) => {
            eprintln!("error: {e}");
            Ok(e.exit_code() as i32)
        }
    }
}

async fn dispatch_inner(cli: Cli) -> Result<i32> {
    // Run the hard-incompatibilities guard on every entry.
    forbidden::check_local_cargo_toml()?;
    let global_home = match cli.runs_dir {
        Some(path) => MoaganHome::at(path),
        None => MoaganHome::resolve()?,
    };
    // Track F (D.28.3 + D.28.4): reconcile filesystem vs SQLite at
    // the top of every pipeline-opening dispatch. Only commands that
    // actually open the pipeline pay the cost; the read-only ones
    // (`moagan inspect`, `moagan diff`, `moagan doctor`,
    // `moagan telemetry`, `moagan validate`) skip the boot pass so
    // their exit stays deterministic and latency-free. The
    // `Config::startup_reconcile` flag (default `true`) and
    // `MOAGAN_STARTUP_RECONCILE=false` env var gate the call.
    if should_reconcile_at_startup(&cli.cmd) {
        let cfg = Config::load()?;
        if cfg.startup_reconcile {
            let db = Db::open(&global_home.meta_db_path())?;
            let report = crate::reconcile::startup_reconcile(&global_home, &db, &cfg)?;
            tracing::info!(?report, "startup reconcile done");
        }
    }
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
            context,
            context_summary,
            context_full,
            model,
            allow_injection,
        } => {
            // Phase J: validate the `--context-{summary,full}` flags
            // are only useful with `--context`. Setting them without
            // `--context` is a silent no-op today; we surface the
            // mistake as `Error::InvalidArgs` so the operator does
            // not debug a "missing context" run.
            if (context_summary || context_full) && context.is_none() {
                return Err(Error::InvalidArgs(
                    "--context-summary / --context-full require --context <ref>".into(),
                ));
            }
            let scope = if context_full {
                crate::context::ContextScope::Full
            } else if context_summary {
                crate::context::ContextScope::SummaryFull
            } else {
                crate::context::ContextScope::Summary
            };
            let mut cfg = Config::load()?;
            // Track E (catalog §D.11.10): `--allow-injection` opts
            // out of the sandbox's argv-side secret-stripping pass.
            // The flag wins on conflict with the env override so the
            // operator gets the explicit CLI behaviour they asked
            // for. The validate phase reads the cfg knob and wires
            // it into the Sandbox via `with_allow_injection`.
            cfg.sandbox_allow_injection |= allow_injection;
            // Q5: `--model <name>` overrides the model on the resolved
            // provider. Applied AFTER `apply_env_overrides()` (which
            // runs inside `Config::load()`) so the CLI wins on conflict
            // with `MOAGAN_MINIMAX_MODEL`. The override mutates the
            // provider spec so every phase that reads
            // `cfg.provider(name).model` (including the manifest stub
            // and `RunContext::default_model`) sees the new value
            // without any further plumbing.
            //
            // Validation-2026-08-04 fix #2: `--model` accepts the
            // canonical model name (`MiniMax-M3`) AND the lowercase
            // provider-alias form (`minimax-m3`). The alias form
            // resolves to the canonical config.provider entry's
            // model so the upstream API receives a valid model id
            // instead of the alias string verbatim (which causes
            // upstream 4xx errors).
            if let Some(m) = model.as_deref()
                && !m.trim().is_empty()
            {
                let selected = if provider.is_empty() {
                    cfg.default_provider.clone()
                } else {
                    provider.clone()
                };
                let raw = m.trim();
                // Validation-2026-08-04 fix #2 (alias resolution):
                // when `--model` matches a registered provider name AND
                // the resolved provider is the same as the user's
                // selected provider, translate the lowercase alias
                // (`minimax-m3`) to the canonical model (`MiniMax-M3`).
                // When the user explicitly targets a different provider
                // (e.g. `--provider opencode_go --model minimax-m3`),
                // the alias is NOT translated because `minimax-m3` is
                // the legitimate OpenCode Go model id on the /messages
                // endpoint (see Fix #4).
                let resolved = if cfg.providers.contains_key(raw)
                    && (raw == selected
                        || cfg.providers.get(raw).map(|s| s.kind.as_str())
                            == Some(
                                cfg.providers
                                    .get(&selected)
                                    .map(|s| s.kind.as_str())
                                    .unwrap_or(""),
                            )) {
                    cfg.providers
                        .get(raw)
                        .map(|s| s.model.clone())
                        .unwrap_or_else(|| raw.to_owned())
                } else {
                    raw.to_owned()
                };
                let spec = cfg.providers.get_mut(&selected).ok_or_else(|| {
                    Error::InvalidArgs(format!("--model: provider '{selected}' is not in config"))
                })?;
                spec.model = resolved;
            }
            let run_id = run::run(
                run::RunOptions {
                    mode,
                    provider,
                    prompt,
                    home: Some(runs_dir.unwrap_or_else(|| global_home.root().to_path_buf())),
                    mock_dir,
                    non_interactive,
                    max_parallelism,
                    no_replace_sources,
                    context,
                    context_scope: scope,
                },
                &cfg,
            )
            .await?;
            println!("run id: {run_id}");
            Ok(0)
        }
        Cmd::Continue {
            run_id,
            switch_provider,
            switch_api_key,
            skip_checkpoint,
            non_interactive,
        } => {
            let id = run_id.ok_or_else(|| {
                Error::InvalidArgs("--run-id is required for `moagan continue`".into())
            })?;
            let parsed = id.parse().map_err(|e| Error::InvalidArgs(format!("{e}")))?;
            continue_cmd::run_continue(
                &global_home,
                parsed,
                continue_cmd::ContinueOptions {
                    switch_provider,
                    switch_api_key,
                    skip_checkpoint,
                    non_interactive,
                },
            )
            .await?;
            Ok(0)
        }
        Cmd::Resume {
            run_id,
            non_interactive,
        } => {
            let parsed: crate::ids::RunId = run_id
                .parse()
                .map_err(|e| Error::InvalidArgs(format!("{e}")))?;
            continue_cmd::run_resume(&global_home, parsed, non_interactive).await?;
            Ok(0)
        }
        Cmd::Rerun {
            run_id,
            override_json,
            matrix_override,
            same_config: _,
        } => {
            let parsed: crate::ids::RunId = run_id
                .parse()
                .map_err(|e| Error::InvalidArgs(format!("{e}")))?;
            // `--override-json` and `--matrix-override` are aliases;
            // if both are set, prefer `--matrix-override` (the
            // spec-blessed name).
            let raw = matrix_override.or(override_json);
            continue_cmd::run_rerun(&global_home, parsed, raw).await?;
            Ok(0)
        }
        Cmd::Import {
            source_path,
            target_runs_dir,
        } => {
            continue_cmd::run_import(&global_home, &source_path, target_runs_dir.as_deref())?;
            Ok(0)
        }
        Cmd::Inspect {
            limit,
            run_id,
            verbose,
        } => {
            let home = &global_home;
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
        Cmd::Refine {
            run_id,
            proposal,
            mock_dir,
        } => {
            let cfg = Config::load()?;
            let home = Arc::new(global_home.clone());
            continue_cmd::run_refine(
                run_id
                    .parse()
                    .map_err(|e| Error::InvalidArgs(format!("{e}")))?,
                &proposal,
                &cfg,
                &home,
                mock_dir.as_deref(),
            )
            .await?;
            println!("refined proposal {proposal} for run {run_id}");
            Ok(0)
        }
        Cmd::Rerank { run_id } => {
            let cfg = Config::load()?;
            let home = Arc::new(global_home.clone());
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
            non_interactive,
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
                    home: Some(runs_dir.unwrap_or_else(|| global_home.root().to_path_buf())),
                    mock_dir,
                    cardinality,
                    max_parallelism,
                    dimensions,
                    facets_per_dimension,
                    cluster_threshold,
                    out_dir: None,
                    non_interactive,
                },
                &cfg,
            )
            .await?;
            println!("discovery run id: {run_id}");
            Ok(0)
        }
        Cmd::Telemetry { sub } => telemetry_cmd::TelemetryCmd::dispatch(sub).await,
        Cmd::Validate { brief_path, mode } => {
            validate::run(validate::ValidateArgs { brief_path, mode })
        }
        Cmd::Diff {
            run_a,
            run_b,
            format,
            include_proposals,
        } => diff::run(diff::DiffArgs {
            run_a,
            run_b,
            format,
            include_proposals,
        }),
        Cmd::Repair {
            cleanup_orphans,
            reindex_artifacts,
            recover_zombies,
            yes,
            run,
            dry_run,
        } => {
            let parsed_run = match run.as_deref() {
                None => None,
                Some(raw) => Some(
                    raw.parse::<crate::ids::RunId>()
                        .map_err(|e| Error::InvalidArgs(format!("invalid run id '{raw}': {e}")))?,
                ),
            };
            let code = repair::run(repair::RepairArgs {
                cleanup_orphans,
                reindex_artifacts,
                recover_zombies,
                yes,
                run: parsed_run,
                dry_run,
                home_override: None,
            })?;
            Ok(code)
        }
    }
}
