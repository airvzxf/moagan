//! CLI surface. Subcommands are: `run`, `continue`, `resume`, `rerun`,
//! `inspect`, `refine`, `rerank`, `telemetry`. v0.2 ships `run`,
//! `inspect`, `refine`, and `rerank`; `continue`/`resume`/`rerun` remain
//! stubbed with the v0.2-friendly error message. `telemetry` lands in
//! v0.3 sub-fase I (T01-06 §10.7 + V4 §8.7).

use std::io::IsTerminal;
use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use tracing::{debug, error, info, trace, warn};

use crate::cli::discover::MIN_SKETCHES_PER_CELL;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::storage::sqlite::Db;

pub mod audit;
pub mod continue_cmd;
pub mod coverage_cmd;
pub mod diff;
pub mod discover;
pub mod discover_explain;
pub mod doctor;
pub mod flags_batch;
pub mod forbidden;
pub mod inspect;
pub mod pause_cmd;
pub mod probe;
pub mod rate;
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
        let s = match self {
            Self::Fast => "fast",
            Self::Standard => "standard",
            Self::Deep => "deep",
            Self::Explore => "explore",
            Self::Batch => "batch",
        };
        trace!(mode = s, "Mode::as_str");
        s
    }

    /// Whether this mode is allowed to run sketches before proposals.
    /// `fast` skips sketches; everything else runs a `SketchPhase`
    /// between route and propose. `batch` reuses the same answer as
    /// `standard` because batch determinism is a downstream concern.
    pub fn runs_sketches(&self) -> bool {
        let v = !matches!(self, Self::Fast);
        trace!(mode = ?self, runs_sketches = v, "Mode::runs_sketches");
        v
    }

    /// Cardinality ceiling on concurrent LLM calls for proposals in
    /// this mode. Spec §5.3 numbers. v0.2 only acts on the upper
    /// bound; the per-mode cardinality tuning lands in Sub-fase A
    /// commit "wire sketch_phase" once the `ProposePhase` accepts
    /// `desired_proposals` as input.
    pub fn desired_proposals(&self) -> usize {
        let v = match self {
            Self::Fast => 3,
            Self::Standard => 3,
            Self::Deep => 5,
            Self::Explore => 0, // no full proposals; sketches only
            Self::Batch => 3,
        };
        debug!(mode = ?self, desired_proposals = v, "Mode::desired_proposals");
        v
    }

    /// Cardinality ceiling for the sketches phase. `fast` returns 0
    /// so callers skip the phase entirely. `explore` returns the
    /// upper bound of its spec range; `deep` and `standard` cluster
    /// around 4-6.
    pub fn desired_sketches(&self) -> usize {
        let v = match self {
            Self::Fast => 0,
            Self::Standard => 4,
            Self::Deep => 6,
            Self::Explore => 12,
            Self::Batch => 4,
        };
        debug!(mode = ?self, desired_sketches = v, "Mode::desired_sketches");
        v
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// CLI-facing variant of [`crate::phases::PipelineKind`] for
/// `moagan continue --kind <linear|discovery>`. Lives here because
/// clap derive macros need it next to the subcommand definition;
/// the dispatcher maps it back to the canonical
/// [`crate::phases::PipelineKind`] before calling
/// [`crate::cli::continue_cmd::run_continue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum ContinueKindArg {
    /// Linear pipeline (`fast | standard | deep | explore | batch`).
    Linear,
    /// Discovery pipeline (`moagan discover`). v0.5 PR-24.
    Discovery,
}

impl From<ContinueKindArg> for crate::phases::PipelineKind {
    fn from(value: ContinueKindArg) -> Self {
        match value {
            ContinueKindArg::Linear => crate::phases::PipelineKind::Linear,
            ContinueKindArg::Discovery => crate::phases::PipelineKind::Discovery,
        }
    }
}

/// stderr format selector (per ADR-0002 §B).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum LogFormatArg {
    /// Pick `text` when stderr is a TTY (interactive shell),
    /// `json` otherwise (CI, redirected, piped). The default.
    Auto,
    /// Coloured text. Optimised for humans in a terminal.
    Text,
    /// NDJSON, one event per line. Optimised for `jq` /
    /// `Promtail` / `Loki` / `Datadog` ingestion.
    Json,
}

/// stdout event-stream selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum EventFormatArg {
    /// Pick `jsonl` when stdout is not a TTY and stay silent
    /// on a TTY. Honours `MOAGAN_EVENT_FORMAT` if set.
    Auto,
    /// Emit one NDJSON event per line on stdout when stdout is
    /// not a TTY; stay silent in interactive mode so the
    /// terminal output stays uncluttered.
    Jsonl,
    /// No stdout events. Use when the run's stdout must remain
    /// minimal (e.g. wrapping moagan inside another TUI).
    Off,
}

/// stdout decision-event verbosity selector. Decoupled from
/// [`EventFormatArg`] so the high-volume cache / judge / category
/// events can be silenced or saturated independently of the rest
/// of the bus. See `docs/events-v1.md` for the curated list and
/// the Summary / All split rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum DecisionFormatArg {
    /// Silence every `Decision` event.
    Off,
    /// Emit only the curated, low-volume decisions (default).
    /// Covers `winner_picked`, `low_confidence_winner`,
    /// `cluster_skipped`, `repair_applied`, `portfolio_finalized`.
    Summary,
    /// Emit every decision, including per-LLM-call cache events
    /// (`cache_hit` / `cache_miss`), per-proposal judge verdicts
    /// (`judge_verdict`), and per-sketch category assignments
    /// (`category_assigned`). Intended for dashboards and audits,
    /// not run-of-the-mill console output.
    All,
}

impl DecisionFormatArg {
    /// Map the CLI-facing [`DecisionFormatArg`] to the
    /// internal [`crate::telemetry::stdout_events::DecisionFormat`]
    /// used by the resolution / `should_emit_decision` helpers.
    pub fn to_internal(self) -> crate::telemetry::stdout_events::DecisionFormat {
        use crate::telemetry::stdout_events::DecisionFormat;
        match self {
            Self::Off => DecisionFormat::Off,
            Self::Summary => DecisionFormat::Summary,
            Self::All => DecisionFormat::All,
        }
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
    /// Format of the tracing events emitted to stderr. `auto`
    /// (default) picks `text` when stderr is a TTY and `json`
    /// otherwise — the latter produces NDJSON that downstream
    /// tooling (`jq`, Promtail, Loki, Datadog) can parse without a
    /// custom adapter. The `--logs <PATH>` flag and `MOAGAN_RUN_LOGS`
    /// env var that this flag replaces were removed in v0.11; use
    /// POSIX redirection (`2> path`) to capture stderr instead.
    /// Per ADR-0002 §B.
    #[arg(long, global = true, value_enum, default_value_t = LogFormatArg::Auto,
          env = "MOAGAN_LOG_FORMAT")]
    pub log_format: LogFormatArg,
    /// Format of the structured events emitted to stdout. `auto`
    /// (default) picks `jsonl` when stdout is not a TTY and stays
    /// silent on a TTY. `off` silences stdout entirely. The
    /// `MOAGAN_EVENT_FORMAT` env var is honoured with the same
    /// precedence as the explicit flag (env > flag > default).
    #[arg(long, global = true, value_enum, default_value_t = EventFormatArg::Auto,
          env = "MOAGAN_EVENT_FORMAT")]
    pub event_format: EventFormatArg,
    /// Verbosity of the `Decision` events emitted on stdout (the
    /// curated audit trail of operator-facing decisions made by the
    /// pipeline). Independent from `--event-format`: even when the
    /// event stream is `off`, `--decision-format all` does NOT
    /// produce output. `summary` (default) emits the curated,
    /// low-volume decisions (`winner_picked`,
    /// `low_confidence_winner`, `cluster_skipped`, `repair_applied`,
    /// `portfolio_finalized`) — one per run, suitable for log
    /// aggregation. `all` additionally emits the high-volume
    /// decisions (`cache_hit`, `cache_miss`, `judge_verdict`,
    /// `category_assigned`) which can produce dozens of events per
    /// run; intended for dashboards / audits, not run-of-the-mill
    /// console output. `off` silences every `Decision` event. The
    /// `MOAGAN_DECISION_FORMAT` env var is honoured with the same
    /// precedence as the explicit flag (env > flag > default).
    #[arg(long, global = true, value_enum, default_value_t = DecisionFormatArg::Summary,
          env = "MOAGAN_DECISION_FORMAT")]
    pub decision_format: DecisionFormatArg,
    /// DEPRECATED — v0.12.0..v0.13.x. Removed in v0.14.0.
    /// Historically `moagan` wrote all tracing logs to stderr; the
    /// v0.12.0 stream routing flip (PR-04a / E-1) sends logs to
    /// **stdout** by default (only `ERROR`-level events still go
    /// to stderr). Set `--log-to-stderr` (or
    /// `MOAGAN_LOG_TO_STDERR=1`) to keep the legacy
    /// "all-logs-on-stderr" behaviour for scripts that pipe
    /// `2> log.jsonl`. A `DEPRECATED` warning is emitted to the
    /// tracing subscriber every time the flag is active so the
    /// operator sees the migration deadline. Migration:
    /// `1> out.jsonl 2> errors.jsonl` (the canonical Unix split).
    ///
    /// The parser is `BoolishValueParser` so the env var accepts the
    /// shell-conventional `1`/`0` (alongside `true`/`false` /
    /// `yes`/`no` / `on`/`off`); clap's strict `bool` parser only
    /// takes `true`/`false` and would reject
    /// `MOAGAN_LOG_TO_STDERR=1` from the Makefile with
    /// `error: invalid value '1' for '--log-to-stderr'`. Issue #657
    /// fix #1.
    #[arg(
        long,
        global = true,
        default_value_t = false,
        env = "MOAGAN_LOG_TO_STDERR",
        value_parser = clap::builder::BoolishValueParser::new(),
    )]
    pub log_to_stderr: bool,
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
        trace!("Cli::from_iter_args: parsing");
        let res =
            <Self as Parser>::try_parse_from(iter).map_err(|e| Error::InvalidArgs(e.to_string()));
        match &res {
            Ok(_) => debug!("Cli::from_iter_args: parsed ok"),
            Err(e) => warn!(error = %e, "Cli::from_iter_args: parse failed"),
        }
        res
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
        /// Provider name (`SECTION` or `SECTION:MODEL`).
        ///
        /// Mandatory for every LLM-touching command in v0.10. The
        /// dispatcher builds one `Provider` per `(section, model)`
        /// pair, so a multi-model section like `[providers.opencode]`
        /// requires `SECTION:MODEL` to disambiguate the target.
        /// Single-model sections (`[providers.minimax]`,
        /// `[providers.deepseek]`, `[providers.mock]`) accept the
        /// bare `SECTION` form.
        ///
        /// Resolution order (highest first):
        /// 1. CLI flag.
        /// 2. `MOAGAN_DEFAULT_PROVIDER` env var (same `SECTION[:MODEL]`
        ///    format).
        /// 3. `[defaults] provider` in `~/.config/moagan/config.toml`.
        ///
        /// If none is set the command fails with a clear error
        /// message listing the operator's options.
        #[arg(long, value_name = "SECTION[:MODEL]")]
        provider: Option<String>,
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
        /// Non-interactive: no prompts. Honours the
        /// `MOAGAN_NON_INTERACTIVE` env var so CI / smoke scripts
        /// that forget the flag still skip checkpoints instead of
        /// blocking on stdin. CLI > env > default precedence.
        /// The parser is `BoolishValueParser` so the env var
        /// accepts the shell-conventional `1`/`0` (alongside
        /// `true`/`false` / `yes`/`no`); clap's strict `bool`
        /// parser only takes `true`/`false` and would reject
        /// `MOAGAN_NON_INTERACTIVE=1` from the Makefile.
        #[arg(
            long,
            default_value_t = false,
            env = "MOAGAN_NON_INTERACTIVE",
            value_parser = clap::builder::BoolishValueParser::new(),
        )]
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
        /// D.22.1, D.12.5: opt-in for the deterministic pattern-based
        /// adversary pass that runs the seven patterns from
        /// `src/ranking/adversary_patterns.rs::run_all_patterns`
        /// against the just-judged proposals and writes
        /// `rankings/adversary_report.json`. The pass is also enabled
        /// automatically for `Mode::Deep` (the only mode where the
        /// seven-pattern cost is amortised); the explicit flag wins
        /// on conflict because operators can disable it for deep via
        /// the inverse. Default `false` (off for `fast`/`standard`/
        /// `explore`/`batch`).
        #[arg(long, default_value_t = false)]
        adversary: bool,
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
        /// Track K (catalog §D.21): apply a domain-specific profile
        /// on top of the loaded `Config`. Looks up `<name>.toml`
        /// under `$MOAGAN_HOME/profiles/` and falls back to
        /// `~/.config/moagan/profiles/`. Supports `extends`
        /// inheritance (child overrides parent). Applied AFTER
        /// `Config::load()` + env overrides so the CLI wins on
        /// conflict. Empty / whitespace values are ignored to
        /// match the convention used by `--model` and
        /// `--runs-dir`.
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        /// Hash algorithm threaded through the export-side
        /// checksums. `blake3` matches the canonical internal
        /// hash (cache keys, ledger, brief binding); `sha256`
        /// is the audit-friendly alternative that auditors can
        /// re-verify with the usual CLI tooling. The choice is
        /// mirrored onto `Config::export.hash_algo`; the
        /// `--hash-algo` flag wins on conflict with the
        /// `MOAGAN_HASH_ALGO` env var. Empty / whitespace
        /// values are ignored so an accidental trailing space
        /// does not silently flip the algorithm.
        #[arg(long, value_name = "ALGO")]
        hash_algo: Option<String>,
    },
    /// Continue a paused or failed run.
    Continue {
        /// Run id (defaults to the most recent run).
        #[arg(long)]
        run_id: Option<String>,
        /// Track K.2b: resume from a `paused.json` instead of
        /// querying SQLite for the last completed phase. When set,
        /// the dispatcher reads `<run_dir>/paused.json` and (today)
        /// prints the resume plan; the actual loop skip that uses
        /// the file lands in PR C.5 (K.2 wires `continue_cmd.rs`).
        #[arg(long, default_value_t = false)]
        from_pause: bool,
        /// v0.5 PR-24 (V4 §6.11, T01-06 §10.2): which pipeline kind
        /// the run belongs to. Defaults to `linear` for the
        /// historic `fast | standard | deep | explore | batch`
        /// runs. `discovery` resumes a `moagan discover` run by
        /// stitching the coordinator (matrix fan-out) with the
        /// post-matrix pipeline (`discover_tag → ... →
        /// discover_summary`) using the filtered canonical
        /// discovery pipeline as the reference. Without this
        /// flag, `moagan continue <discover_run_id>` fails with
        /// `unknown phase "discover_matrix"` because the linear
        /// canonical list does not include the `discover_*`
        /// phases.
        #[arg(long, value_enum, default_value_t = ContinueKindArg::Linear)]
        kind: ContinueKindArg,
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
        /// a non-TTY stdin. Honours the `MOAGAN_NON_INTERACTIVE`
        /// env var with CLI > env > default precedence. Parser is
        /// `BoolishValueParser` so the env var accepts `1`/`0`
        /// alongside `true`/`false` / `yes`/`no`.
        #[arg(
            long,
            default_value_t = false,
            env = "MOAGAN_NON_INTERACTIVE",
            value_parser = clap::builder::BoolishValueParser::new(),
        )]
        non_interactive: bool,
    },
    /// Resume a run mid-phase (continue without switch flags).
    Resume {
        /// Run id.
        #[arg(long)]
        run_id: String,
        /// Non-interactive: every checkpoint is a
        /// `<skipped:non_interactive>` marker instead of blocking
        /// on stdin. Useful for CI runs. Honours the
        /// `MOAGAN_NON_INTERACTIVE` env var with CLI > env >
        /// default precedence. Parser is `BoolishValueParser` so
        /// the env var accepts `1`/`0` alongside `true`/`false` /
        /// `yes`/`no`.
        #[arg(
            long,
            default_value_t = false,
            env = "MOAGAN_NON_INTERACTIVE",
            value_parser = clap::builder::BoolishValueParser::new(),
        )]
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
        /// pipeline. Pass `--same-config=false` to opt out of the
        /// override (the cloned manifest is the authoritative
        /// config; any `--matrix-override` JSON is silently
        /// ignored).
        #[arg(long, action = clap::ArgAction::Set, default_value_t = true, value_parser = clap::value_parser!(bool))]
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
        /// PR-7: print the snapshot of which provider / model
        /// capabilities were in effect during the run (read from
        /// the manifest's `provider` and `model` fields, plus the
        /// `models.dev` catalog row if the on-disk cache is
        /// present). When the manifest is missing the field, the
        /// command prints a warning and exits 0 so the operator
        /// can still chain it into shell scripts.
        #[arg(long, default_value_t = false)]
        capabilities: bool,
    },
    /// Localised refinement. Two sub-modes, mutually exclusive:
    ///
    /// - `--action <action>`: invoke
    ///   `src/phases/refine.rs::dispatch_refine_action` (D.22.2)
    ///   on the run. The action is one of `tighten-constraint`,
    ///   `add-evidence`, `split-proposal`, `merge-proposal`,
    ///   `rerun-critique`, `drop-proposal`, `request-human-input`
    ///   (kebab-case; snake_case also accepted). Optional
    ///   `--verdict-detail <text>` supplies the constraint text
    ///   for `tighten-constraint`.
    /// - `--proposal <proposal_id>`: re-issue the deliver prompt
    ///   for one specific proposal and write
    ///   `final/refined_<proposal_id>.md`.
    Refine {
        /// Run id.
        #[arg(long)]
        run_id: String,
        /// Proposal id (legacy sub-mode: re-run deliver for one
        /// proposal). Mutually exclusive with `--action`.
        #[arg(long, conflicts_with = "action", required_unless_present = "action")]
        proposal: Option<String>,
        /// Refine action (D.22.2). One of the seven kebab-case
        /// wire forms (`tighten-constraint`, `add-evidence`,
        /// `split-proposal`, `merge-proposal`, `rerun-critique`,
        /// `drop-proposal`, `request-human-input`). Mutually
        /// exclusive with `--proposal`.
        #[arg(long, conflicts_with = "proposal", value_parser = clap::value_parser!(crate::ranking::RefineAction))]
        action: Option<crate::ranking::RefineAction>,
        /// Verdict detail forwarded to the dispatcher as
        /// `RefineContext.verdict_detail`. Used by
        /// `tighten-constraint` (recorded as the appended
        /// `prohibited_decisions` entry) and `add-evidence`
        /// (recorded as a source-context note).
        #[arg(long)]
        verdict_detail: Option<String>,
        /// Mock responses directory. Required when the original
        /// run used `--provider mock` so the deliver re-run can
        /// replay the canned responses (the cache layer covers
        /// cache hits but a cache miss still needs the mock
        /// fixtures). Only used by the `--proposal` sub-mode.
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
    ///
    /// PR-7 adds `--capabilities` to print the resolved capability
    /// matrix per `(provider, model)` the operator has configured,
    /// cross-referenced with the `models.dev` catalog when the
    /// on-disk cache is available. The default behaviour (no
    /// flag) keeps the pre-PR-7 environment check unchanged so
    /// existing CI scripts do not regress.
    Doctor {
        /// Print the per-provider capability table instead of
        /// (or in addition to) the standard environment checks.
        #[arg(long, default_value_t = false)]
        capabilities: bool,
    },
    /// `moagan probe <verb>` — operator-driven diagnostics for
    /// the LLM transport layer. Verb-first naming per the
    /// operator-facing convention (the `moagan <verb> <noun>`
    /// order reads naturally in a shell). The sub-commands are
    /// the on-demand counterparts to the startup auto-probes
    /// and the manual pins the operator can set when an
    /// auto-probe misfires. Today: `max_tokens` and
    /// `temperature`.
    Probe {
        /// Probe sub-command (`max_tokens`, `temperature`).
        #[command(subcommand)]
        sub: probe::ProbeCmd,
    },
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
        /// Provider name (`SECTION` or `SECTION:MODEL`). See the
        /// `run` command's `--provider` flag for the resolution
        /// order and the v0.10 mandatory semantics.
        #[arg(long, value_name = "SECTION[:MODEL]")]
        provider: Option<String>,
        /// User prompt.
        #[arg(long)]
        prompt: String,
        /// Override the home directory.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Load mock responses from this directory (provider=mock only).
        #[arg(long)]
        mock_dir: Option<std::path::PathBuf>,
        /// F2 (Track G.2): sketches per matrix cell. The matrix
        /// fan-out is `cells() × sketches_per_cell ×
        /// profile_total`. Default `10` (replaces the v0.5
        /// `cardinality = 80` floor); must be `>= 1` (the F2
        /// operator-facing floor, lowered from `10` in v0.13.2).
        /// A 4-dim × 2-facet matrix with the default fan-out
        /// produces 80 sketches; raise `sketches_per_cell` to
        /// expand the per-cell fan-out without adding cells.
        /// Lower bound 1 is intended for integration tests and
        /// debugging; nominal discovery runs should use the
        /// default 10. Overridden by
        /// `MOAGAN_DISCOVERY_SKETCHES_PER_CELL` (env) and
        /// `[discovery_matrix].sketches_per_cell` (TOML); the
        /// CLI flag wins on conflict.
        #[arg(long = "sketches-per-cell", default_value_t = 10, value_name = "N")]
        sketches_per_cell: usize,
        /// Override the global concurrent-LLM cap.
        #[arg(long, value_name = "N")]
        max_parallelism: Option<usize>,
        /// F1 (Track G.2): target number of dimensions in the
        /// exploration matrix. `None` lets the
        /// `Role::DimensionDeriver` pick the dimension count
        /// freely (asymmetric facets are allowed). Ignored
        /// when `--matrix-spec` is supplied; required when
        /// the operator wants `--facets-per-dimension` to be
        /// honoured without a spec.
        #[arg(long, value_name = "N")]
        dimensions: Option<usize>,
        /// F1 (Track G.2): target facets per dimension when the
        /// operator does NOT supply a `--matrix-spec`. Requires
        /// `--dimensions`; without a spec the LLM is free to
        /// pick asymmetric facet counts (the F1 contract).
        #[arg(long, value_name = "N")]
        facets_per_dimension: Option<usize>,
        /// F1 (Track G.2): operator-supplied matrix spec.
        /// Repetible; each occurrence appends one dimension.
        /// Two accepted formats (the parser handles both):
        ///
        /// * Repetible form — `--matrix-spec 'auth=oauth,api-key'`
        ///   --matrix-spec 'storage=sql,kv'`. Each flag declares
        ///   exactly one dimension.
        /// * Consolidated form — a single flag can declare several
        ///   dimensions separated by `;`:
        ///   `--matrix-spec 'deployment=serverless,self-hosted;storage=sql,kv'`.
        ///
        /// When non-empty, the matrix uses the spec verbatim and
        /// the `Role::DimensionDeriver` is NOT invoked.
        #[arg(long = "matrix-spec", value_name = "SPEC", action = clap::ArgAction::Append)]
        matrix_spec: Vec<String>,
        /// F1 (Track G.2): force the LLM-derive path even when
        /// the operator did not pass a spec. Useful in CI to
        /// exercise the `Role::DimensionDeriver` call.
        #[arg(long, default_value_t = false)]
        llm_derive: bool,
        /// SimHash threshold for clustering (0..=1). Default 0.7.
        #[arg(long, default_value_t = 0.7)]
        cluster_threshold: f32,
        /// Non-interactive: no prompts. Every checkpoint becomes a
        /// `<skipped:non_interactive>` marker. Required for CI / smoke
        /// runs where stdin is not a TTY (otherwise `discover` would
        /// hang on `intake`'s yes/no prompt). Honours the
        /// `MOAGAN_NON_INTERACTIVE` env var with CLI > env > default
        /// precedence. Parser is `BoolishValueParser` so the env var
        /// accepts `1`/`0` alongside `true`/`false` / `yes`/`no`.
        #[arg(
            long,
            default_value_t = false,
            env = "MOAGAN_NON_INTERACTIVE",
            value_parser = clap::builder::BoolishValueParser::new(),
        )]
        non_interactive: bool,
        /// Opt-in switch for the cross-run facet cache (V4 §6.8 +
        /// catalog D.13.13). When set, the `discover_facet` phase
        /// writes derived facet lists to
        /// `<MOAGAN_HOME>/cache/facets/` keyed by
        /// `sha256(brief + category_id)` and skips the
        /// `facet_deriver` LLM call on subsequent runs that share
        /// the same brief + category. Default `false` so the
        /// baseline "LLM every run" contract is preserved unless
        /// the operator explicitly opts in. The TTL is
        /// `MOAGAN_FACET_CACHE_TTL_SECS` (default 7 days).
        #[arg(long, default_value_t = false)]
        cache_facets: bool,
        /// PR-D1: per-provider sampling-temperature profile. May
        /// be passed multiple times; each occurrence applies one
        /// profile to the named provider model. The spec grammar is
        /// `provider=<model>;temperatures=<csv>;replicas=<n>`:
        ///
        /// * `provider=<model>` — provider MODEL name (the same
        ///   string stored on `ProviderConfig::model`, e.g.
        ///   `MiniMax-M3`, `mimo-v2.5`).
        /// * `temperatures=<csv>` — comma-separated floats in
        ///   `0.0..=2.0`. At least one value required.
        /// * `replicas=<n>` — integer `>= 1`.
        ///
        /// Multiple specs for the same provider are allowed; the
        /// LAST one wins (documented in `cli::discover`).
        /// Providers without a spec fall back to the matrix's
        /// `default_profile` (`[1.0] × 1`), which reproduces the
        /// v0.5 single-shot contract byte-for-byte. Default empty.
        #[arg(long = "temperature-profile", value_name = "SPEC", action = clap::ArgAction::Append)]
        temperature_profiles: Vec<String>,
        /// F3 (Track G.2): print the cardinality calculation and
        /// exit. Does NOT start a run, does NOT call the LLM
        /// (even when `--llm-derive` is set), does NOT create a
        /// run directory. The cells count is reported as a
        /// placeholder when no `--matrix-spec` is supplied (the
        /// `Role::DimensionDeriver` would normally own that
        /// resolution at runtime). Useful as a pre-flight sanity
        /// check before a real run.
        #[arg(long, default_value_t = false)]
        explain: bool,
    },
    /// Smoke-test the END-TO-END pipeline (discover + run --mode fast)
    /// against the real provider. Two-run flow:
    ///
    /// 1. `moagan discover` with cardinalidad 8 (1 sketch per
    ///    dimension × facets_per_dimension = 1), 1 temperature
    ///    (1.0), 1 replica. Produces a `run_id` with a
    ///    persisted `sketches/` library.
    /// 2. `moagan run --mode fast` that consumes the discover
    ///    run's library via `--context <run_id> --context-full`
    ///    (the `Full` scope loads every text-like file under the
    ///    run dir, including `sketches/*.json`).
    ///
    /// Cost: ~60-120 s of API budget, ~3 MB of disk. Exercises
    /// both the discovery code path (matrix build + coordinator)
    /// AND the run --mode fast path (intake → clarify → sketch →
    /// judges → tag → cluster → facets → portfolio), so a
    /// preflight that succeeds is a strong end-to-end signal.
    /// Both run ids are printed on stdout so the operator can
    /// drill into either one.
    ///
    /// The pre-PR-564 version only wrapped `discover`, which
    /// missed the upstream intake/clarify phases. PR #565 wrapped
    /// `run --mode fast` only, which duplicates the existing
    /// fast-mode test coverage. The two-step flow is the actual
    /// addition because it links the two runs through
    /// `--context`, validating the cross-run plumbing AND the
    /// per-run planner.
    Preflight {
        /// Provider name (`SECTION` or `SECTION:MODEL`). See the
        /// `run` command's `--provider` flag for the resolution
        /// order and the v0.10 mandatory semantics.
        #[arg(long, value_name = "SECTION[:MODEL]")]
        provider: Option<String>,
        /// User prompt.
        #[arg(long)]
        prompt: String,
        /// Override the home directory.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Load mock responses from this directory (provider=mock only).
        #[arg(long)]
        mock_dir: Option<std::path::PathBuf>,
        /// Override the global concurrent-LLM cap.
        #[arg(long, value_name = "N")]
        max_parallelism: Option<usize>,
        /// Non-interactive: no prompts. Honours the
        /// `MOAGAN_NON_INTERACTIVE` env var so CI / smoke scripts
        /// that forget the flag still skip checkpoints instead of
        /// blocking on stdin. CLI > env > default precedence.
        /// Parser is `BoolishValueParser` so the env var accepts
        /// `1`/`0` alongside `true`/`false` / `yes`/`no`.
        #[arg(
            long,
            default_value_t = false,
            env = "MOAGAN_NON_INTERACTIVE",
            value_parser = clap::builder::BoolishValueParser::new(),
        )]
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
    /// `moagan coverage` — show the SanCov runtime coverage
    /// report for one run. ADR-0002. The `text` sub-subcommand
    /// always works (no external tool required); the `html`
    /// sub-subcommand shells out to `grcov`.
    Coverage {
        /// Subcommand (`show`).
        #[command(subcommand)]
        sub: coverage_cmd::CoverageCmd,
    },
    /// `moagan pause <run_id>` — serialise current run state to
    /// `<run_dir>/paused.json` and stamp a `paused.lock` with TTL
    /// 5 min. Track K.2b (catalog §D.22.5).
    Pause {
        /// Run id to pause (UUID v7).
        #[arg(value_name = "RUN_ID")]
        run_id: String,
    },
    /// `moagan list --paused` — enumerate every run directory under
    /// `<home>/.runs/` that carries a `paused.json`. Track K.2b
    /// (catalog §D.22.5).
    List {
        /// Filter to paused runs (the only kind the v0.4 pause
        /// surface understands today; non-paused listing lives on
        /// `moagan inspect`).
        #[arg(long, default_value_t = false)]
        paused: bool,
    },
    /// `moagan rate <run_id> <proposal_id> <score>` — record a
    /// user-driven rating for a proposal. PR C.5 (K.3b). No-op
    /// when `MOAGAN_LEARNING` is unset.
    Rate {
        /// Run id (UUID v7) that produced the proposal.
        #[arg(value_name = "RUN_ID")]
        run_id: String,
        /// Proposal id (e.g. `p_001` or `s_001`).
        #[arg(value_name = "PROPOSAL_ID")]
        proposal_id: String,
        /// Score in `[0.0, 1.0]`. `0.0` = worst, `1.0` = best.
        #[arg(value_name = "SCORE")]
        score: String,
    },
}

/// Inputs for `rate::run`. Mirrors the positional args on
/// `Cmd::Rate { .. }` but stays as a `String`/`String`/`String`
/// shape so the dispatcher can construct it from clap's parsed
/// output without losing the parse-failure semantics of clap.
#[derive(Debug, Clone)]
pub struct RateArgs {
    /// Run id (UUID v7).
    pub run_id: String,
    /// Proposal id.
    pub proposal_id: String,
    /// Score in `[0.0, 1.0]`, kept as a string so the dispatcher
    /// can surface the original value in error messages.
    pub score: String,
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
    let v = matches!(
        cmd,
        Cmd::Run { .. }
            | Cmd::Continue { .. }
            | Cmd::Resume { .. }
            | Cmd::Rerun { .. }
            | Cmd::Import { .. }
            | Cmd::Refine { .. }
            | Cmd::Rerank { .. }
            | Cmd::Discover { .. }
    );
    trace!(reconcile = v, "should_reconcile_at_startup");
    v
}

/// Track F (D.28.3 + D.28.4): the actual startup reconcile call.
/// Extracted from [`dispatch_inner`] so unit tests can exercise
/// the disabled / enabled / sweep paths without re-entering the
/// CLI parser. Returns `Some(report)` when the reconcile ran and
/// `None` when [`Config::startup_reconcile`] is `false`. Errors
/// from the underlying helpers bubble up via `?` so a missing
/// meta.sqlite or a poisoned `MOAGAN_HOME` still surfaces to the
/// dispatcher.
fn run_startup_reconcile(
    home: &MoaganHome,
    cfg: &Config,
) -> Result<Option<crate::reconcile::StartupReconcileReport>> {
    if !cfg.startup_reconcile {
        debug!("run_startup_reconcile: skipped (startup_reconcile=false)");
        return Ok(None);
    }
    debug!(
        home = %home.root().display(),
        "run_startup_reconcile: opening meta.sqlite"
    );
    let db = Db::open(&home.meta_db_path())?;
    let report = crate::reconcile::startup_reconcile(home, &db, cfg)?;
    info!(
        orphans_removed = report.orphans_removed,
        zombies_recovered = report.zombies_recovered,
        "startup reconcile done"
    );
    Ok(Some(report))
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
            Self::Doctor { .. } => "Check the local environment",
            Self::Probe { .. } => {
                "Operator-driven diagnostics (probe max_tokens, probe temperature)"
            }
            Self::Audit { .. } => "External, transparent audit trail",
            Self::Discover { .. } => "Discovery mode (knowledge base by category)",
            Self::Telemetry { .. } => "Inspect, export, and serve telemetry dashboards",
            Self::Coverage { .. } => "Show the runtime coverage report for one run (ADR-0002)",
            Self::Validate { .. } => "Validate a brief without running the pipeline",
            Self::Diff { .. } => "Compare two runs side-by-side (params, artefacts, scores)",
            Self::Repair { .. } => "Reconcile filesystem vs SQLite",
            Self::Pause { .. } => {
                "Serialise current run state to paused.json (cross-process hibernation)"
            }
            Self::List { .. } => "List runs (filter by --paused)",
            Self::Rate { .. } => "Record a user rating for a proposal (opt-in learning loop)",
            Self::Preflight { .. } => {
                "Smoke-test the END-TO-END pipeline (discover + run --mode fast) against the real provider. Runs `discover` with cardinalidad 8 + 1 temp + 1 replica, then feeds the discover run's library into a `run --mode fast` via `--context <discover_run_id> --context-full`. Both run ids are printed on stdout."
            }
        }
    }
}

/// Dispatch the parsed CLI with a pre-allocated `run_id` and
/// convert domain errors into process exit codes. The returned
/// [`DispatchResult`] carries the real `run_id` produced /
/// referenced by the subcommand so the outer
/// [`crate::run_with_cli`] can stamp it onto the `pipeline_span`
/// and the
/// [`crate::telemetry::stdout_events::Event::RunStart`] /
/// [`crate::telemetry::stdout_events::Event::RunEnd`] events
/// instead of the historic `"pre-dispatch"` /
/// `<help-or-readonly>` / `<n/a>` placeholders.
///
/// Why the pre-allocated id matters (v0.11.2 audit fix):
/// `pipeline_span` is constructed in `run_with_cli` with
/// `run_id = %run_id` BEFORE dispatch runs. Every event emitted
/// DURING dispatch inherits the span context — so the run_id
/// must be the SAME one the dispatcher uses downstream. Arms
/// that produce a new run (Run / Discover / Preflight /
/// Continue-fork) USE this id instead of generating their own;
/// arms that operate on an existing run (Resume / Rerun / Refine
/// / Rerank / Continue --from-pause) keep the id the operator
/// passed on the CLI (which is what the outer pre-parse
/// computed). Read-only arms ignore the param.
pub async fn dispatch_with_run_id(cli: Cli, run_id: crate::ids::RunId) -> Result<DispatchResult> {
    trace!(
        candidate_run_id = %run_id,
        "dispatch_with_run_id: enter"
    );
    let result = dispatch_inner(cli, run_id).await;
    match &result {
        Ok(dr) => {
            info!(
                exit_code = dr.exit_code,
                run_id = ?dr.run_id.as_ref().map(|r| r.short()),
                mode = ?dr.mode,
                "dispatch: ok"
            );
            Ok(dr.clone())
        }
        Err(e) => {
            error!(error = %e, exit_code = e.exit_code() as i32, "dispatch: error");
            // PR-04a (E-1) stream routing flip: the structured
            // `tracing::error!` above is now the SOLE machine-facing
            // surface for the dispatch error. v0.11 emitted a
            // duplicate `eprintln!("error: {e}")` when stderr was
            // a TTY, which masked the JSON line for TTY users who
            // piped stderr into a downstream tool. With the v0.12.0
            // routing flip, the JSON `tracing::error!` line is
            // always emitted on stderr (uniquely so — that's the
            // whole point of the flip) and the redundant plain-text
            // fallback is gone. TTY users still get a readable line
            // because `fmt::layer().text()` renders the JSON
            // event as `ERROR … error: …` automatically.
            Ok(DispatchResult {
                exit_code: e.exit_code() as i32,
                run_id: None,
                mode: None,
                provider: None,
                model: None,
                prompt_hash: None,
            })
        }
    }
}

/// Convenience wrapper for callers (integration tests, ad-hoc
/// CLI invocations) that do not carry a pre-allocated `run_id`.
/// Allocates a fresh UUIDv7 internally so the inner
/// `dispatch_with_run_id` always receives one. The
/// `pipeline_span` built by the outer `run_with_cli` will carry
/// the SAME fresh id, so events emitted during dispatch and the
/// `Event::RunStart.run_id` stay byte-identical.
pub async fn dispatch(cli: Cli) -> Result<DispatchResult> {
    dispatch_with_run_id(cli, crate::ids::RunId::new()).await
}

/// Result of dispatching a parsed [`Cli`]. Carries the real
/// `run_id` produced (or referenced) by the subcommand so the
/// outer [`crate::run_with_cli`] can stamp it onto the
/// `pipeline_span` and the
/// [`crate::telemetry::stdout_events::Event::RunStart`] /
/// [`crate::telemetry::stdout_events::Event::RunEnd`] events
/// instead of the historic `"pre-dispatch"` /
/// `<help-or-readonly>` / `<n/a>` placeholders.
///
/// `run_id` is `Some(_)` whenever the subcommand produced or
/// selected a concrete run — the run that was created (Run,
/// Discover, Preflight's discover leg), the run that was resumed
/// (Continue / Resume / Rerun), the run that was refined / reranked
/// (Refine, Rerank), or the run that was imported (Import). For
/// read-only commands (Inspect, Diff, Doctor, Validate, Telemetry,
/// Pause, List, Coverage show, Repair) and side-effect-only
/// commands (Probe, Audit proxy/verify, Rate) the field is `None`;
/// `run_with_cli` substitutes a `<read-only>` sentinel for the
/// machine-facing event so `jq -c 'select(.kind=="run_start")'`
/// keeps matching.
#[derive(Debug, Clone)]
pub struct DispatchResult {
    /// Process exit code (0 = success).
    pub exit_code: i32,
    /// Real run id produced / referenced by the subcommand, or
    /// `None` for read-only commands.
    pub run_id: Option<crate::ids::RunId>,
    /// Stable mode label (`"run"`, `"discover"`, `"continue"`,
    /// `"resume"`, `"rerun"`, `"refine"`, `"rerank"`, `"import"`,
    /// `"preflight"`, `"audit"`, …). Replaces the
    /// `<help-or-readonly>` placeholder.
    pub mode: Option<&'static str>,
    /// Resolved provider (the full `SECTION:MODEL` string), or
    /// `None` when no LLM-backed command ran.
    pub provider: Option<String>,
    /// Resolved model id (the suffix after `SECTION:`), or
    /// `None` when no LLM-backed command ran.
    pub model: Option<String>,
    /// BLAKE3 hex digest of the resolved prompt. `None` for
    /// subcommands that do not consume a prompt (read-only
    /// commands; rerun/continue/resume use the parent run's
    /// prompt so they inherit the parent's hash via the
    /// manifest, not the dispatcher).
    pub prompt_hash: Option<String>,
}

impl DispatchResult {
    /// Build a `DispatchResult` for a read-only subcommand that
    /// does not touch the pipeline.
    fn read_only(exit_code: i32, mode: &'static str) -> Self {
        Self {
            exit_code,
            run_id: None,
            mode: Some(mode),
            provider: None,
            model: None,
            prompt_hash: None,
        }
    }

    /// Build a `DispatchResult` for an LLM-backed subcommand
    /// that produced / referenced a single run.
    fn with_run(
        exit_code: i32,
        mode: &'static str,
        run_id: crate::ids::RunId,
        provider: Option<String>,
        model: Option<String>,
        prompt_hash: Option<String>,
    ) -> Self {
        Self {
            exit_code,
            run_id: Some(run_id),
            mode: Some(mode),
            provider,
            model,
            prompt_hash,
        }
    }
}

async fn dispatch_inner(cli: Cli, run_id: crate::ids::RunId) -> Result<DispatchResult> {
    debug!(candidate_run_id = %run_id, "dispatch_inner: enter");
    // Run the hard-incompatibilities guard on every entry.
    forbidden::check_local_cargo_toml()?;
    trace!("dispatch_inner: forbidden-crates guard passed");
    // v0.11 removed the `--logs <PATH>` flag and `MOAGAN_RUN_LOGS`
    // env var. Stderr is the single source of tracing events; the
    // operator captures it with POSIX redirection
    // (`moagan … 2> log.jsonl`). See ADR-0002 §B.
    let global_home = match cli.runs_dir {
        Some(path) => {
            trace!(path = %path.display(), "dispatch_inner: explicit runs_dir");
            MoaganHome::at(path)
        }
        None => {
            let h = MoaganHome::resolve()?;
            trace!(home = %h.root().display(), "dispatch_inner: resolved MOAGAN_HOME");
            h
        }
    };
    // Track F (D.28.3 + D.28.4): reconcile filesystem vs SQLite at
    // the top of every pipeline-opening dispatch. Only commands that
    // actually open the pipeline pay the cost; the read-only ones
    // (`moagan inspect`, `moagan diff`, `moagan doctor`,
    // `moagan telemetry`, `moagan validate`) skip the boot pass so
    // their exit stays deterministic and latency-free. The
    // `Config::startup_reconcile` flag (default `true`) and
    // `MOAGAN_STARTUP_RECONCILE=false` env var gate the call.
    //
    // The actual reconcile call lives in
    // [`run_startup_reconcile`] so unit tests can drive the
    // disabled / enabled / sweep paths without going through the
    // full `Config::load()` + `Cli::parse` plumbing.
    if should_reconcile_at_startup(&cli.cmd) {
        debug!("dispatch_inner: pipeline-opening command — running startup reconcile");
        let cfg = Config::load()?;
        run_startup_reconcile(&global_home, &cfg)?;
    } else {
        trace!("dispatch_inner: read-only command — skipping startup reconcile");
    }
    info!(cmd = ?cli.cmd, "dispatch: routing command");
    match cli.cmd {
        Cmd::Run {
            mode,
            provider,
            mut prompt,
            runs_dir,
            mock_dir,
            non_interactive,
            max_parallelism,
            no_replace_sources,
            adversary,
            context,
            context_summary,
            context_full,
            model: _model,
            allow_injection,
            profile,
            hash_algo,
        } => {
            // v0.10 (Phase 5): the `--model` flag is gone. The
            // operator must now pass `--provider PROVIDER:MODEL`
            // (or `--provider PROVIDER --model MODEL`). The flag
            // is kept on the struct for backwards-compat parsing
            // (so old scripts fail with a clear error message)
            // but ignored here.
            if let Some(m) = _model.as_deref()
                && !m.trim().is_empty()
            {
                warn!(model = m, "deprecated --model flag used");
                return Err(Error::InvalidArgs(
                    "--model is no longer a separate flag; pass the model id as part of \
                     --provider (e.g. --provider opencode:kimi-k3 or \
                     --provider opencode --model kimi-k3)"
                        .into(),
                ));
            }
            // v0.10: validate `--provider` is in the new
            // PROVIDER:MODEL shape so the operator sees a
            // friendly error before any I/O happens.
            if let Some(provider) = provider.as_deref()
                && !provider.is_empty()
                && !provider.contains(':')
            {
                // Either it is the legacy `--provider <section>`
                // (the CLI accepts it but the section must be a
                // one-model alias) or it is a typo. We surface
                // a clear message so the operator knows what
                // shape we expect.
                warn!(
                    provider = provider,
                    "bare SECTION provider without MODEL id"
                );
                let cfg_early = Config::load().unwrap_or_default();
                if let Some(spec) = cfg_early.providers_legacy.get(provider)
                    && spec.models.len() > 1
                {
                    return Err(Error::InvalidArgs(format!(
                        "--provider '{provider}' now requires a model id (this section has \
                         {} models); pass `--provider {provider}:MODEL` instead, or pick a \
                         single-model alias (one of [{}])",
                        spec.models.len(),
                        spec.models
                            .iter()
                            .map(|m| m.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    )));
                }
            }
            // Phase J: validate the `--context-{summary,full}` flags
            // are only useful with `--context`. Setting them without
            // `--context` is a silent no-op today; we surface the
            // mistake as `Error::InvalidArgs` so the operator does
            // not debug a "missing context" run.
            if (context_summary || context_full) && context.is_none() {
                warn!(
                    context_summary = context_summary,
                    context_full = context_full,
                    "--context-summary/--context-full without --context"
                );
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
            // `--hash-algo <blake3|sha256>` overrides
            // `Config::export.hash_algo`. The CLI flag wins on
            // conflict with the env var (`MOAGAN_HASH_ALGO`) so
            // the operator gets the explicit choice they typed.
            // Empty / whitespace values are ignored so a stray
            // `--hash-algo ""` does not silently corrupt the
            // config.
            if let Some(raw) = hash_algo.as_deref()
                && !raw.trim().is_empty()
            {
                if let Ok(parsed) = raw.trim().parse::<crate::cli::flags_batch::HashAlgo>() {
                    cfg.export.hash_algo = parsed;
                } else {
                    return Err(Error::InvalidArgs(format!(
                        "--hash-algo: invalid value '{raw}' (expected blake3 or sha256)"
                    )));
                }
            }
            // Track K (catalog §D.21): `--profile <name>` loads a
            // domain-specific profile from
            // `$MOAGAN_HOME/profiles/<name>.toml` (with the
            // `~/.config/moagan/profiles/` fallback) and applies
            // it on top of the loaded `Config`. Applied AFTER
            // env overrides so the CLI wins on conflict, but the
            // operator can also resolve the profile entirely via
            // env vars by leaving the flag empty. Whitespace-only
            // values are ignored to keep the flag ergonomic.
            if let Some(name) = profile.as_deref()
                && !name.trim().is_empty()
            {
                debug!(profile = name, "applying CLI profile override");
                let profile = Config::load_profile(name.trim())?;
                cfg.apply_profile(&profile);
                trace!(profile = name, "profile applied");
            }
            // Q5: `--model <name>` overrides the model on the resolved
            // provider. Applied AFTER `apply_env_overrides()` (which
            // runs inside `Config::load()`) so the CLI wins on conflict
            // with `MOAGAN_MINIMAX_MODEL`. The override mutates the
            // provider spec so every phase that reads
            // `cfg.provider(name).model` (including the manifest stub
            // and `RunContext::default_model`) sees the new value
            // without any further plumbing.
            //
            // v0.10: `--model` is now part of `--provider PROVIDER:MODEL`.
            // The flag is still parsed for backwards compatibility
            // (the error above surfaces a friendly message), but
            // no alias-resolution happens here.
            // D.14.7: `--prompt -` reads the prompt body from stdin
            // instead of treating the literal string as the prompt.
            // The substitution happens here, BEFORE `RunOptions` is
            // constructed, so the pipeline sees the resolved prompt
            // through `opts.prompt` (manifest.cli_prompt, intake
            // raw_prompt, etc.) and every downstream consumer
            // (cache keys, redaction, intake normalisation) picks
            // up the same string.
            if flags_batch::prompt_is_stdin(&prompt) {
                trace!("reading prompt body from stdin (-)");
                prompt = flags_batch::read_prompt_from_stdin()?;
                debug!(prompt_len = prompt.len(), "prompt read from stdin");
            }
            // v0.10: `--provider` is optional at the clap level so we can
            // produce a friendly error message that includes the
            // resolution order. Resolve `Option<String> -> String`
            // here so `RunOptions::provider` (still `String` for
            // back-compat with the rest of the pipeline) gets the
            // final value. The empty-string check below is a no-op
            // for the operator — `--provider ""` is rejected at the
            // `--model`-presence check above — but guards against a
            // missing flag slipping through.
            let provider = match provider.as_deref() {
                Some(s) if !s.trim().is_empty() => s.to_owned(),
                _ => cfg.default_provider.clone(),
            };
            if provider.trim().is_empty() {
                warn!(
                    "--provider missing; no CLI flag, no MOAGAN_DEFAULT_PROVIDER, no config default"
                );
                return Err(Error::InvalidArgs(
                    "--provider is required (or set MOAGAN_DEFAULT_PROVIDER, \
                     or [defaults] provider in config.toml); example:\n  \
                     moagan run --provider opencode:kimi-k3 --prompt \"...\"\n  \
                     moagan run --provider opencode --model kimi-k3 --prompt \"...\""
                        .into(),
                ));
            }
            // Snapshot the resolved prompt + provider so the
            // outer `run_with_cli` can stamp real values onto the
            // `pipeline_span` / `Event::RunStart` / `RunEnd`
            // payloads. The pipeline moves both into `RunOptions`,
            // so the snapshot is captured here (a `String::clone`
            // for the provider; a fresh `blake3` of the prompt).
            let resolved_provider = provider.clone();
            let resolved_prompt_hash = crate::ids::blake3_hex(prompt.as_bytes());
            let resolved_model = probe::parse_provider_model(&provider).ok().map(|(_, m)| m);
            debug!(
                provider = %provider,
                mode = ?mode,
                "dispatching moagan run"
            );
            let new_run_id = run::run(
                run::RunOptions {
                    mode,
                    provider,
                    prompt,
                    home: Some(runs_dir.unwrap_or_else(|| global_home.root().to_path_buf())),
                    mock_dir,
                    non_interactive,
                    max_parallelism,
                    no_replace_sources,
                    adversary,
                    context,
                    context_scope: scope,
                },
                &cfg,
                run_id,
            )
            .await?;
            // The `run id: <uuid>` footer is a human affordance.
            // When stdout is being consumed as NDJSON (i.e. it is
            // not a TTY), writing this line corrupts the stream
            // and breaks `moagan … | jq`. The matching
            // `Event::RunEnd` already carries `run_id` for machine
            // consumers. Only print on a TTY. We print the id the
            // pipeline ACTUALLY used (`new_run_id`) — it is
            // guaranteed to match the value `run_with_cli`
            // pre-allocated and stamped on `pipeline_span`.
            if std::io::stdout().is_terminal() {
                println!("run id: {new_run_id}");
            }
            Ok(DispatchResult::with_run(
                0,
                "run",
                new_run_id,
                Some(resolved_provider),
                resolved_model,
                Some(resolved_prompt_hash),
            ))
        }
        Cmd::Continue {
            run_id,
            from_pause,
            kind,
            switch_provider,
            switch_api_key,
            skip_checkpoint,
            non_interactive,
        } => {
            debug!(
                from_pause = from_pause,
                kind = ?kind,
                skip_checkpoint = skip_checkpoint,
                "dispatching moagan continue"
            );
            // Track K.2b: `--from-pause` short-circuits to the
            // pause-aware resume path. PR C.3 only logs the resume
            // plan; the real loop skip that uses `paused.json` lands
            // in PR C.5 (K.2 wires `continue_cmd.rs`). Until then,
            // `--from-pause` is a no-op-ish probe that confirms the
            // file is present and well-formed.
            //
            // v0.5 PR-24: `--kind discovery` forces a discovery
            // resume even though the run was registered as a
            // discovery run (manifest.mode = "discover"). Without
            // this, the linear `parse_mode` rejects `"discover"`
            // and the resume fails with `unknown mode "discover"`.
            // `--from-pause` does not accept `--kind`; the pause
            // path always uses the linear pipeline because the
            // pause file records phase names from the linear list.
            if from_pause {
                let id = run_id.ok_or_else(|| {
                    Error::InvalidArgs(
                        "--run-id is required for `moagan continue --from-pause`".into(),
                    )
                })?;
                let parsed = id.parse().map_err(|e| Error::InvalidArgs(format!("{e}")))?;
                let code = pause_cmd::run_continue_from_pause(&global_home, parsed).await?;
                return Ok(DispatchResult::with_run(
                    code, "continue", parsed, None, None, None,
                ));
            }
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
                    kind: kind.into(),
                },
            )
            .await?;
            Ok(DispatchResult::with_run(
                0, "continue", parsed, None, None, None,
            ))
        }
        Cmd::Resume {
            run_id,
            non_interactive,
        } => {
            let parsed: crate::ids::RunId = run_id
                .parse()
                .map_err(|e| Error::InvalidArgs(format!("{e}")))?;
            debug!(run_id = %parsed, "dispatching moagan resume");
            continue_cmd::run_resume(&global_home, parsed, non_interactive).await?;
            Ok(DispatchResult::with_run(
                0, "resume", parsed, None, None, None,
            ))
        }
        Cmd::Rerun {
            run_id,
            override_json,
            matrix_override,
            same_config,
        } => {
            let parsed: crate::ids::RunId = run_id
                .parse()
                .map_err(|e| Error::InvalidArgs(format!("{e}")))?;
            // `--override-json` and `--matrix-override` are aliases;
            // if both are set, prefer `--matrix-override` (the
            // spec-blessed name).
            let raw = matrix_override.or(override_json);
            // `--same-config` is now wired through to the rerun
            // helper instead of being destructured with `_: ` and
            // discarded (PR-B1). When `true` (default) the
            // helper treats the parent's config as immutable and
            // folds `--matrix-override` on top; `--same-config=false`
            // suppresses the override entirely.
            tracing::info!(
                run_id = %parsed,
                same_config,
                has_override = raw.is_some(),
                "rerun dispatch"
            );
            continue_cmd::run_rerun(&global_home, parsed, raw, same_config).await?;
            // Per the c1 plan: `Rerun` reports the input run id
            // (the parent that was cloned). The forked child id
            // is owned by `run_rerun` (it ends up on
            // `manifest.run_id` of the new run dir); surfacing
            // it here would require plumbing through the
            // helper's return value, which the plan deferred.
            Ok(DispatchResult::with_run(
                0, "rerun", parsed, None, None, None,
            ))
        }
        Cmd::Import {
            source_path,
            target_runs_dir,
        } => {
            debug!(
                source = %source_path.display(),
                target = ?target_runs_dir.as_ref().map(|p| p.display().to_string()),
                "dispatching moagan import"
            );
            // The `run_import` helper validates the source
            // manifest and stamps the imported run on the local
            // home. To propagate the imported run id through the
            // dispatcher we re-read the manifest: the file
            // already exists at `source_path/manifest.json`
            // (validated inside `run_import` before this
            // dispatcher call would return `Ok(_)`).
            let manifest_bytes = std::fs::read(source_path.join("manifest.json")).map_err(|e| {
                Error::InvalidState(format!(
                    "import: source manifest disappeared during dispatch: {e}"
                ))
            })?;
            #[derive(serde::Deserialize)]
            struct ManifestStub {
                run_id: crate::ids::RunId,
            }
            let stub: ManifestStub = serde_json::from_slice(&manifest_bytes)
                .map_err(|e| Error::InvalidState(format!("import: manifest malformed: {e}")))?;
            let imported_run_id = stub.run_id;
            continue_cmd::run_import(&global_home, &source_path, target_runs_dir.as_deref())?;
            Ok(DispatchResult::with_run(
                0,
                "import",
                imported_run_id,
                None,
                None,
                None,
            ))
        }
        Cmd::Inspect {
            limit,
            run_id,
            verbose,
            capabilities,
        } => {
            debug!(
                limit = limit,
                run_id = ?run_id.as_deref(),
                capabilities = capabilities,
                "dispatching moagan inspect"
            );
            let home = &global_home;
            let db = Db::open(&home.meta_db_path())?;
            if let Some(id) = run_id {
                let parsed = id.parse().map_err(|e| Error::InvalidArgs(format!("{e}")))?;
                if capabilities {
                    inspect::print_run_capabilities(&global_home, parsed)?;
                } else {
                    match inspect::summarize_run(&db, parsed)? {
                        Some(summary) => {
                            inspect::print_run_summary(&summary, verbose);
                        }
                        None => {
                            warn!(run_id = %parsed, "inspect: run not in index");
                            return Err(Error::InvalidState(format!(
                                "run {id} not found in the index"
                            )));
                        }
                    }
                }
            } else {
                let entries = inspect::list_recent(&db, limit)?;
                trace!(entries = entries.len(), "inspect: list_recent");
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
            Ok(DispatchResult::read_only(0, "inspect"))
        }
        Cmd::Refine {
            run_id,
            proposal,
            action,
            verdict_detail,
            mock_dir,
        } => {
            debug!(
                run_id = %run_id,
                action = ?action.as_ref().map(|a| a.as_cli_str()),
                "dispatching moagan refine"
            );
            let home = Arc::new(global_home.clone());
            let run_id_parsed = run_id
                .parse()
                .map_err(|e| Error::InvalidArgs(format!("{e}")))?;

            if let Some(action) = action {
                // D.22.2: invoke `dispatch_refine_action`.
                let outcome =
                    continue_cmd::run_refine_action(run_id_parsed, action, verdict_detail, &home)
                        .await?;
                match outcome.prohibited_decisions {
                    Some(pds) if !pds.is_empty() => {
                        println!(
                            "refine action '{}' applied to run {run_id}; prohibited_decisions now [{}]",
                            action.as_cli_str(),
                            pds.join(", ")
                        );
                    }
                    _ => {
                        println!(
                            "refine action '{}' applied to run {run_id}",
                            action.as_cli_str()
                        );
                    }
                }
                if outcome.emitted_telemetry {
                    println!("refine action emitted a StaleArtifact telemetry event");
                }
                Ok(DispatchResult::with_run(
                    0,
                    "refine",
                    run_id_parsed,
                    None,
                    None,
                    None,
                ))
            } else if let Some(proposal) = proposal.as_deref() {
                let cfg = Config::load()?;
                continue_cmd::run_refine(run_id_parsed, proposal, &cfg, &home, mock_dir.as_deref())
                    .await?;
                println!("refined proposal {proposal} for run {run_id}");
                Ok(DispatchResult::with_run(
                    0,
                    "refine",
                    run_id_parsed,
                    None,
                    None,
                    None,
                ))
            } else {
                // clap's `required_unless_present` should prevent
                // this; surface a friendly error anyway so a
                // programmatic caller (not clap) gets a clear
                // message.
                Err(Error::InvalidArgs(
                    "refine requires either --proposal <id> or --action <action>".into(),
                ))
            }
        }
        Cmd::Rerank { run_id } => {
            debug!(run_id = %run_id, "dispatching moagan rerank");
            let cfg = Config::load()?;
            let home = Arc::new(global_home.clone());
            let parsed = run_id
                .parse()
                .map_err(|e| Error::InvalidArgs(format!("{e}")))?;
            continue_cmd::run_rerank(parsed, &cfg, &home).await?;
            println!("reranked run {run_id}");
            Ok(DispatchResult::with_run(
                0, "rerank", parsed, None, None, None,
            ))
        }
        Cmd::Doctor { capabilities } => {
            let code = doctor::run(capabilities)?;
            Ok(DispatchResult::read_only(code, "doctor"))
        }
        Cmd::Probe { sub } => {
            let code = probe::dispatch(&sub).await?;
            Ok(DispatchResult::read_only(code, "probe"))
        }
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
                debug!(
                    upstream = %upstream,
                    port = port,
                    "dispatching audit proxy"
                );
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
                Ok(DispatchResult::read_only(0, "audit_proxy"))
            }
            AuditCmd::Verify { runs_dir, run_id } => {
                debug!(
                    run_id = ?run_id.as_deref(),
                    "dispatching audit verify"
                );
                let args = audit::VerifyArgs { runs_dir, run_id };
                let code = audit::verify_cmd(args).await?;
                Ok(DispatchResult::read_only(code, "audit_verify"))
            }
        },
        Cmd::Discover {
            provider,
            prompt,
            runs_dir,
            mock_dir,
            sketches_per_cell,
            max_parallelism,
            dimensions,
            facets_per_dimension,
            matrix_spec,
            llm_derive,
            cluster_threshold,
            non_interactive,
            cache_facets,
            temperature_profiles,
            explain,
        } => {
            if sketches_per_cell < MIN_SKETCHES_PER_CELL {
                warn!(
                    sketches_per_cell = sketches_per_cell,
                    min = MIN_SKETCHES_PER_CELL,
                    "sketches-per-cell below floor"
                );
                return Err(Error::InvalidArgs(format!(
                    "sketches-per-cell {sketches_per_cell} below the minimum of {MIN_SKETCHES_PER_CELL}"
                )));
            }
            // F1: `--facets-per-dimension` only makes sense when the
            // operator is opting into the LLM-derive path AND has a
            // target dimension count. Without `--matrix-spec` and
            // without `--llm-derive`, the LLM picks facets
            // asymmetrically per dimension; honouring a rigid
            // `--facets-per-dimension` flag in that path is a
            // contradiction, so reject it cleanly.
            if facets_per_dimension.is_some()
                && matrix_spec.iter().all(|s| s.trim().is_empty())
                && !llm_derive
                && dimensions.is_none()
            {
                warn!("--facets-per-dimension without --matrix-spec/--llm-derive/--dimensions");
                return Err(Error::InvalidArgs(
                    "facets-per-dimension requires an explicit --matrix-spec; \
                     without one facets are derived per-dimension by the LLM"
                        .to_string(),
                ));
            }
            // PR-D1: parse the CLI `--temperature-profile` specs into
            // a typed `Vec<TemperatureProfileSpec>`. Each spec is
            // validated (provider non-empty, every temperature in
            // `0.0..=2.0`, replicas >= 1) so a malformed flag surfaces
            // as `Error::InvalidArgs` instead of silently collapsing
            // the matrix to the default profile.
            let parsed_profiles = temperature_profiles
                .iter()
                .map(|s| discover::TemperatureProfileSpec::parse(s))
                .collect::<std::result::Result<Vec<_>, Error>>()?;
            let cfg = Config::load()?;
            // F3 (Track G.2): `--explain` short-circuits BEFORE
            // `discover::run` so the pipeline is never invoked.
            // `discover::run` so the pipeline is never invoked.
            // The dispatcher prints the formatted table to
            // stdout and exits 0; no `run_id` is allocated, no
            // run dir is created, the LLM is never called.
            //
            // The plan suggests wiring this inside
            // `discover::run`, but placing the short-circuit at
            // the dispatcher boundary keeps the explain path
            // entirely outside the pipeline (the dispatcher
            // already loads `cfg`, parses the profiles, and
            // owns the "discovery run id: ..." print — so
            // short-circuiting here avoids printing a fake
            // `discovery run id: 00000000-...` placeholder).
            if explain {
                debug!("discover: --explain short-circuit");
                // v0.10: --provider is mandatory; resolve the
                // optional flag into a concrete value using the
                // same precedence as `run`.
                let explain_provider = match provider.as_deref() {
                    Some(s) if !s.trim().is_empty() => s.to_owned(),
                    _ => cfg.default_provider.clone(),
                };
                if explain_provider.trim().is_empty() {
                    warn!("discover --explain: --provider missing");
                    return Err(Error::InvalidArgs(
                        "--provider is required (or set MOAGAN_DEFAULT_PROVIDER, \
                         or [defaults] provider in config.toml)"
                            .into(),
                    ));
                }
                let explain_opts = discover::DiscoverOptions {
                    provider: explain_provider,
                    prompt: prompt.clone(),
                    home: Some(runs_dir.unwrap_or_else(|| global_home.root().to_path_buf())),
                    mock_dir: mock_dir.clone(),
                    sketches_per_cell,
                    max_parallelism,
                    dimensions,
                    facets_per_dimension,
                    matrix_spec: matrix_spec.clone(),
                    llm_derive,
                    cluster_threshold,
                    out_dir: None,
                    non_interactive,
                    cache_facets,
                    temperature_profiles: parsed_profiles,
                    explain: true,
                };
                let rendered = discover_explain::build_and_format(&explain_opts, &cfg)?;
                println!("{rendered}");
                return Ok(DispatchResult::read_only(0, "discover_explain"));
            }
            // v0.10: --provider is mandatory; resolve it through
            // the same precedence as `run` so the operator sees
            // one canonical error message.
            let provider = match provider.as_deref() {
                Some(s) if !s.trim().is_empty() => s.to_owned(),
                _ => cfg.default_provider.clone(),
            };
            if provider.trim().is_empty() {
                warn!("discover: --provider missing");
                return Err(Error::InvalidArgs(
                    "--provider is required (or set MOAGAN_DEFAULT_PROVIDER, \
                     or [defaults] provider in config.toml)"
                        .into(),
                ));
            }
            // Snapshot the resolved prompt + provider so the
            // outer `run_with_cli` can stamp real values onto
            // `pipeline_span` / `Event::RunStart` / `RunEnd`.
            let resolved_provider = provider.clone();
            let resolved_prompt_hash = crate::ids::blake3_hex(prompt.as_bytes());
            let resolved_model = probe::parse_provider_model(&provider).ok().map(|(_, m)| m);
            debug!(
                provider = %provider,
                sketches_per_cell = sketches_per_cell,
                "dispatching moagan discover"
            );
            let new_run_id = discover::run(
                discover::DiscoverOptions {
                    provider,
                    prompt,
                    home: Some(runs_dir.unwrap_or_else(|| global_home.root().to_path_buf())),
                    mock_dir,
                    sketches_per_cell,
                    max_parallelism,
                    dimensions,
                    facets_per_dimension,
                    matrix_spec,
                    llm_derive,
                    cluster_threshold,
                    out_dir: None,
                    non_interactive,
                    cache_facets,
                    temperature_profiles: parsed_profiles,
                    explain: false,
                },
                &cfg,
                run_id,
            )
            .await?;
            println!("discovery run id: {new_run_id}");
            Ok(DispatchResult::with_run(
                0,
                "discover",
                new_run_id,
                Some(resolved_provider),
                resolved_model,
                Some(resolved_prompt_hash),
            ))
        }
        Cmd::Telemetry { sub } => {
            let code = telemetry_cmd::TelemetryCmd::dispatch(sub).await?;
            Ok(DispatchResult::read_only(code, "telemetry"))
        }
        Cmd::Coverage { sub } => {
            debug!("dispatching moagan coverage");
            let rc = coverage_cmd::dispatch(&global_home, sub)?;
            Ok(DispatchResult::read_only(rc, "coverage"))
        }
        Cmd::Validate { brief_path, mode } => {
            debug!(
                brief_path = %brief_path.display(),
                "dispatching moagan validate"
            );
            let code = validate::run(validate::ValidateArgs { brief_path, mode })?;
            Ok(DispatchResult::read_only(code, "validate"))
        }
        Cmd::Diff {
            run_a,
            run_b,
            format,
            include_proposals,
        } => {
            debug!(
                run_a = %run_a,
                run_b = %run_b,
                "dispatching moagan diff"
            );
            let code = diff::run(diff::DiffArgs {
                run_a,
                run_b,
                format,
                include_proposals,
                home_override: None,
            })?;
            Ok(DispatchResult::read_only(code, "diff"))
        }
        Cmd::Repair {
            cleanup_orphans,
            reindex_artifacts,
            recover_zombies,
            yes,
            run,
            dry_run,
        } => {
            debug!(
                cleanup_orphans,
                reindex_artifacts,
                recover_zombies,
                yes,
                run = ?run.as_deref(),
                dry_run,
                "dispatching moagan repair"
            );
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
            Ok(DispatchResult::read_only(code, "repair"))
        }
        Cmd::Pause { run_id } => {
            let parsed: crate::ids::RunId = run_id
                .parse()
                .map_err(|e| Error::InvalidArgs(format!("invalid run id '{run_id}': {e}")))?;
            debug!(run_id = %parsed, "dispatching moagan pause");
            let code = pause_cmd::run_pause(
                &global_home,
                pause_cmd::PauseArgs {
                    run_id: parsed,
                    phase: None,
                    completed: None,
                },
            )?;
            Ok(DispatchResult::read_only(code, "pause"))
        }
        Cmd::Preflight {
            provider,
            prompt,
            runs_dir,
            mock_dir,
            max_parallelism,
            non_interactive,
        } => {
            // Preflight runs the FULL discovery → fast pipeline
            // end-to-end against the real provider, with a 8-sketch
            // matrix so the cost is bounded:
            //
            // 1. `moagan discover` with cardinalidad 8 (one sketch
            //    per dimension × facets_per_dimension = 1), single
            //    temperature (1.0), single replica. Produces a
            //    `run_id` with a persisted `sketches/` library.
            // 2. `moagan run --mode fast` that consumes the
            //    discover run's library via `--context <run_id>
            //    --context-full` (the `Full` scope loads
            //    every text-like file under the run dir, including
            //    `sketches/*.json`).
            //
            // Cost: ~30-60 s of API budget per step (~60-120 s
            // total), ~3 MB of disk per run. The two-step flow
            // exercises BOTH the discovery code path
            // (matrix build + coordinator) AND the run --mode fast
            // path (intake → clarify → sketch → judges → tag →
            // cluster → facets → portfolio), so a preflight that
            // succeeds is a strong end‑to‑end signal. A preflight
            // that fails tells the operator exactly which step
            // regressed because we print both run ids.
            //
            // The pre-PR-564 version only wrapped `discover`, which
            // missed the upstream intake/clarify phases. PR #565
            // wrapped `run --mode fast` only, which the operator
            // pointed out duplicates the existing fast-mode test
            // coverage. The two-step flow is the actual addition
            // because it links the two runs through `--context`,
            // validating the cross-run plumbing AND the per-run
            // planner.
            let cfg = Config::load()?;
            // `--runs-dir` is the path of the `.runs` directory
            // (the global_home.root()). The preflight derives the
            // parent from it so that the discover run lives at
            // `<runs-dir>/<id>` rather than the doubled
            // `<runs-dir>/.runs/<id>` path that `home.run_dir`
            // would otherwise produce.
            let home_root = runs_dir
                .clone()
                .map(|p| {
                    if p.ends_with(".runs") {
                        p.parent().map(|x| x.to_path_buf()).unwrap_or(p)
                    } else {
                        p
                    }
                })
                .unwrap_or_else(|| global_home.root().to_path_buf());
            // v0.10: --provider is mandatory; resolve it through
            // the same precedence as `run`.
            let provider = match provider.as_deref() {
                Some(s) if !s.trim().is_empty() => s.to_owned(),
                _ => cfg.default_provider.clone(),
            };
            if provider.trim().is_empty() {
                return Err(Error::InvalidArgs(
                    "--provider is required (or set MOAGAN_DEFAULT_PROVIDER, \
                     or [defaults] provider in config.toml)"
                        .into(),
                ));
            }
            // Snapshot the resolved prompt + provider so the
            // outer `run_with_cli` can stamp real values onto
            // `pipeline_span` / `Event::RunStart` / `RunEnd`.
            let resolved_provider = provider.clone();
            let resolved_prompt_hash = crate::ids::blake3_hex(prompt.as_bytes());
            let resolved_model = probe::parse_provider_model(&provider).ok().map(|(_, m)| m);
            let discover_run_id = discover::run(
                discover::DiscoverOptions {
                    provider: provider.clone(),
                    prompt: prompt.clone(),
                    home: Some(home_root.clone()),
                    mock_dir: mock_dir.clone(),
                    sketches_per_cell: 10,
                    max_parallelism,
                    dimensions: Some(8),
                    facets_per_dimension: Some(1),
                    matrix_spec: Vec::new(),
                    llm_derive: false,
                    cluster_threshold: 0.7,
                    out_dir: Some(home_root.join(".runs")),
                    non_interactive,
                    cache_facets: false,
                    temperature_profiles: vec![discover::TemperatureProfileSpec {
                        provider: "MiniMax-M3".to_owned(),
                        temperatures: vec![1.0],
                        replicas_per_temperature: 1,
                    }],
                    explain: false,
                },
                &cfg,
                run_id,
            )
            .await?;
            println!("preflight discover run_id: {discover_run_id}");

            // The fast leg gets a fresh UUIDv7 so its run directory
            // is independent of the discover run (the pre-PR-c1
            // contract: the fast run is a real sibling, not a child
            // manifest of the discover one). The discover leg
            // already consumed the pre-allocated candidate id.
            let fast_run_id = run::run(
                run::RunOptions {
                    mode: Mode::Fast,
                    provider,
                    prompt: prompt.clone(),
                    home: Some(home_root.clone()),
                    mock_dir: mock_dir.clone(),
                    non_interactive,
                    max_parallelism,
                    no_replace_sources: false,
                    adversary: false,
                    context: Some(discover_run_id.to_string()),
                    context_scope: crate::context::loader::ContextScope::Full,
                },
                &cfg,
                crate::ids::RunId::new(),
            )
            .await?;
            println!("preflight fast run_id: {fast_run_id}");
            // Per the c1 plan: Preflight reports the FIRST run
            // id produced (the discover leg). The fast leg's run
            // id is printed on stdout for human operators but
            // not propagated through the dispatcher — surfacing
            // it would require changing `run::run` (or piping
            // the second id through `PreflightCommand`); the
            // plan defers that wiring.
            Ok(DispatchResult::with_run(
                0,
                "preflight",
                discover_run_id,
                Some(resolved_provider),
                resolved_model,
                Some(resolved_prompt_hash),
            ))
        }
        Cmd::List { paused } => {
            if !paused {
                return Err(Error::InvalidArgs(
                    "`moagan list` today only supports `--paused`; use `moagan inspect` for the full listing".into(),
                ));
            }
            debug!("dispatching moagan list --paused");
            let code = pause_cmd::run_list(&global_home, pause_cmd::ListArgs {})?;
            Ok(DispatchResult::read_only(code, "list"))
        }
        Cmd::Rate {
            run_id,
            proposal_id,
            score,
        } => {
            debug!(
                run_id = %run_id,
                proposal_id = %proposal_id,
                "dispatching moagan rate"
            );
            let code = rate::run(RateArgs {
                run_id,
                proposal_id,
                score,
            })?;
            Ok(DispatchResult::read_only(code, "rate"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_EVENT_FORMAT_LOCK;
    use crate::TEST_LOG_TO_STDERR_LOCK;
    use crate::TEST_MOAGAN_HOME_LOCK;

    /// Allocate a fresh `(TempDir, path)` pair.
    ///
    /// Returning the [`tempfile::TempDir`] alongside the path
    /// forces the caller to bind it so the directory outlives the
    /// test scope (and is auto-removed by `Drop` on success or
    /// panic — no `/tmp/moagan-*` leak).
    fn unique_tmp(label: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix(&format!("moagan-cli-{label}-"))
            .tempdir()
            .expect("tmp dir");
        let path = tmp.path().to_path_buf();
        (tmp, path)
    }

    fn lock_home_env(tmp: &std::path::Path) -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp);
        }
        guard
    }

    fn unlock_home_env(guard: std::sync::MutexGuard<'static, ()>) {
        unsafe {
            std::env::remove_var("MOAGAN_HOME");
        }
        drop(guard);
    }

    /// `should_reconcile_at_startup` is the gate that scopes the
    /// reconcile sweep to pipeline-opening commands. Read-only
    /// commands like `Doctor` must NOT trigger the reconcile so
    /// `moagan doctor` keeps its deterministic exit code.
    #[test]
    fn should_reconcile_at_startup_skips_read_only_commands() {
        assert!(!should_reconcile_at_startup(&Cmd::Doctor {
            capabilities: false
        }));
    }

    /// D.28.3 + D.28.4 wire: when
    /// `Config::startup_reconcile == true` (the default), the
    /// pre-dispatch reconcile actually runs and returns a report.
    /// The test sets up a clean `MOAGAN_HOME` (no orphans, no
    /// zombies) and asserts the report is `Some` with both
    /// counters at zero, the canonical "ran, nothing to do"
    /// outcome.
    #[test]
    fn cli_run_invokes_startup_reconcile_when_enabled() {
        let (_keep, tmp) = unique_tmp("reconcile-enabled");
        let guard = lock_home_env(&tmp);
        let home = MoaganHome::at(tmp.clone());

        let cfg = Config::default();
        assert!(
            cfg.startup_reconcile,
            "default Config::startup_reconcile must be true"
        );

        let report =
            run_startup_reconcile(&home, &cfg).expect("reconcile must succeed on a fresh home");
        let report = report.expect("Some(report) when startup_reconcile is enabled");
        assert_eq!(report.orphans_removed, 0);
        assert_eq!(report.zombies_recovered, 0);

        unlock_home_env(guard);
        // `_keep` (TempDir) drops at end of test → dir cleaned up.
    }

    /// D.28.3 + D.28.4 wire: when
    /// `Config::startup_reconcile == false`, the pre-dispatch
    /// reconcile short-circuits and returns `Ok(None)`. No DB
    /// open, no `meta.sqlite` migration, no filesystem walk —
    /// the operator who opted out pays nothing.
    #[test]
    fn cli_run_skips_startup_reconcile_when_disabled() {
        let (_keep, tmp) = unique_tmp("reconcile-disabled");
        let guard = lock_home_env(&tmp);
        let home = MoaganHome::at(tmp.clone());

        let cfg = Config {
            startup_reconcile: false,
            ..Config::default()
        };

        let report = run_startup_reconcile(&home, &cfg)
            .expect("disabled path must not error on a fresh home");
        assert!(
            report.is_none(),
            "startup_reconcile=false must short-circuit to Ok(None); got {report:?}"
        );

        unlock_home_env(guard);
        // `_keep` (TempDir) drops at end of test → dir cleaned up.
    }

    /// D.28.3 + D.28.4 wire end-to-end: the pre-dispatch
    /// reconcile actually cleans up `*.tmp.<hex>` orphans and
    /// flips stale `running` rows to `interrupted`. The test
    /// seeds both shapes and asserts the returned report
    /// reports the right counts and the side effects land on
    /// disk + SQLite.
    #[test]
    fn cli_run_reconcile_sweeps_zombies_and_orphans() {
        let (_keep, tmp) = unique_tmp("reconcile-sweep");
        let guard = lock_home_env(&tmp);
        let home = MoaganHome::at(tmp.clone());

        // Open the DB once so the migration runner applies
        // before we seed a zombie row (otherwise `register_run`
        // would race the migration).
        let db = crate::storage::sqlite::Db::open(&home.meta_db_path()).expect("open db");

        // Seed a zombie: stale `running` row whose
        // `updated_unix` is past the 2h threshold.
        let zombie = crate::ids::RunId::new();
        db.register_run(zombie, "fast", "running", "0.4.0", None, None, None)
            .expect("register zombie");
        let now = crate::time::now_unix_secs();
        let past = now - crate::reconcile::ZOMBIE_HEARTBEAT_SECS - 600;
        db._test_backdate_updated_unix(zombie, past)
            .expect("backdate");

        // Seed an orphan: a `*.tmp.<hex>` file inside a real
        // run directory. `cleanup_orphans` walks every run
        // directory it can find on disk; creating a real run
        // dir makes the walker hit the fixture.
        let run_with_orphan = crate::ids::RunId::new();
        let proposals_dir = home
            .runs_dir()
            .join(run_with_orphan.to_string())
            .join("proposals");
        std::fs::create_dir_all(&proposals_dir).expect("mkdir");
        let orphan = proposals_dir.join("p_001.json.tmp.deadbeef01234567");
        std::fs::write(&orphan, b"orphan").expect("write orphan");
        drop(db);

        let cfg = Config::default();
        assert!(cfg.startup_reconcile);
        let report = run_startup_reconcile(&home, &cfg)
            .expect("sweep must succeed")
            .expect("Some(report) when enabled");

        assert_eq!(report.orphans_removed, 1, "exactly one orphan removed");
        assert_eq!(report.zombies_recovered, 1, "exactly one zombie recovered");

        // Side effects must land on disk + SQLite.
        assert!(!orphan.exists(), "orphan file must be gone");
        let db_after = crate::storage::sqlite::Db::open(&home.meta_db_path()).expect("reopen db");
        let row = db_after
            .get_run(zombie)
            .expect("get_run")
            .expect("zombie row must still exist (flipped to interrupted)");
        assert_eq!(
            row.status, "interrupted",
            "zombie must be flipped to `interrupted`"
        );

        unlock_home_env(guard);
        // `_keep` (TempDir) drops at end of test → dir cleaned up.
    }

    /// PR-B1 (B1.2): `moagan rerun --same-config=false` is parsed
    /// at the CLI layer and reaches the dispatcher. The previous
    /// audit had the dispatcher destructuring it with `_: ` so
    /// the value was discarded; the wire-up routes it through to
    /// `continue_cmd::run_rerun`. Pin the parsed shape here so a
    /// future clap refactor cannot silently drop the field.
    #[test]
    fn rerun_cli_parses_same_config_default_true() {
        let cli =
            Cli::try_parse_from(["moagan", "rerun", "--run-id", "01HF3Z1K9R5X7QYABCDEF01234"])
                .expect("parse must succeed");
        match cli.cmd {
            Cmd::Rerun { same_config, .. } => {
                assert!(same_config, "default for --same-config must remain `true`");
            }
            other => panic!("expected Cmd::Rerun, got {other:?}"),
        }
    }

    /// PR-B1 (B1.2): `--same-config=false` reaches the dispatcher
    /// intact. The flag is now wired through; the dispatcher
    /// passes the parsed bool to `run_rerun` instead of
    /// discarding it with `_: `. The clap `action = Set` +
    /// `default_value_t = true` combination makes
    /// `--same-config=false` round-trip as expected (the cheatsheet
    /// documents this combination under §4 row 2).
    #[test]
    fn rerun_cli_parses_same_config_false() {
        let cli = Cli::try_parse_from([
            "moagan",
            "rerun",
            "--run-id",
            "01HF3Z1K9R5X7QYABCDEF01234",
            "--same-config=false",
        ])
        .expect("parse must succeed");
        match cli.cmd {
            Cmd::Rerun { same_config, .. } => {
                assert!(
                    !same_config,
                    "--same-config=false must round-trip to the dispatcher"
                );
            }
            other => panic!("expected Cmd::Rerun, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Issue #657 regression tests
    // -----------------------------------------------------------------------

    /// Issue #657 fix #1: the documented `MOAGAN_LOG_TO_STDERR=1`
    /// shell idiom now parses through clap. Before the fix the
    /// strict `bool` parser rejected `1` with
    /// `error: invalid value '1' for '--log-to-stderr'`, so an
    /// operator following the docstring hit a hard startup failure.
    /// The `BoolishValueParser` accepts every canonical spelling
    /// (`true`/`false`, `yes`/`no`, `on`/`off`, `1`/`0`).
    #[test]
    fn log_to_stderr_env_accepts_shell_idiomatic_one() {
        // Save-and-restore pattern (same as PR #677 at
        // `src/telemetry/stdout_events.rs:557-567` and PR #678 at
        // `src/cli/mod.rs::tests::event_format_default_is_auto`):
        // if the test panics between `set_var` and the restore
        // block, the inherited `MOAGAN_LOG_TO_STDERR` value (if
        // any) is still restored. The lock guards against
        // concurrent access; the save/restore guards against panic
        // leakage.
        //
        // SAFETY: tests in this module serialise on one of
        // `TEST_MOAGAN_HOME_LOCK` (for `MOAGAN_HOME` mutations),
        // `TEST_LOG_TO_STDERR_LOCK` (for `MOAGAN_LOG_TO_STDERR`
        // mutations, PR #679 item 1), or `TEST_EVENT_FORMAT_LOCK`
        // (for `MOAGAN_EVENT_FORMAT` mutations). Env mutations are
        // scoped to each test body and undone in its cleanup.
        let prev = std::env::var("MOAGAN_LOG_TO_STDERR").ok();
        let _guard = TEST_LOG_TO_STDERR_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_LOG_TO_STDERR", "1");
        }
        let cli = Cli::try_parse_from(["moagan", "doctor"])
            .expect("MOAGAN_LOG_TO_STDERR=1 must parse through clap");
        assert!(
            cli.log_to_stderr,
            "MOAGAN_LOG_TO_STDERR=1 must round-trip as `true`"
        );
        drop(_guard);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("MOAGAN_LOG_TO_STDERR", v),
                None => std::env::remove_var("MOAGAN_LOG_TO_STDERR"),
            }
        }
    }

    /// Issue #657 fix #1: `MOAGAN_LOG_TO_STDERR=false` (the
    /// canonical off-spelling) parses as `false` through clap.
    /// Pin the round-trip so a future clap refactor cannot
    /// silently regress the BoolishValueParser on the off side.
    #[test]
    fn log_to_stderr_env_accepts_false() {
        // Save-and-restore pattern (same as the sibling
        // `log_to_stderr_env_accepts_shell_idiomatic_one` above;
        // PR #677 / PR #678 precedent). Guards against panic
        // leakage of the inherited `MOAGAN_LOG_TO_STDERR` value.
        let prev = std::env::var("MOAGAN_LOG_TO_STDERR").ok();
        let _guard = TEST_LOG_TO_STDERR_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_LOG_TO_STDERR", "false");
        }
        let cli = Cli::try_parse_from(["moagan", "doctor"])
            .expect("MOAGAN_LOG_TO_STDERR=false must parse through clap");
        assert!(
            !cli.log_to_stderr,
            "MOAGAN_LOG_TO_STDERR=false must round-trip as `false`"
        );
        drop(_guard);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("MOAGAN_LOG_TO_STDERR", v),
                None => std::env::remove_var("MOAGAN_LOG_TO_STDERR"),
            }
        }
    }

    /// Issue #657 fix #1: the explicit `--log-to-stderr` flag
    /// still works after the BoolishValueParser swap (regression
    /// guard — the parser change must NOT break the CLI flag
    /// path).
    #[test]
    fn log_to_stderr_flag_still_parses() {
        let cli = Cli::try_parse_from(["moagan", "--log-to-stderr", "doctor"])
            .expect("--log-to-stderr flag must still parse");
        assert!(cli.log_to_stderr);
    }

    /// Issue #657 fix #2: the new `auto` arm is the documented
    /// default. `Cli::try_parse_from(["moagan", "doctor"])` with
    /// no `--event-format` and no `MOAGAN_EVENT_FORMAT` env var
    /// lands on `EventFormatArg::Auto` so the env-var propagation
    /// in `src/main.rs` is a no-op (preserving any inherited
    /// `MOAGAN_EVENT_FORMAT=off`).
    #[test]
    fn event_format_default_is_auto() {
        // Save-and-restore pattern (same as PR #677 at
        // `src/telemetry/stdout_events.rs:557-567`): if the test
        // panics between `remove_var` and the end, the inherited
        // value is still restored. Lock guards against concurrent
        // access; the save/restore guards against panic leakage.
        let prev = std::env::var("MOAGAN_EVENT_FORMAT").ok();
        let _guard = TEST_EVENT_FORMAT_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::remove_var("MOAGAN_EVENT_FORMAT");
        }
        let cli = Cli::try_parse_from(["moagan", "doctor"]).expect("parse must succeed");
        assert_eq!(
            cli.event_format,
            EventFormatArg::Auto,
            "default for --event-format must be Auto (issue #657 fix #2)"
        );
        drop(_guard);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("MOAGAN_EVENT_FORMAT", v),
                None => std::env::remove_var("MOAGAN_EVENT_FORMAT"),
            }
        }
    }

    /// Issue #657 fix #2: an inherited `MOAGAN_EVENT_FORMAT=off`
    /// reaches the CLI parser as `EventFormatArg::Off` via the
    /// `env = "MOAGAN_EVENT_FORMAT"` clap binding. The pre-fix
    /// `Some("jsonl")` arm in `src/main.rs` overwrote the env
    /// var unconditionally; the new `Auto`-aware propagation
    /// leaves it alone when the operator did not pass the flag
    /// explicitly. The propagation logic itself lives in
    /// `src/main.rs` and is exercised end-to-end by the
    /// integration test in
    /// `tests/integration_e2e_script_paths.rs` (the
    /// `MOAGAN_EVENT_FORMAT` inherited-off case).
    #[test]
    fn event_format_env_off_reaches_parser() {
        // Same save/restore pattern as
        // `event_format_default_is_auto` above.
        let prev = std::env::var("MOAGAN_EVENT_FORMAT").ok();
        let _guard = TEST_EVENT_FORMAT_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_EVENT_FORMAT", "off");
        }
        let cli = Cli::try_parse_from(["moagan", "doctor"]).expect("parse must succeed");
        assert_eq!(
            cli.event_format,
            EventFormatArg::Off,
            "MOAGAN_EVENT_FORMAT=off must reach the parser as Off (issue #657 fix #2)"
        );
        drop(_guard);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("MOAGAN_EVENT_FORMAT", v),
                None => std::env::remove_var("MOAGAN_EVENT_FORMAT"),
            }
        }
    }

    /// Issue #657 fix #2: the explicit `--event-format jsonl` flag
    /// still parses as `Jsonl` (regression guard — the
    /// default-arm swap to `Auto` must NOT change the spelled-out
    /// `jsonl` arm).
    #[test]
    fn event_format_explicit_jsonl_flag() {
        let cli = Cli::try_parse_from(["moagan", "--event-format", "jsonl", "doctor"])
            .expect("parse must succeed");
        assert_eq!(cli.event_format, EventFormatArg::Jsonl);
    }

    /// Issue #657 fix #2: the explicit `--event-format off` flag
    /// still parses as `Off`.
    #[test]
    fn event_format_explicit_off_flag() {
        let cli = Cli::try_parse_from(["moagan", "--event-format", "off", "doctor"])
            .expect("parse must succeed");
        assert_eq!(cli.event_format, EventFormatArg::Off);
    }
}
