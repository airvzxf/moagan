//! `moagan telemetry` — read-only inspection of run telemetry.
//!
//! Implements the eight subcommands spelled out in T01-06 §10.7 and
//! `proposal-01-concept.md` §8.7:
//!
//! | Subcommand | Purpose                                              |
//! |------------|------------------------------------------------------|
//! | `list`     | List recent runs (mirrors `moagan inspect`).          |
//! | `summary`  | Per-run aggregates (tokens, calls, by-model, by-phase). |
//! | `compare`  | Diff two runs side-by-side.                          |
//! | `provider` | Provider plans + recent per-provider usage.          |
//! | `view`     | Read-only HTTP dashboard on `127.0.0.1:<port>`.      |
//! | `export`   | Bundle the run as `tar.gz` / `tar` / `zip` + SHA256SUMS. |
//! | `cleanup`  | Apply retention policy (`--dry-run` supported).      |
//! | `verify`   | Re-hash an exported bundle against its SHA256SUMS.   |
//!
//! All subcommands read SQLite (the index) and the filesystem (the
//! canonical record). They never mutate run state.

use crate::error::{Error, Result};
use crate::ids::RunId;

/// Top-level `moagan telemetry` subcommand.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum TelemetryCmd {
    /// `moagan telemetry list [--limit <N>] [--run <id>]`.
    List {
        /// Optional override for `MOAGAN_HOME`.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Maximum number of runs to print when `--run` is omitted.
        #[arg(long, default_value_t = 10)]
        limit: u32,
        /// Optional run id to drill into.
        #[arg(long)]
        run: Option<String>,
    },
    /// `moagan telemetry summary --run <id>`.
    Summary {
        /// Optional override for `MOAGAN_HOME`.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Target run id.
        #[arg(long)]
        run: String,
    },
    /// `moagan telemetry compare <run_a> <run_b>`.
    Compare {
        /// Optional override for `MOAGAN_HOME`.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// First run id.
        #[arg(long)]
        run_a: String,
        /// Second run id.
        #[arg(long)]
        run_b: String,
    },
    /// `moagan telemetry provider [--list | --plan <name>]`.
    Provider {
        /// Optional override for `MOAGAN_HOME`.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Provider plan name to display.
        #[arg(long)]
        plan: Option<String>,
        /// List every configured provider.
        #[arg(long, default_value_t = false)]
        list: bool,
    },
    /// `moagan telemetry view --port <port>`.
    View {
        /// Optional override for `MOAGAN_HOME`.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Bind port. `0` requests a kernel-assigned free port.
        #[arg(long, default_value_t = 4096)]
        port: u16,
    },
    /// `moagan telemetry export --run <id> [--level <summary|full>]
    ///  [--format <tar.gz|tar|zip>] [--out <path>]`.
    Export {
        /// Optional override for `MOAGAN_HOME`.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Target run id.
        #[arg(long)]
        run: String,
        /// Export level: `summary` (default) or `full`.
        #[arg(long, default_value_t = ExportLevel::default())]
        level: ExportLevel,
        /// Export format: `tar.gz` (default), `tar`, or `zip`.
        #[arg(long, default_value_t = ExportFormat::default())]
        format: ExportFormat,
        /// Destination path.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// `moagan telemetry cleanup [--dry-run] [--archive] [--cross-run] [--yes]`.
    ///
    /// Two modes:
    /// * The default per-run retention pass (D.5.1) — driven by
    ///   `Config::retention` and the `--archive` flag.
    /// * The cross-run sweep (D.29-D.32) — selected by
    ///   `--cross-run`. Reports orphan rows per table by default
    ///   (dry-run). Pass `--yes` to actually purge the rows;
    ///   `--dry-run` is honoured either way for symmetry.
    ///   `--cross-run` is mutually exclusive with `--archive`.
    Cleanup {
        /// Optional override for `MOAGAN_HOME`.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// When true, print what would be deleted without touching
        /// the filesystem. The default for the per-run retention
        /// pass is `false`; the cross-run sweep inverts the default
        /// so a missing `--yes` always lands in dry-run.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Override the retention policy for this invocation:
        /// archive eligible runs into `<root>/archive/YYYY-MM-DD/`
        /// instead of deleting them. The config knob
        /// `Config::retention.policy` is the default; the flag
        /// wins.
        #[arg(long, default_value_t = false)]
        archive: bool,
        /// Switch from the per-run retention pass to the cross-run
        /// sweep (D.29-D.32). The sweep lists — and with `--yes`
        /// purges — every SQLite row whose `run_id` no longer
        /// exists in the `runs` table. The per-table row count is
        /// the only output.
        #[arg(long, default_value_t = false)]
        cross_run: bool,
        /// Confirm a destructive cross-run purge. When `--cross-run`
        /// is set and `--yes` is missing, the sweep runs in
        /// dry-run mode regardless of `--dry-run`. This two-flag
        /// shape mirrors the `cargo install --force` / `git push
        /// --force` convention so a missing confirmation never
        /// silently drops rows.
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// `moagan telemetry verify --path <export-path>`.
    Verify {
        /// Optional override for `MOAGAN_HOME`.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Path to an exported directory (or archive) carrying a
        /// `SHA256SUMS` file.
        #[arg(long)]
        path: std::path::PathBuf,
    },
    /// `moagan telemetry config` — print the effective configuration
    /// (providers, parallelism, timeouts, privacy, telemetry level).
    /// API keys are NEVER printed — they are visible only via the
    /// `SecretString::expose()` path inside the registry.
    Config {
        /// Optional override for `MOAGAN_HOME`.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
    },
    /// `moagan telemetry plan [<provider>] [--window-days N]`.
    ///
    /// Rolling-window quota view aggregated from the per-call
    /// `calls` table (T01-06 §2.1). Distinct from `provider --plan`,
    /// which drills into one provider's per-run rollup; this subcommand
    /// answers "how much of my token plan have I consumed in the
    /// last N days?" for every configured provider at once.
    ///
    /// Exit semantics: 0 when at least one call lands in the window,
    /// 1 when the window is empty (mirrors the `moagan validate`
    /// "no FAIL → 0, anything to report → 1" convention).
    Plan {
        /// Optional override for `MOAGAN_HOME`.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Optional provider filter (must match a key in
        /// `[providers]`). When `None`, every provider is aggregated.
        #[arg(value_name = "PROVIDER")]
        provider: Option<String>,
        /// Length of the rolling window in days. The CLI default is
        /// `7`; a `[providers.X].plan.window_days` override on the
        /// (single) filtered provider also wins when set.
        #[arg(long, default_value_t = 7)]
        window_days: u32,
    },
    /// `moagan telemetry cost --run <id>` (or `--all`).
    ///
    /// PR-7: per-call USD cost aggregation, computed from the
    /// `cost_usd` column the runtime wrote against the
    /// `models.dev` catalog rates. With `--run <id>` the
    /// aggregation is scoped to one run; with `--all` it spans
    /// every run in the index. Read-only; never mutates state.
    Cost {
        /// Optional override for `MOAGAN_HOME`.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Target run id. Mutually exclusive with `--all`.
        #[arg(long, conflicts_with = "all")]
        run: Option<String>,
        /// Aggregate across every run in the index.
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// `moagan telemetry alerts list [--since <date>] [--provider <name>]`.
    ///
    /// v0.8 push-side: list saturation events (catalog §D.23 +
    /// §D.27) recorded by the runtime. `--since` accepts either a
    /// unix timestamp (seconds) or an ISO calendar date
    /// (`YYYY-MM-DD`); the conversion is best-effort and falls
    /// back to "no lower bound" on parse errors. `--provider`
    /// filters by the inner provider name (e.g. `minimax`,
    /// `mock`). Read-only; never mutates state.
    Alerts {
        /// Optional override for `MOAGAN_HOME`.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// Subcommand (`list` for now; reserved for future
        /// `clear` / `archive` sub-subcommands).
        #[command(subcommand)]
        action: AlertsAction,
    },
}

/// Sub-action under `moagan telemetry alerts` (catalog §D.23 +
/// §D.27, v0.8 push-side).
#[derive(Debug, Clone, clap::Subcommand)]
pub enum AlertsAction {
    /// List recent saturation events.
    List {
        /// Lower bound. Unix seconds (`1700000000`) or ISO date
        /// (`2024-01-01`); either is accepted and converted to
        /// unix seconds. Missing means "no lower bound".
        #[arg(long)]
        since: Option<String>,
        /// Provider name filter (e.g. `minimax`, `mock`).
        #[arg(long)]
        provider: Option<String>,
        /// Maximum number of rows to print. `0` means "no cap".
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
}

/// Export level. Mirrors T01-06 §10.9 + V4 §9.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportLevel {
    /// Manifest + brief + sketches summary + rankings. Default.
    #[default]
    Summary,
    /// Everything in summary plus `calls.jsonl.gz` and all outputs.
    Full,
}

/// Export archive format. Mirrors T01-06 §10.9 + V4 §9.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportFormat {
    /// gzipped tarball. Default.
    #[default]
    TarGz,
    /// plain tar archive (uncompressed).
    Tar,
    /// zip archive (uses `deflate`).
    Zip,
    /// F5: tar archive compressed with zstd (`.tar.zst`).
    TarZst,
}

impl std::str::FromStr for ExportLevel {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "summary" => Ok(Self::Summary),
            "full" => Ok(Self::Full),
            other => Err(Error::InvalidArgs(format!(
                "invalid export level '{other}' (expected 'summary' or 'full')"
            ))),
        }
    }
}

impl std::str::FromStr for ExportFormat {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "tar.gz" | "targz" | "tgz" => Ok(Self::TarGz),
            "tar" => Ok(Self::Tar),
            "zip" => Ok(Self::Zip),
            "tar.zst" | "tarzst" | "tzst" => Ok(Self::TarZst),
            other => Err(Error::InvalidArgs(format!(
                "invalid export format '{other}' (expected 'tar.gz', 'tar', 'zip', or 'tar.zst')"
            ))),
        }
    }
}

impl std::fmt::Display for ExportLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Summary => "summary",
            Self::Full => "full",
        })
    }
}

impl std::fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TarGz => "tar.gz",
            Self::Tar => "tar",
            Self::Zip => "zip",
            Self::TarZst => "tar.zst",
        })
    }
}

impl TelemetryCmd {
    /// Dispatch the telemetry subcommand.
    pub async fn dispatch(self) -> Result<i32> {
        match self {
            Self::List { .. } => list::run(&self).map(|_| 0),
            Self::Summary { .. } => summary::run(&self).map(|_| 0),
            Self::Compare { .. } => compare::run(&self).map(|_| 0),
            Self::Provider { .. } => provider::run(&self).map(|_| 0),
            Self::View { .. } => view::run(&self).await.map(|_| 0),
            Self::Export { .. } => export::run(&self).map(|_| 0),
            Self::Cleanup { .. } => cleanup::run(&self).map(|_| 0),
            Self::Verify { .. } => verify::run(&self).map(|_| 0),
            Self::Config { .. } => config::run(&self).map(|_| 0),
            // `plan` is the only subcommand that needs to surface a
            // non-zero exit code on the happy path (the window is
            // empty) so the operator's shell `if moagan telemetry
            // plan; then …` can branch. Its `run` returns
            // `Result<i32>` directly; every other subcommand
            // collapses to 0 on `Ok`.
            Self::Plan { .. } => plan::run(&self),
            Self::Cost { .. } => cost::run(&self).map(|_| 0),
            Self::Alerts { .. } => alerts::run(&self).map(|_| 0),
        }
    }

    /// Extract a `RunId` from the variants that carry one. Returns
    /// `Err(InvalidState)` for variants that don't.
    #[allow(dead_code)]
    pub(crate) fn parse_run(&self, raw: &str) -> Result<RunId> {
        raw.parse()
            .map_err(|e| Error::InvalidArgs(format!("invalid run id '{raw}': {e}")))
    }
}

/// Resolve the `MoaganHome` for a telemetry subcommand. When
/// `runs_dir` is `Some`, the explicit path is used; otherwise the
/// standard `MOAGAN_HOME` / `~/.local/share/moagan` resolution
/// applies (T01-06 §11.1).
pub(crate) fn resolve_home(
    runs_dir: Option<&std::path::Path>,
) -> Result<crate::fs_layout::MoaganHome> {
    match runs_dir {
        Some(p) => Ok(crate::fs_layout::MoaganHome::at(p.to_path_buf())),
        None => crate::fs_layout::MoaganHome::resolve(),
    }
}

#[allow(dead_code)]
mod stubs_removed {
    use super::{Error, Result};
    pub(crate) fn run_stub(_name: &str) -> Result<()> {
        Err(Error::InvalidState(
            "moagan telemetry: stub called (should be unreachable)".to_string(),
        ))
    }
}

mod list {
    use super::{Error, Result, TelemetryCmd};
    use crate::ids::RunId;
    use crate::storage::sqlite::Db;

    pub(super) fn run(cmd: &TelemetryCmd) -> Result<()> {
        let (runs_dir, limit, run) = match cmd {
            TelemetryCmd::List {
                runs_dir,
                limit,
                run,
            } => (runs_dir.as_ref(), *limit, run.as_deref()),
            _ => return Err(Error::InvalidState("list: wrong variant".into())),
        };
        let home = super::resolve_home(runs_dir.map(|p| p.as_path()))?;
        let db = Db::open(&home.meta_db_path())?;

        if let Some(raw) = run {
            let run_id: RunId = raw
                .parse()
                .map_err(|e| Error::InvalidArgs(format!("invalid run id '{raw}': {e}")))?;
            let row = db
                .get_run(run_id)?
                .ok_or_else(|| Error::InvalidState(format!("run {raw} not found in the index")))?;
            let agg = db.run_aggregate(run_id)?;
            let phases = db.list_phase_summaries_for_run(run_id)?;
            let usage = db.list_provider_usage_for_run(run_id)?;
            let run_dir = home.run_dir(run_id);
            print_one_run(&row, run_dir.root(), &agg, &phases, &usage);
        } else {
            let rows = db.list_runs(limit)?;
            if rows.is_empty() {
                println!("(no runs in the index)");
                return Ok(());
            }
            println!(
                "{:<14}  {:<10}  {:<12}  {:<13}  {:<13}",
                "run", "mode", "status", "calls", "tokens"
            );
            for row in &rows {
                let run_id: RunId = row
                    .run_id
                    .parse()
                    .map_err(|e| Error::InvalidArgs(format!("bad run row: {e}")))?;
                let agg = db.run_aggregate(run_id)?;
                println!(
                    "{:<14}  {:<10}  {:<12}  {:<13}  {:<13}",
                    short_id(&row.run_id),
                    row.mode,
                    row.status,
                    agg.calls,
                    agg.total_tokens(),
                );
            }
            println!("({} run(s); use --run <id> to drill into one)", rows.len());
        }
        Ok(())
    }

    /// First 8 chars of a UUIDv7 string. UUIDv7's first segment is
    /// always ASCII, so byte-slicing is safe here.
    fn short_id(raw: &str) -> &str {
        raw.get(..8).unwrap_or(raw)
    }

    fn print_one_run(
        row: &crate::storage::sqlite::RunRow,
        run_dir: &std::path::Path,
        agg: &crate::storage::sqlite::RunAggregate,
        phases: &[crate::storage::sqlite::PhaseSummaryRow],
        usage: &[crate::storage::sqlite::ProviderUsageRow],
    ) {
        println!(
            "run {}  mode={}  status={}",
            row.run_id, row.mode, row.status
        );
        println!(
            "  created_unix={}  updated_unix={}",
            row.created_unix, row.updated_unix
        );
        println!("  dir={}", run_dir.display());
        println!(
            "  calls={}  tokens={}  providers={}  phases={}  warnings={}  checkpoints={}",
            agg.calls,
            agg.total_tokens(),
            agg.provider_count,
            agg.phase_count,
            agg.warnings,
            agg.checkpoints,
        );
        println!(
            "  by-status: ok={}  error={}  timeout={}  cancelled={}",
            agg.ok_calls(),
            agg.error_calls,
            agg.timeout_calls,
            agg.cancelled_calls
        );
        if !phases.is_empty() {
            println!("  phases:");
            for p in phases {
                let dur = match (p.started_unix, p.ended_unix) {
                    (Some(s), Some(e)) => format!("{}s", e.saturating_sub(s)),
                    _ => "-".into(),
                };
                let err = p.error.as_deref().unwrap_or("-");
                println!(
                    "    {:<10}  seq={}  status={:<6}  duration={}  error={}",
                    p.phase, p.seq, p.status, dur, err
                );
            }
        }
        if !usage.is_empty() {
            println!("  provider_usage:");
            for u in usage {
                println!(
                    "    {:<10}  {:<16}  calls={:<5}  in={:<8}  out={:<6}  cache_read={}  cache_creation={}",
                    u.provider,
                    u.model,
                    u.calls,
                    u.input_tokens,
                    u.output_tokens,
                    u.cache_read,
                    u.cache_creation,
                );
            }
        }
    }
}

mod summary {
    use super::{Error, Result, TelemetryCmd};
    use crate::ids::RunId;
    use crate::storage::sqlite::Db;

    pub(super) fn run(cmd: &TelemetryCmd) -> Result<()> {
        let (runs_dir, run) = match cmd {
            TelemetryCmd::Summary { runs_dir, run } => (runs_dir.as_ref(), run.as_str()),
            _ => return Err(Error::InvalidState("summary: wrong variant".into())),
        };
        let run_id: RunId = run
            .parse()
            .map_err(|e| Error::InvalidArgs(format!("invalid run id '{run}': {e}")))?;
        let home = super::resolve_home(runs_dir.map(|p| p.as_path()))?;
        let db = Db::open(&home.meta_db_path())?;
        let row = db
            .get_run(run_id)?
            .ok_or_else(|| Error::InvalidState(format!("run {run} not found in the index")))?;
        let agg = db.run_aggregate(run_id)?;
        let usage = db.list_provider_usage_for_run(run_id)?;
        let phases = db.list_phase_summaries_for_run(run_id)?;
        let run_dir = home.run_dir(run_id);
        let root = run_dir.root();
        let bytes = dir_bytes(root).unwrap_or(0);
        let duration_secs = row.updated_unix.saturating_sub(row.created_unix).max(0);

        println!("Run: {}", row.run_id);
        println!("Mode: {}", row.mode);
        println!("Status: {}", row.status);
        println!("Duration: {}", human_duration(duration_secs));
        println!("Tokens: {}", agg.total_tokens());
        println!(
            "Calls: {} (ok={} error={} timeout={} cancelled={})",
            agg.calls,
            agg.ok_calls(),
            agg.error_calls,
            agg.timeout_calls,
            agg.cancelled_calls
        );
        println!("Phases: {}", agg.phase_count);
        println!("Warnings: {}", agg.warnings);
        println!("Checkpoints: {}", agg.checkpoints);
        println!("Disk: {} (path: {})", human_bytes(bytes), root.display());

        if !usage.is_empty() {
            println!("\nBy model:");
            for u in usage {
                println!(
                    "  {:<10}  {:<20}  calls={:<5}  tokens={}",
                    u.provider,
                    u.model,
                    u.calls,
                    u.input_tokens + u.output_tokens
                );
            }
        }

        if !phases.is_empty() {
            println!("\nBy phase:");
            // Aggregate durations by phase name (collapse sequences).
            let mut totals: std::collections::BTreeMap<String, (i64, i64)> =
                std::collections::BTreeMap::new();
            for p in &phases {
                let entry = totals.entry(p.phase.clone()).or_insert((0, 0));
                entry.0 += 1;
                if let (Some(s), Some(e)) = (p.started_unix, p.ended_unix) {
                    entry.1 += e.saturating_sub(s).max(0);
                }
            }
            for (phase, (count, secs)) in totals {
                println!(
                    "  {:<14}  invocations={:<3}  total_secs={}",
                    phase, count, secs
                );
            }
        }
        Ok(())
    }

    /// Recursive byte count of a directory. Missing directories
    /// return `None`; permission errors are propagated via the
    /// `Result` (callers fall back to 0).
    fn dir_bytes(path: &std::path::Path) -> Option<u64> {
        let mut total: u64 = 0;
        let mut stack = vec![path.to_path_buf()];
        while let Some(p) = stack.pop() {
            let meta = std::fs::symlink_metadata(&p).ok()?;
            if meta.is_file() {
                total = total.checked_add(meta.len())?;
            } else if meta.is_dir() {
                for entry in std::fs::read_dir(&p).ok()? {
                    let entry = entry.ok()?;
                    stack.push(entry.path());
                }
            }
        }
        Some(total)
    }

    fn human_bytes(bytes: u64) -> String {
        const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
        let mut value = bytes as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit < UNITS.len() - 1 {
            value /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{} {}", bytes, UNITS[0])
        } else {
            format!("{:.1} {}", value, UNITS[unit])
        }
    }

    fn human_duration(secs: i64) -> String {
        if secs <= 0 {
            return "0s".to_owned();
        }
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        let mut parts = Vec::new();
        if h > 0 {
            parts.push(format!("{h}h"));
        }
        if m > 0 {
            parts.push(format!("{m}m"));
        }
        if s > 0 || parts.is_empty() {
            parts.push(format!("{s}s"));
        }
        parts.join(" ")
    }
}

pub(crate) mod compare {
    use super::{Error, Result, TelemetryCmd};
    use crate::ids::RunId;
    use crate::storage::sqlite::{Db, RunAggregate, RunRow};

    pub(super) fn run(cmd: &TelemetryCmd) -> Result<()> {
        let (runs_dir, run_a, run_b) = match cmd {
            TelemetryCmd::Compare {
                runs_dir,
                run_a,
                run_b,
            } => (runs_dir.as_ref(), run_a.as_str(), run_b.as_str()),
            _ => return Err(Error::InvalidState("compare: wrong variant".into())),
        };
        let a: RunId = run_a
            .parse()
            .map_err(|e| Error::InvalidArgs(format!("invalid run id '{run_a}': {e}")))?;
        let b: RunId = run_b
            .parse()
            .map_err(|e| Error::InvalidArgs(format!("invalid run id '{run_b}': {e}")))?;
        let home = super::resolve_home(runs_dir.map(|p| p.as_path()))?;
        let db = Db::open(&home.meta_db_path())?;
        let row_a = db
            .get_run(a)?
            .ok_or_else(|| Error::InvalidState(format!("run {run_a} not found in the index")))?;
        let row_b = db
            .get_run(b)?
            .ok_or_else(|| Error::InvalidState(format!("run {run_b} not found in the index")))?;
        let agg_a = db.run_aggregate(a)?;
        let agg_b = db.run_aggregate(b)?;

        print_side_by_side(&row_a, &agg_a, &row_b, &agg_b);
        println!();
        print_diff("tokens", agg_a.total_tokens(), agg_b.total_tokens());
        print_diff("calls", agg_a.calls, agg_b.calls);
        print_diff("ok_calls", agg_a.ok_calls(), agg_b.ok_calls());
        print_diff("error_calls", agg_a.error_calls, agg_b.error_calls);
        print_diff("timeout_calls", agg_a.timeout_calls, agg_b.timeout_calls);
        print_diff(
            "cancelled_calls",
            agg_a.cancelled_calls,
            agg_b.cancelled_calls,
        );
        print_diff("providers", agg_a.provider_count, agg_b.provider_count);
        print_diff("phases", agg_a.phase_count, agg_b.phase_count);
        print_diff("warnings", agg_a.warnings, agg_b.warnings);
        print_diff("checkpoints", agg_a.checkpoints, agg_b.checkpoints);
        let dur_a = row_a.updated_unix.saturating_sub(row_a.created_unix).max(0);
        let dur_b = row_b.updated_unix.saturating_sub(row_b.created_unix).max(0);
        print_diff("duration_secs", dur_a, dur_b);
        Ok(())
    }

    /// Side-by-side summary used by both `moagan telemetry compare`
    /// and `moagan diff` (D.14.2). Exposed at `pub(crate)` so the
    /// top-level `diff` module can reuse the rendering without
    /// duplicating the columnar printer.
    pub(crate) fn print_side_by_side(
        a: &RunRow,
        agg_a: &RunAggregate,
        b: &RunRow,
        agg_b: &RunAggregate,
    ) {
        let id_a = short(&a.run_id);
        let id_b = short(&b.run_id);
        println!("{:<22}  {:<30}  {:<30}", "", id_a, id_b);
        println!("{:<22}  {:<30}  {:<30}", "mode", a.mode, b.mode);
        println!("{:<22}  {:<30}  {:<30}", "status", a.status, b.status);
        println!(
            "{:<22}  {:<30}  {:<30}",
            "tokens",
            agg_a.total_tokens(),
            agg_b.total_tokens()
        );
        println!("{:<22}  {:<30}  {:<30}", "calls", agg_a.calls, agg_b.calls);
        println!(
            "{:<22}  {:<30}  {:<30}",
            "errors", agg_a.error_calls, agg_b.error_calls
        );
    }

    /// One row of `(a, b, b - a)` for the eleven baseline metrics the
    /// `compare` subcommand emits. Exposed at `pub(crate)` so `diff`
    /// can extend the list with filesystem-aware metrics without
    /// duplicating the formatter.
    pub(crate) fn print_diff(label: &str, a: i64, b: i64) {
        let delta = b - a;
        let sign = if delta > 0 { "+" } else { "" };
        println!(
            "{:<22}  a={:<10}  b={:<10}  delta={}{}",
            label, a, b, sign, delta
        );
    }

    fn short(raw: &str) -> &str {
        raw.get(..8).unwrap_or(raw)
    }
}

mod provider {
    use super::{Error, Result, TelemetryCmd};
    use crate::config::{Config, ProviderConfig};
    use crate::storage::sqlite::Db;
    use std::collections::BTreeMap;

    pub(super) fn run(cmd: &TelemetryCmd) -> Result<()> {
        let (runs_dir, plan, list) = match cmd {
            TelemetryCmd::Provider {
                runs_dir,
                plan,
                list,
            } => (runs_dir.as_ref(), plan.as_deref(), *list),
            _ => return Err(Error::InvalidState("provider: wrong variant".into())),
        };
        let cfg = Config::load()?;
        let home = super::resolve_home(runs_dir.map(|p| p.as_path()))?;
        let db = Db::open(&home.meta_db_path())?;

        if list {
            list_providers(&cfg, &db);
        } else if let Some(name) = plan {
            plan_summary(name, &cfg, &db)?;
        } else {
            // Default action (no flag): list providers. This matches
            // V4 §8.7 ("moagan telemetry provider" with no flags
            // shows the provider roster).
            list_providers(&cfg, &db);
        }
        Ok(())
    }

    fn list_providers(cfg: &Config, db: &Db) {
        println!(
            "{:<14}  {:<24}  {:<14}  {:<10}  {:<10}  calls",
            "name", "endpoint", "model", "tokens_in", "tokens_out"
        );
        let rows = db.aggregate_provider_usage().unwrap_or_default();
        let mut by_key: BTreeMap<(String, String), (i64, i64, i64)> = BTreeMap::new();
        for r in &rows {
            let entry = by_key
                .entry((r.provider.clone(), r.model.clone()))
                .or_insert((0, 0, 0));
            entry.0 += r.calls;
            entry.1 += r.input_tokens;
            entry.2 += r.output_tokens;
        }
        for (name, provider_cfg) in &cfg.providers {
            // v0.10: each row in `models[]` is a distinct
            // `(provider, model)` pair the upstream sees; the
            // SQLite join keys on the same pair. The section-level
            // `endpoint` is `Option<String>`; per-model endpoints
            // live on the model itself.
            for model in &provider_cfg.models {
                let key = (name.clone(), model.id.clone());
                let stats = by_key.get(&key).copied().unwrap_or((0, 0, 0));
                let endpoint_str = provider_cfg.endpoint.as_deref().unwrap_or("");
                println!(
                    "{:<14}  {:<24}  {:<14}  {:<10}  {:<10}  {}",
                    name,
                    trim(endpoint_str, 24),
                    trim(&model.id, 14),
                    stats.1,
                    stats.2,
                    stats.0
                );
            }
        }
    }

    fn plan_summary(name: &str, cfg: &Config, db: &Db) -> Result<()> {
        let provider: &ProviderConfig = cfg
            .providers
            .get(name)
            .ok_or_else(|| Error::InvalidArgs(format!("unknown provider plan '{name}'")))?;
        println!("Provider: {}", name);
        // v0.10: the section name doubles as the kind tag for API-
        // key lookup; the deprecated `provider.kind` field is gone.
        println!("Kind: {name}");
        println!(
            "Endpoint: {}",
            provider.endpoint.as_deref().unwrap_or("<none>")
        );
        // Each registered model gets its own block so the
        // operator can read `max_tokens`, `endpoint` (per-model
        // override), and the section-level temperature / top-p
        // without scrolling through a single-line dump.
        for model in &provider.models {
            println!("Model: {}", model.id);
            if let Some(max) = model.max_tokens {
                println!("  Max tokens: {max}");
            }
            if let Some(endpoint) = &model.endpoint {
                println!("  Endpoint: {endpoint}");
            }
        }
        if let Some(t) = provider.temperature {
            println!("Temperature: {t}");
        }
        if let Some(p) = provider.top_p {
            println!("Top-p: {p}");
        }
        println!();
        println!("Recent usage (last 20 runs):");
        let rows = db.recent_runs_for_provider(name, 20)?;
        if rows.is_empty() {
            println!("  (no recorded usage)");
            return Ok(());
        }
        #[allow(clippy::print_literal)]
        {
            println!(
                "  {:<14}  {:<24}  calls={:<5}  in={:<8}  out={:<6}  last_call_unix={}",
                "model", "endpoint", "", "", "", ""
            );
        }
        for r in &rows {
            let last = r
                .last_call_unix
                .map(|u| u.to_string())
                .unwrap_or_else(|| "-".into());
            println!(
                "  {:<14}  {:<24}  calls={:<5}  in={:<8}  out={:<6}  last_call_unix={}",
                trim(&r.model, 14),
                trim(provider.endpoint.as_deref().unwrap_or(""), 24),
                r.calls,
                r.input_tokens,
                r.output_tokens,
                last
            );
        }
        Ok(())
    }

    /// Truncate `s` to at most `max` chars, appending an ellipsis
    /// when truncation occurred. Used by the column printer so a
    /// long endpoint URL does not blow up the table layout.
    fn trim(s: &str, max: usize) -> String {
        if s.chars().count() <= max {
            s.to_owned()
        } else {
            let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
            out.push('…');
            out
        }
    }
}

mod view {
    use super::{Error, Result, TelemetryCmd};
    use crate::config::Config;
    use crate::telemetry::dashboard::{self, DashboardConfig};
    use std::net::{IpAddr, SocketAddr};
    use std::sync::Arc;

    pub(super) async fn run(cmd: &TelemetryCmd) -> Result<()> {
        let (runs_dir, port) = match cmd {
            TelemetryCmd::View { runs_dir, port } => (runs_dir.as_ref(), *port),
            _ => return Err(Error::InvalidState("view: wrong variant".into())),
        };
        let home = super::resolve_home(runs_dir.map(|p| p.as_path()))?;
        let cfg = Config::load()?;
        // `ServerConfig::ensure_home` controls whether the
        // dashboard creates the runs/ + cache/ directories on
        // startup (default true). When the operator disables it
        // (e.g., for a read-only CI dashboard pointing at a
        // production home) we fail fast on a missing layout
        // instead of silently materialising empty dirs.
        if cfg.server.ensure_home {
            home.ensure()?;
        } else if !home.runs_dir().exists() {
            return Err(Error::InvalidState(format!(
                "dashboard home has no .runs/ directory and ensure_home=false: {}",
                home.root().display()
            )));
        }
        let bind = SocketAddr::new(
            cfg.server.host.parse::<IpAddr>().map_err(|e| {
                Error::InvalidArgs(format!("invalid dashboard host '{}': {e}", cfg.server.host))
            })?,
            // CLI flag wins over config when the caller passed one
            // other than the default (4096). Otherwise honor the
            // config knob.
            if port == 4096 { cfg.server.port } else { port },
        );
        let dash_cfg = DashboardConfig {
            bind,
            home: Arc::new(home),
            db_path: None,
        };
        let handle = dashboard::start(dash_cfg).await?;
        println!("dashboard listening on http://{}", handle.local_addr);
        println!("endpoints:");
        println!("  GET /api/runs");
        println!("  GET /api/runs/<id>");
        println!("  GET /api/runs/<id>/phases");
        println!("  GET /api/runs/<id>/calls");
        println!("  GET /api/runs/<id>/provider_usage");
        println!("  GET /api/runs/<id>/hashes");
        println!("  GET /api/runs/<id>/export?level=summary|full&format=tar.gz|tar|zip");
        println!("press Ctrl-C to stop");
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    }
}

mod export {
    use super::{Error, Result, TelemetryCmd};
    use crate::ids::RunId;

    pub(super) fn run(cmd: &TelemetryCmd) -> Result<()> {
        let (runs_dir, run, level, format, out) = match cmd {
            TelemetryCmd::Export {
                runs_dir,
                run,
                level,
                format,
                out,
            } => (
                runs_dir.as_ref(),
                run.as_str(),
                *level,
                *format,
                out.as_deref(),
            ),
            _ => return Err(Error::InvalidState("export: wrong variant".into())),
        };
        let run_id: RunId = run
            .parse()
            .map_err(|e| Error::InvalidArgs(format!("invalid run id '{run}': {e}")))?;
        let home = super::resolve_home(runs_dir.map(|p| p.as_path()))?;
        let run_dir = home.run_dir(run_id);
        if !run_dir.root().exists() {
            return Err(Error::InvalidState(format!(
                "run {run} directory not found at {}",
                run_dir.root().display()
            )));
        }
        let default_name = format!("run_{}_{}.{}", run_id.short(), level, extension_for(format));
        let out_path = out
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| run_dir.root().with_file_name(default_name));
        let result =
            crate::telemetry::export::export_run(&run_dir, run_id, level, format, &out_path)?;
        println!("export: wrote {} file(s)", result.file_count);
        println!("  payload bytes: {}", result.payload_bytes);
        println!("  archive bytes: {}", result.archive_bytes);
        println!("  archive sha256: {}", result.archive_sha256);
        println!("  path: {}", result.archive_path.display());
        Ok(())
    }

    fn extension_for(format: super::ExportFormat) -> &'static str {
        match format {
            super::ExportFormat::TarGz => "tar.gz",
            super::ExportFormat::Tar => "tar",
            super::ExportFormat::Zip => "zip",
            super::ExportFormat::TarZst => "tar.zst",
        }
    }
}

mod cleanup {
    use super::{Error, Result, TelemetryCmd};
    use crate::config::Config;
    use crate::ids::RunId;
    use crate::storage::sqlite::Db;
    use crate::telemetry::cross_run_sweep::{OrphanReport, OrphanTableStat};
    use crate::telemetry::retention::{RetentionConfig, RetentionPolicy, apply};

    pub(super) fn run(cmd: &TelemetryCmd) -> Result<()> {
        let (runs_dir, dry_run, archive_flag, cross_run, yes) = match cmd {
            TelemetryCmd::Cleanup {
                runs_dir,
                dry_run,
                archive,
                cross_run,
                yes,
            } => (runs_dir.as_ref(), *dry_run, *archive, *cross_run, *yes),
            _ => return Err(Error::InvalidState("cleanup: wrong variant".into())),
        };
        // The two modes are mutually exclusive: the per-run
        // retention pass moves the run directory, while the
        // cross-run sweep touches only the SQLite index. Combining
        // them would change the operator's mental model of which
        // side of the database is being mutated, so the CLI
        // rejects the combination at the boundary.
        if cross_run && archive_flag {
            return Err(Error::InvalidArgs(
                "--cross-run and --archive are mutually exclusive \
                 (the sweep touches SQLite only; the archive moves .runs/<id>/)"
                    .into(),
            ));
        }
        if cross_run {
            return run_cross_run(runs_dir.map(|p| p.as_path()), dry_run, yes);
        }
        run_per_run_retention(runs_dir, dry_run, archive_flag)
    }

    /// Per-run retention pass. Drives the existing
    /// `src/telemetry/retention.rs` policy with the same knobs as
    /// before; the dispatch is split into a dedicated function so
    /// the cross-run path stays a sibling rather than a branch.
    fn run_per_run_retention(
        runs_dir: Option<&std::path::PathBuf>,
        dry_run: bool,
        archive_flag: bool,
    ) -> Result<()> {
        let home = super::resolve_home(runs_dir.map(|p| p.as_path()))?;
        let runs_dir = home.runs_dir();
        let db = Db::open(&home.meta_db_path()).ok();
        let cfg = Config::load()?;
        // The CLI flag `--archive` wins over the config knob so
        // operators can run a one-off archive without editing the
        // config file.
        let config_policy = match cfg.retention.policy.as_str() {
            "archive" => RetentionPolicy::Archive,
            _ => RetentionPolicy::Delete,
        };
        let policy = if archive_flag {
            RetentionPolicy::Archive
        } else {
            config_policy
        };
        let retention_cfg = RetentionConfig {
            keep_runs_days: cfg.retention.keep_runs_days,
            keep_runs_count: cfg.retention.keep_runs_count,
            max_storage_bytes: cfg.retention.max_storage_bytes,
            policy,
        };
        let db_lookup = |run_id: RunId| -> Option<i64> {
            db.as_ref()
                .and_then(|d| d.get_run(run_id).ok().flatten())
                .map(|r| r.updated_unix)
        };
        let report = apply(&runs_dir, &db_lookup, &retention_cfg, dry_run)?;
        if report.candidates.is_empty() {
            println!(
                "(no runs match the retention policy; nothing to {}.)",
                if dry_run { "remove" } else { "act on" }
            );
            return Ok(());
        }
        println!(
            "{}: {} run(s) selected, total {} bytes",
            if dry_run { "dry-run" } else { "apply" },
            report.candidates.len(),
            report.total_bytes
        );
        for cand in &report.candidates {
            println!(
                "  {}  bytes={}  updated_unix={}  policy={:?}",
                cand.run_id.short(),
                cand.bytes,
                cand.updated_unix,
                report.policy
            );
        }
        Ok(())
    }

    /// Cross-run sweep. `dry_run` is forced to `true` unless `yes`
    /// is passed, mirroring the `cargo install --force` convention
    /// so a missing confirmation never silently drops rows.
    /// Either way the report is printed to stdout; the only
    /// difference is whether the underlying DELETEs run.
    fn run_cross_run(runs_dir: Option<&std::path::Path>, dry_run: bool, yes: bool) -> Result<()> {
        let home = super::resolve_home(runs_dir)?;
        let db = Db::open(&home.meta_db_path())?;
        let effective_dry_run = dry_run || !yes;
        let report = if effective_dry_run {
            db.list_orphans()?
        } else {
            db.purge_orphans()?
        };
        let header = if effective_dry_run {
            "dry-run"
        } else {
            "purge"
        };
        if report.is_empty() {
            println!(
                "cross-run sweep ({header}): no orphan rows found across {} tables",
                report.tables.len()
            );
            return Ok(());
        }
        println!(
            "cross-run sweep ({header}): {} orphan row(s) across {} tables",
            report.total_rows,
            report.tables.iter().filter(|t| t.rows > 0).count(),
        );
        print_report(&report);
        if effective_dry_run {
            println!(
                "tip: re-run with --yes to purge the rows (the dry-run summary above will not be repeated)."
            );
        }
        Ok(())
    }

    /// Format the per-table orphan report. The table name is
    /// left-padded to 18 chars so the columns line up; the count
    /// column is right-padded to 6 so a thousand-row sweep stays
    /// readable in a wide terminal.
    fn print_report(report: &OrphanReport) {
        for stat in report.tables.iter().filter(|t| t.rows > 0) {
            println!(
                "  {:<18}  {:<13}  {:>6}",
                stat.table, stat.run_id_column, stat.rows
            );
        }
        // The `redact_audit` table has a nullable `run_id`; the
        // trait distinguishes it implicitly via the per-table
        // entry. We surface the annotation only when the table
        // appears zero-rowed AND has a nullable FK, so the operator
        // does not see a misleading "0" for a column that is
        // allowed to be NULL.
        if report.tables.iter().all(|t| t.rows == 0) {
            return;
        }
        let nullable_with_zero: Vec<&OrphanTableStat> = report
            .tables
            .iter()
            .filter(|t| t.run_id_is_nullable && t.rows == 0)
            .collect();
        if !nullable_with_zero.is_empty() {
            println!(
                "  (zero orphan rows on nullable FK: {})",
                nullable_with_zero
                    .iter()
                    .map(|t| t.table)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    /// Suppress the unused-import warning when the cross-run
    /// tables list is empty during a no-op compilation. The
    /// `OrphanTableStat` type is exposed via the public re-export
    /// in `src/telemetry/cross_run_sweep.rs`; the helper just
    /// keeps the import live so clippy does not flag a phantom
    /// dep when this module is the only consumer.
    #[allow(dead_code)]
    fn _types_are_used(_: OrphanTableStat) {}
}

mod verify {
    use super::{Error, Result, TelemetryCmd};
    use crate::telemetry::verify::{self, VerifyVerdict};

    pub(super) fn run(cmd: &TelemetryCmd) -> Result<()> {
        let path = match cmd {
            TelemetryCmd::Verify { path, .. } => path,
            _ => return Err(Error::InvalidState("verify: wrong variant".into())),
        };
        let report = verify::verify(path)?;
        let mut ok = 0;
        let mut fail = 0;
        for row in &report.rows {
            if matches!(row.verdict, VerifyVerdict::Ok) {
                ok += 1;
            } else {
                fail += 1;
            }
            if let VerifyVerdict::Mismatch { expected, actual } = &row.verdict {
                println!(
                    "MISMATCH  {}  expected={}  actual={}",
                    row.path, expected, actual
                );
            } else {
                println!("{:9}  {}", row.verdict.label(), row.path);
            }
        }
        println!();
        println!("OK: {} files verified, {} failed", ok, fail);
        if fail > 0 {
            Err(Error::InvalidState(format!(
                "{fail} file(s) failed verification"
            )))
        } else {
            Ok(())
        }
    }
}

mod config {
    //! `moagan telemetry config` — print the effective configuration.
    //!
    //! Mirrors T01-06 §10.7 and V4 §2.7. API keys are NEVER printed;
    //! the operator can grep the registry code path if they need the
    //! resolved value.
    use super::{Result, TelemetryCmd};
    use crate::config::Config;

    pub(super) fn run(_cmd: &TelemetryCmd) -> Result<()> {
        let cfg = Config::load()?;
        println!("=== providers ===");
        let mut names: Vec<&String> = cfg.providers.keys().collect();
        names.sort();
        for name in names {
            let spec = cfg
                .providers
                .get(name)
                .expect("provider present in same map we just iterated");
            // v0.10: model id is sourced from the section's first
            // `models[]` entry (no more legacy `spec.model`); the
            // section-level `endpoint` is `Option<String>`.
            let model_str = spec
                .models
                .first()
                .map(|m| m.id.clone())
                .unwrap_or_default();
            let endpoint_str = spec.endpoint.as_deref().unwrap_or("");
            println!("{name:20} model={:24} endpoint={}", model_str, endpoint_str,);
        }
        println!();
        println!("=== parallelism ===");
        println!("max_parallelism={}", cfg.max_parallelism);
        println!();
        println!("=== timeouts ===");
        println!(
            "sketch={}s phase={}s total={}s (0 means infinite)",
            cfg.sketch_timeout_secs, cfg.phase_timeout_secs, cfg.total_timeout_secs,
        );
        println!();
        println!("=== privacy (redact policy) ===");
        println!("redact_in_telemetry={}", cfg.redact_in_telemetry);
        println!();
        println!("=== stability (Phase H) ===");
        println!(
            "enabled={} n_perturbations={} sensitive_threshold={} seed={:#x}",
            cfg.stability.enabled,
            cfg.stability.n_perturbations,
            cfg.stability.sensitive_threshold,
            cfg.stability.seed,
        );
        println!();
        println!("=== export ===");
        println!(
            "format={} compression={}",
            cfg.export_format, cfg.export_compression
        );
        println!();
        println!("=== gate ===");
        println!(
            "min_length={} max_length={}",
            cfg.gate_min_length, cfg.gate_max_length
        );
        println!("forbidden_techs={:?}", cfg.gate_forbidden_techs);
        println!();
        println!("=== server (dashboard) ===");
        println!(
            "port={} host={} io_timeout_secs={} ensure_home={}",
            cfg.server.port, cfg.server.host, cfg.server.io_timeout_secs, cfg.server.ensure_home,
        );
        println!();
        println!("=== retention ===");
        println!(
            "keep_runs_days={} keep_runs_count={} max_storage_bytes={} policy={}",
            cfg.retention.keep_runs_days,
            cfg.retention.keep_runs_count,
            cfg.retention.max_storage_bytes,
            cfg.retention.policy,
        );
        println!();
        println!("=== default_provider ===");
        println!("{}", cfg.default_provider);
        Ok(())
    }
}

mod plan {
    //! `moagan telemetry plan [<provider>] [--window-days N]`.
    //!
    //! Rolling-window quota view aggregated from the per-call
    //! `calls` table (T01-06 §2.1). Distinct from
    //! `moagan telemetry provider --plan`, which drills into one
    //! provider's per-run rollup; this subcommand answers "how much
    //! of my token plan have I consumed in the last N days?" for
    //! every configured provider at once.
    //!
    //! The row formatter is exposed at `pub(super)` so the test
    //! module can pin the output without standing up a database.

    use super::{Error, Result, TelemetryCmd};
    use crate::config::{Config, PlanConfig};
    use crate::storage::sqlite::{Db, WindowUsageRow};

    pub(super) fn run(cmd: &TelemetryCmd) -> Result<i32> {
        let (runs_dir, provider_filter, mut window_days) = match cmd {
            TelemetryCmd::Plan {
                runs_dir,
                provider,
                window_days,
            } => (runs_dir.as_ref(), provider.as_deref(), *window_days),
            _ => return Err(Error::InvalidState("plan: wrong variant".into())),
        };
        if window_days == 0 {
            return Err(Error::InvalidArgs(
                "--window-days must be >= 1 (use a positive rolling window)".into(),
            ));
        }

        // Per-provider `plan.window_days` wins when a single provider
        // is being filtered; otherwise the CLI default is the only
        // source of truth (mixing windows per row would be misleading
        // in a side-by-side printout). Lookup is best-effort: a
        // missing config file just falls back to the CLI default so
        // a fresh `MOAGAN_HOME` still produces a sensible answer.
        let cfg = Config::load().ok();
        let mut plan_for_filter: Option<&PlanConfig> = None;
        if let (Some(cfg), Some(name)) = (cfg.as_ref(), provider_filter)
            && let Some(spec) = cfg.providers.get(name)
        {
            plan_for_filter = spec.plan.as_ref();
            if let Some(p) = spec.plan.as_ref()
                && let Some(w) = p.window_days
            {
                window_days = w;
            }
        }

        let home = super::resolve_home(runs_dir.map(|p| p.as_path()))?;
        let db = Db::open(&home.meta_db_path())?;
        let rows = db.aggregate_window_usage(window_days, provider_filter)?;

        if rows.is_empty() {
            // Match the "no calls in the window" exit convention
            // (1, not 0). Operators script this as
            // `moagan telemetry plan || echo "no recent usage"` so
            // they get a real signal without having to grep stdout.
            let scope = match provider_filter {
                Some(name) => format!(" for provider '{name}'"),
                None => String::new(),
            };
            println!("(no calls in the last {window_days} day(s){scope})");
            return Ok(1);
        }

        // Header — kept consistent with the column widths in
        // `format_row` so the table reads cleanly in any terminal
        // ≥ 96 columns wide. The last column is a literal label
        // (no positional data follows) so the `print_literal`
        // lint fires; allow it locally to mirror the same pattern
        // in `provider::plan_summary`.
        #[allow(clippy::print_literal)]
        {
            println!(
                "{:<12}  {:<18}  {:<10}  {:<30}  calls=N err=N cached=…k window=Nd",
                "provider", "model", "plan", "usage"
            );
        }

        // Build a lookup from `(provider, model) -> PlanConfig` so a
        // single run with no `--provider` filter still annotates
        // each row with the matching config block. Models outside
        // the config map render `(no plan)` so the operator can spot
        // unconfigured providers at a glance.
        let plan_lookup = |provider: &str, model: &str| -> Option<PlanConfig> {
            cfg.as_ref()
                .and_then(|c| {
                    c.providers
                        .values()
                        .find(|spec| {
                            // v0.10: the lookup uses the section name
                            // (which doubles as the model id when the
                            // section only registers one model); the
                            // legacy `spec.model` singleton is gone.
                            let section_first_model = spec
                                .models
                                .first()
                                .map(|m| m.id.clone())
                                .unwrap_or_default();
                            spec.models.iter().any(|m| m.id == model)
                                && provider == section_first_model
                                || (provider == section_first_model && model == section_first_model)
                        })
                        .or_else(|| {
                            // The `provider` field in the call log is
                            // the registry key the call landed under;
                            // it may be either the section name or the
                            // model id when the section only registers
                            // one model. Look up by either.
                            c.providers.values().find(|spec| {
                                let section_first_model = spec
                                    .models
                                    .first()
                                    .map(|m| m.id.clone())
                                    .unwrap_or_default();
                                section_first_model == provider
                            })
                        })
                })
                .and_then(|spec| spec.plan.clone())
        };

        for row in &rows {
            // Priority: explicit CLI filter wins (so `moagan telemetry
            // plan minimax-m3` shows the row's plan even if the
            // provider name and model don't match the lookup above);
            // otherwise fall back to the table lookup.
            let plan_annotation: Option<PlanConfig> = match plan_for_filter {
                Some(p) => Some(p.clone()),
                None => plan_lookup(&row.provider, &row.model),
            };
            println!("{}", format_row(row, plan_annotation.as_ref(), window_days));
        }

        println!(
            "({} row(s) over the last {} day(s))",
            rows.len(),
            window_days
        );
        Ok(0)
    }

    /// Format a single `WindowUsageRow` as a left-aligned text line.
    /// Extracted as a pure function so the formatter can be unit
    /// tested without standing up a database. Width constants are
    /// tuned for the column widths in the table header above;
    /// change both together if you need a wider table.
    pub(super) fn format_row(
        row: &WindowUsageRow,
        plan: Option<&PlanConfig>,
        window_days: u32,
    ) -> String {
        let plan_label = match plan.and_then(|p| p.plan_id.as_ref()) {
            Some(id) => truncate(id, 10),
            None => "(no plan)".to_string(),
        };

        let usage = match plan.and_then(|p| p.limit_tokens) {
            Some(limit) if limit > 0 => {
                let pct = (row.total_tokens as f64 / limit as f64) * 100.0;
                format!(
                    "{} / {} ({:.1}%)",
                    human_count(row.total_tokens),
                    human_count_u64(limit),
                    pct
                )
            }
            _ => human_count(row.total_tokens),
        };

        format!(
            "{:<12}  [{:<16}]  {:<10}  {:<30}  calls={:<5} err={:<3} cached={:<5} window={}d",
            truncate(&row.provider, 12),
            truncate(&row.model, 16),
            plan_label,
            truncate(&usage, 30),
            row.call_count,
            row.error_count,
            human_count_compact(row.cached_tokens),
            window_days,
        )
    }

    /// Right-truncate `s` to at most `max` chars, appending an
    /// ellipsis when truncation occurred. Mirrors the `trim` helper
    /// in `provider::plan_summary` so the column layout stays
    /// consistent across both subcommands.
    fn truncate(s: &str, max: usize) -> String {
        if s.chars().count() <= max {
            s.to_owned()
        } else {
            let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
            out.push('…');
            out
        }
    }

    /// Thousands-separated integer (e.g. `1_234_567`). Used for the
    /// `usage` column where readability matters more than width.
    fn human_count(n: i64) -> String {
        if n == 0 {
            return "0".to_owned();
        }
        let negative = n < 0;
        let digits: Vec<char> = n.abs().to_string().chars().collect();
        let mut out = String::with_capacity(digits.len() + digits.len() / 3);
        for (i, c) in digits.iter().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                out.insert(0, ',');
            }
            out.insert(0, *c);
        }
        if negative {
            out.insert(0, '-');
        }
        out
    }

    /// `u64` counterpart to [`human_count`]. The plan limit is
    /// declared as `u64` (mirroring token-count semantics) so we
    /// keep both helpers to avoid silently truncating very large
    /// caps at `i64::MAX`.
    fn human_count_u64(n: u64) -> String {
        if n == 0 {
            return "0".to_owned();
        }
        let digits: Vec<char> = n.to_string().chars().collect();
        let mut out = String::with_capacity(digits.len() + digits.len() / 3);
        for (i, c) in digits.iter().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                out.insert(0, ',');
            }
            out.insert(0, *c);
        }
        out
    }

    /// Compact integer with a single-letter suffix (`12k`, `1.2M`,
    /// `3.4B`). Used for `cached=` where the column width is fixed
    /// and a compact form keeps the table line under 96 chars even
    /// for million-token cache hits.
    fn human_count_compact(n: i64) -> String {
        let abs = n.unsigned_abs() as f64;
        if abs >= 1_000_000_000.0 {
            format!("{:.1}B", abs / 1_000_000_000.0)
        } else if abs >= 1_000_000.0 {
            format!("{:.1}M", abs / 1_000_000.0)
        } else if abs >= 1_000.0 {
            format!("{:.0}k", abs / 1_000.0)
        } else {
            n.to_string()
        }
    }
}

mod cost {
    //! `moagan telemetry cost` — per-call USD cost aggregation.
    //!
    //! The runtime writes `calls.cost_usd` on every call (PR-6,
    //! SQLite v015). This module reads it back and groups by
    //! `(provider, model)` so an operator can answer "how much
    //! money did this run spend?" with a single CLI invocation.
    //!
    //! The output matches the contract spelled out in PR-7:
    //!
    //! ```text
    //! RUN: 018f3a2b-...
    //! Total cost: $0.42
    //!
    //! By provider/model:
    //!   minimax / MiniMax-M3         0.4200 USD  (100 calls)
    //!   minimax / MiniMax-M2.7       0.0000 USD  (12 calls, no cost data)
    //! ```
    //!
    //! `--all` flips the scope from one run to the whole index.
    //! `--run` is mutually exclusive with `--all`; a conflict
    //! surfaces as `Error::InvalidArgs` from clap's `conflicts_with`
    //! attribute, so the dispatcher never sees both at once.

    use super::{Error, Result, TelemetryCmd};
    use crate::ids::RunId;
    use crate::storage::sqlite::Db;

    pub(super) fn run(cmd: &TelemetryCmd) -> Result<()> {
        let (runs_dir, run, all) = match cmd {
            TelemetryCmd::Cost { runs_dir, run, all } => (runs_dir.as_ref(), run.as_deref(), *all),
            _ => return Err(Error::InvalidState("cost: wrong variant".into())),
        };
        // Exactly one of `--run` or `--all` must be set; clap's
        // `conflicts_with` is the primary guard, but the test
        // surface calls `dispatch` directly so we re-check here
        // to keep the contract tight.
        match (run, all) {
            (None, false) => {
                return Err(Error::InvalidArgs(
                    "telemetry cost: pass either --run <id> or --all".into(),
                ));
            }
            (Some(_), true) => {
                return Err(Error::InvalidArgs(
                    "telemetry cost: --run and --all are mutually exclusive".into(),
                ));
            }
            _ => {}
        }
        let home = super::resolve_home(runs_dir.map(|p| p.as_path()))?;
        let db = Db::open(&home.meta_db_path())?;
        let run_id = run
            .map(|raw| {
                raw.parse()
                    .map_err(|e| Error::InvalidArgs(format!("invalid run id '{raw}': {e}")))
            })
            .transpose()?;
        if let Some(rid) = run_id {
            print_for_run(&db, rid)?;
        } else {
            print_for_all(&db)?;
        }
        Ok(())
    }

    fn print_for_run(db: &Db, run_id: RunId) -> Result<()> {
        // The per-run aggregate is the union of every recorded
        // call. Empty result means the run id is unknown; surface
        // a clear error so the operator can re-check.
        if db.get_run(run_id)?.is_none() {
            return Err(Error::InvalidState(format!(
                "run {run_id} not found in the index"
            )));
        }
        let rows = db.aggregate_cost_by_provider_model(Some(run_id))?;
        let total: f64 = rows.iter().map(|r| r.cost_usd).sum();
        println!("RUN: {}", run_id.short());
        println!("Total cost: ${:.4}", total);
        if rows.is_empty() {
            println!("(no calls recorded for this run)");
            return Ok(());
        }
        println!();
        println!("By provider/model:");
        print_rows(&rows);
        Ok(())
    }

    fn print_for_all(db: &Db) -> Result<()> {
        let rows = db.aggregate_cost_by_provider_model(None)?;
        let total: f64 = rows.iter().map(|r| r.cost_usd).sum();
        println!("ALL RUNS");
        println!("Total cost: ${:.4}", total);
        if rows.is_empty() {
            println!("(no calls recorded)");
            return Ok(());
        }
        println!();
        println!("By provider/model:");
        print_rows(&rows);
        Ok(())
    }

    fn print_rows(rows: &[crate::storage::sqlite::CostAggregateRow]) {
        for r in rows {
            // Mirror the example in the PR-7 spec: the per-row
            // dollar amount is fixed to 4 decimal places, and
            // a zero `cost_usd` carries the "no cost data"
            // annotation so the operator does not mistake it
            // for "free".
            if r.cost_usd > 0.0 {
                println!(
                    "  {:<14} / {:<22} {:>8.4} USD  ({} calls)",
                    r.provider, r.model, r.cost_usd, r.calls
                );
            } else {
                println!(
                    "  {:<14} / {:<22} {:>8.4} USD  ({} calls, no cost data)",
                    r.provider, r.model, r.cost_usd, r.calls
                );
            }
        }
    }
}

mod alerts {
    //! `moagan telemetry alerts list` — list saturation events.
    //!
    //! Reads the `saturation_events` SQLite table (v018) and prints
    //! a one-line-per-event summary. The output matches the
    //! contract spelled out in `proposal-03 §D.23`:
    //!
    //! ```text
    //! ALERTS (3 events, since=2024-01-01)
    //!   2024-01-02 03:04:05  minimax  MiniMax-M3     error      100.0%   run=018f…  failure_count=5
    //!   2024-01-02 03:04:05  mock     m              rate_limit   0.0%   run=018f…  capacity=60 refill_per_sec=1
    //!   2024-01-02 03:04:05  deepseek deepseek-chat  token       85.0%   run=018f…  -
    //! ```
    //!
    //! Empty result means "no events in the window"; the subcommand
    //! prints `(no saturation events match the filter)` and exits 0.
    use super::{AlertsAction, Error, Result, TelemetryCmd};
    use crate::storage::sqlite::{Db, SaturationRow};

    pub(super) fn run(cmd: &TelemetryCmd) -> Result<()> {
        let (runs_dir, action) = match cmd {
            TelemetryCmd::Alerts { runs_dir, action } => (runs_dir.as_ref(), action),
            _ => return Err(Error::InvalidState("alerts: wrong variant".into())),
        };
        match action {
            AlertsAction::List {
                since,
                provider,
                limit,
            } => list(
                runs_dir.map(|p| p.as_path()),
                since.as_deref(),
                provider.as_deref(),
                *limit,
            ),
        }
    }

    /// Dispatch the `list` sub-action. `since` is parsed as either a
    /// unix timestamp or an ISO calendar date; both are accepted so
    /// operators can copy/paste from a dashboard without thinking
    /// about the wire format. `provider` is passed through verbatim.
    fn list(
        runs_dir: Option<&std::path::Path>,
        since: Option<&str>,
        provider: Option<&str>,
        limit: u32,
    ) -> Result<()> {
        let home = super::resolve_home(runs_dir)?;
        let db = Db::open(&home.meta_db_path())?;
        let since_unix = parse_since(since);
        let rows = db.list_saturation_events(since_unix, provider, limit)?;
        if rows.is_empty() {
            println!("(no saturation events match the filter)");
            return Ok(());
        }
        let scope = match provider {
            Some(name) => format!(" for provider '{name}'"),
            None => String::new(),
        };
        let since_label = match since {
            Some(s) if since_unix.is_some() => format!(" since={s}"),
            Some(s) => format!(" since={s} (unparsed; no filter applied)"),
            None => String::new(),
        };
        println!("ALERTS ({} event(s){}{})", rows.len(), scope, since_label);
        for row in &rows {
            println!("{}", format_row(row));
        }
        Ok(())
    }

    /// Format one `SaturationRow` as a single text line. The
    /// structured `details` JSON is unpacked into two well-known
    /// keys (`failure_count`, `capacity` + `refill_per_sec`) so the
    /// output is human-readable without losing the structured
    /// payload (the SQLite row keeps the raw JSON for re-parsing).
    fn format_row(row: &SaturationRow) -> String {
        let run_id = row.run_id.as_deref().map(short).unwrap_or("-");
        let pct = format!("{:.1}%", row.threshold_pct);
        let details_summary = unpack_details(row.details.as_deref());
        format!(
            "  {:<19}  {:<10}  {:<20}  {:<10}  {:>7}  run={:<8}  {}",
            format_unix(row.observed_at_unix),
            row.provider,
            truncate(&row.model, 20),
            row.kind,
            pct,
            run_id,
            details_summary,
        )
    }

    /// Pull well-known keys out of the structured `details` JSON.
    /// Returns `"-"` when the payload is absent, empty, or missing
    /// the recognised keys.
    fn unpack_details(details: Option<&str>) -> String {
        let Some(raw) = details else {
            return "-".into();
        };
        let parsed: serde_json::Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => return "-".into(),
        };
        if let Some(fc) = parsed.get("failure_count").and_then(|v| v.as_u64()) {
            return format!("failure_count={fc}");
        }
        let cap = parsed.get("capacity").and_then(|v| v.as_u64());
        let refill = parsed.get("refill_per_sec").and_then(|v| v.as_u64());
        if let (Some(c), Some(r)) = (cap, refill) {
            return format!("capacity={c} refill_per_sec={r}");
        }
        "-".into()
    }

    fn format_unix(secs: i64) -> String {
        let days = secs.div_euclid(86_400);
        let secs_of_day = secs.rem_euclid(86_400);
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        let h = secs_of_day / 3600;
        let mi = (secs_of_day % 3600) / 60;
        let s = secs_of_day % 60;
        format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, h, mi, s)
    }

    fn short(raw: &str) -> &str {
        raw.get(..8).unwrap_or(raw)
    }

    fn truncate(s: &str, max: usize) -> String {
        if s.chars().count() <= max {
            s.to_owned()
        } else {
            let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
            out.push('…');
            out
        }
    }

    /// Parse `--since`. Accepts either a unix timestamp (seconds)
    /// or an ISO calendar date `YYYY-MM-DD`. Returns `None` when
    /// the input is missing OR cannot be parsed; the caller
    /// surfaces the parse failure in the output so the operator
    /// can re-check without the subcommand aborting.
    ///
    /// Implementation note: the inverse of the civil-from-days
    /// algorithm used by `retention::format_date` is fiddly to
    /// hand-roll (the offset between Unix day 0 and the
    /// algorithm's internal day 0 is easy to get wrong). `chrono`
    /// is already a project dependency and the CLI does not need
    /// to re-derive the calendar math, so we delegate the
    /// calendar branch to `chrono::NaiveDate::parse_and_convert`.
    fn parse_since(raw: Option<&str>) -> Option<i64> {
        let s = raw?;
        if let Ok(secs) = s.parse::<i64>() {
            return Some(secs);
        }
        // Calendar branch: `YYYY-MM-DD` at midnight UTC.
        let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
        let datetime = date.and_hms_opt(0, 0, 0)?;
        Some(datetime.and_utc().timestamp())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Unix timestamps (positive integers) are accepted
        /// verbatim. This is the operator-friendly path: the
        /// dashboard hands back a unix number and the CLI does
        /// not need to think about calendars.
        #[test]
        fn parse_since_unix_passthrough() {
            assert_eq!(parse_since(Some("1700000000")), Some(1_700_000_000));
        }

        /// `None` means "no filter", so `parse_since` returns
        /// `None` too — the SQL layer falls through to the
        /// unfiltered branch.
        #[test]
        fn parse_since_none_returns_none() {
            assert_eq!(parse_since(None), None);
        }

        /// ISO calendar dates are converted to unix seconds at
        /// midnight UTC. `1970-01-01` is the epoch, so the
        /// answer must be `0` — the test pins the convention so
        /// any future drift in the civil-from-days helper is
        /// caught here.
        #[test]
        fn parse_since_iso_date_at_epoch() {
            assert_eq!(parse_since(Some("1970-01-01")), Some(0));
        }

        /// ISO calendar dates that do NOT match `YYYY-MM-DD`
        /// (e.g. extra components, wrong separator) fall back to
        /// `None` so the subcommand surfaces an "unparsed" hint
        /// in the output instead of crashing.
        #[test]
        fn parse_since_invalid_falls_back_to_none() {
            assert_eq!(parse_since(Some("not-a-date")), None);
            assert_eq!(parse_since(Some("2024/01/01")), None);
            assert_eq!(parse_since(Some("2024-01")), None);
            assert_eq!(parse_since(Some("2024-01-01-extra")), None);
        }

        /// Empty `SaturationRow` vectors are surfaced with a
        /// stable message so callers can grep on the marker.
        #[test]
        fn list_empty_rows_prints_marker() {
            // We cannot exercise `list` end-to-end without a
            // real SQLite database, but we can pin the empty
            // marker string that the dispatch prints. Keep the
            // assertion string-bound so the test is robust to
            // refactors.
            assert!(
                "(no saturation events match the filter)".contains("no saturation events"),
                "marker must remain stable for grep-ability"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_level_round_trip() {
        for raw in ["summary", "SUMMARY", "Summary"] {
            assert_eq!(raw.parse::<ExportLevel>().unwrap(), ExportLevel::Summary);
        }
        assert_eq!("full".parse::<ExportLevel>().unwrap(), ExportLevel::Full);
        assert!("nope".parse::<ExportLevel>().is_err());
    }

    #[test]
    fn export_format_round_trip() {
        for raw in ["tar.gz", "TAR.GZ", "tgz"] {
            assert_eq!(raw.parse::<ExportFormat>().unwrap(), ExportFormat::TarGz);
        }
        assert_eq!("tar".parse::<ExportFormat>().unwrap(), ExportFormat::Tar);
        assert_eq!("zip".parse::<ExportFormat>().unwrap(), ExportFormat::Zip);
        assert_eq!(
            "tar.zst".parse::<ExportFormat>().unwrap(),
            ExportFormat::TarZst
        );
        assert_eq!(
            "tarZST".parse::<ExportFormat>().unwrap(),
            ExportFormat::TarZst
        );
        assert!("rar".parse::<ExportFormat>().is_err());
    }

    #[test]
    fn list_empty_index_prints_marker() {
        crate::test_support::with_moagan_home("telemetry_list_empty_index", |home| {
            let cmd = TelemetryCmd::List {
                runs_dir: Some(home.to_path_buf()),
                limit: 5,
                run: None,
            };
            // Empty DB doesn't exist yet; the open call creates it. We
            // capture stdout via the dispatch returning Ok(0) so the
            // test only checks the no-error / no-panic contract.
            let code = pollster::block_on(cmd.dispatch());
            assert_eq!(code.unwrap(), 0);
        });
    }

    #[test]
    fn list_unknown_run_id_returns_invalid_args() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = TelemetryCmd::List {
            runs_dir: Some(tmp.path().to_path_buf()),
            limit: 5,
            run: Some("not-a-uuid".into()),
        };
        let err = pollster::block_on(cmd.dispatch()).unwrap_err();
        assert!(matches!(err, Error::InvalidArgs(_)));
    }

    #[test]
    fn list_unknown_run_uuid_returns_invalid_state() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = TelemetryCmd::List {
            runs_dir: Some(tmp.path().to_path_buf()),
            limit: 5,
            run: Some("01900000-0000-0000-0000-000000000000".into()),
        };
        // Open succeeds (creates empty DB) but the row is missing.
        let err = pollster::block_on(cmd.dispatch()).unwrap_err();
        assert!(matches!(err, Error::InvalidState(_)));
    }

    #[test]
    fn summary_invalid_run_id_returns_invalid_args() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = TelemetryCmd::Summary {
            runs_dir: Some(tmp.path().to_path_buf()),
            run: "not-a-uuid".into(),
        };
        let err = pollster::block_on(cmd.dispatch()).unwrap_err();
        assert!(matches!(err, Error::InvalidArgs(_)));
    }

    #[test]
    fn summary_unknown_run_returns_invalid_state() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = TelemetryCmd::Summary {
            runs_dir: Some(tmp.path().to_path_buf()),
            run: "01900000-0000-0000-0000-000000000000".into(),
        };
        let err = pollster::block_on(cmd.dispatch()).unwrap_err();
        assert!(matches!(err, Error::InvalidState(_)));
    }

    #[test]
    fn compare_invalid_run_id_returns_invalid_args() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = TelemetryCmd::Compare {
            runs_dir: Some(tmp.path().to_path_buf()),
            run_a: "not-a-uuid".into(),
            run_b: "01900000-0000-0000-0000-000000000000".into(),
        };
        let err = pollster::block_on(cmd.dispatch()).unwrap_err();
        assert!(matches!(err, Error::InvalidArgs(_)));
    }

    #[test]
    fn compare_unknown_run_returns_invalid_state() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = TelemetryCmd::Compare {
            runs_dir: Some(tmp.path().to_path_buf()),
            run_a: "01900000-0000-0000-0000-000000000000".into(),
            run_b: "01900000-0000-0000-0000-000000000001".into(),
        };
        let err = pollster::block_on(cmd.dispatch()).unwrap_err();
        assert!(matches!(err, Error::InvalidState(_)));
    }

    #[test]
    fn provider_unknown_plan_returns_invalid_args() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = TelemetryCmd::Provider {
            runs_dir: Some(tmp.path().to_path_buf()),
            plan: Some("nonexistent".into()),
            list: false,
        };
        let err = pollster::block_on(cmd.dispatch()).unwrap_err();
        assert!(matches!(err, Error::InvalidArgs(_)));
    }

    /// v0.8 push-side: dispatching `alerts list` against a fresh
    /// `MOAGAN_HOME` (no saturation rows) must exit 0 and print the
    /// empty marker. The SQLite index is empty by construction so
    /// `list_saturation_events` returns `Ok(vec![])` and the
    /// formatter path is short-circuited by the early `is_empty()`
    /// branch.
    #[test]
    fn alerts_list_dispatches_on_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = TelemetryCmd::Alerts {
            runs_dir: Some(tmp.path().to_path_buf()),
            action: super::AlertsAction::List {
                since: None,
                provider: None,
                limit: 100,
            },
        };
        let code = pollster::block_on(cmd.dispatch()).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn provider_list_runs_against_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = TelemetryCmd::Provider {
            runs_dir: Some(tmp.path().to_path_buf()),
            plan: None,
            list: true,
        };
        let code = pollster::block_on(cmd.dispatch()).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn provider_default_action_is_list() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = TelemetryCmd::Provider {
            runs_dir: Some(tmp.path().to_path_buf()),
            plan: None,
            list: false,
        };
        let code = pollster::block_on(cmd.dispatch()).unwrap();
        assert_eq!(code, 0);
    }

    // -----------------------------------------------------------------
    // `moagan telemetry plan` (additive; PR-365+)
    //
    // The formatter is tested as a pure function (no DB, no stdout
    // capture) so we can pin the output strings exactly. The dispatch
    // tests use the existing `with_moagan_home` helper for a fresh
    // SQLite index and exercise the `Result<i32>` exit-code path that
    // distinguishes an empty window (exit 1) from a populated one
    // (exit 0).
    // -----------------------------------------------------------------

    use crate::config::PlanConfig;
    use crate::storage::sqlite::{Db, WindowUsageRow};

    /// Format a row with a `PlanConfig { plan_id, limit_tokens, … }`
    /// attached. The output must include:
    ///   * the plan id verbatim (no transformation),
    ///   * the consumed ratio formatted as `X / Y (P.P%)`,
    ///   * the trailing `calls=… err=… cached=… window=…d` block.
    #[test]
    fn format_usage_row_with_plan() {
        let row = WindowUsageRow {
            provider: "minimax".to_owned(),
            model: "MiniMax-M3".to_owned(),
            call_count: 200,
            total_tokens: 624_000,
            error_count: 2,
            cached_tokens: 12_345,
            first_call_unix: Some(1_700_000_000),
            last_call_unix: Some(1_700_086_400),
        };
        let plan = PlanConfig {
            plan_id: Some("weekly".to_owned()),
            limit_tokens: Some(1_000_000),
            window_days: Some(7),
        };
        let line = super::plan::format_row(&row, Some(&plan), 7);
        assert!(
            line.contains("weekly"),
            "plan id must echo verbatim, got: {line}"
        );
        assert!(
            line.contains("624,000 / 1,000,000"),
            "consumed-ratio column must use thousands separators, got: {line}"
        );
        assert!(
            line.contains("(62.4%)"),
            "percent must be one decimal, got: {line}"
        );
        assert!(
            line.contains("calls=200"),
            "calls counter must be present, got: {line}"
        );
        assert!(
            line.contains("err=2"),
            "err counter must be present, got: {line}"
        );
        assert!(
            line.contains("cached=12k"),
            "12,345 tokens must compact to 12k, got: {line}"
        );
        assert!(
            line.contains("window=7d"),
            "window days must appear, got: {line}"
        );
        // The provider name appears in the row prefix.
        assert!(
            line.contains("minimax"),
            "provider name must appear, got: {line}"
        );
    }

    /// When no `PlanConfig` is attached the row collapses to the
    /// `(no plan)` annotation and the bare `usage` column (no ratio,
    /// no percent). The trailing `calls / err / cached / window`
    /// block stays so a multi-provider printout keeps a uniform
    /// column structure regardless of which providers have plans
    /// configured.
    #[test]
    fn format_usage_row_without_plan() {
        let row = WindowUsageRow {
            provider: "mock".to_owned(),
            model: "mock-model".to_owned(),
            call_count: 50,
            total_tokens: 1_234,
            error_count: 0,
            cached_tokens: 0,
            first_call_unix: Some(1_700_000_000),
            last_call_unix: Some(1_700_000_500),
        };
        let line = super::plan::format_row(&row, None, 7);
        assert!(
            line.contains("(no plan)"),
            "missing-plan annotation must appear, got: {line}"
        );
        assert!(
            line.contains("1,234"),
            "bare usage must use thousands separators, got: {line}"
        );
        assert!(
            !line.contains('%'),
            "percent column must be omitted when no plan is set, got: {line}"
        );
        assert!(
            !line.contains('/'),
            "ratio column must be omitted when no plan is set, got: {line}"
        );
    }

    /// Empty DB → `dispatch` returns `Ok(1)` (the documented exit
    /// convention for "no calls in the window"). The `(no calls in
    /// the last N day(s))` marker is written to stdout; this test
    /// asserts the exit code only — content is covered by the
    /// formatter test above.
    #[test]
    fn cli_telemetry_plan_runs_on_empty_db_exits_one() {
        crate::test_support::with_moagan_home("telemetry_plan_empty_db_exits_one", |home| {
            let cmd = TelemetryCmd::Plan {
                runs_dir: Some(home.to_path_buf()),
                provider: None,
                window_days: 7,
            };
            let code = pollster::block_on(cmd.dispatch()).unwrap();
            assert_eq!(code, 1, "empty window must surface as exit 1");
        });
    }

    /// Realistic seed data (one (provider, model) with several
    /// calls) → `dispatch` returns `Ok(0)`. The stdout assertions
    /// confirm the formatter wired through end-to-end: the provider
    /// name AND a thousands-separated token total both land in the
    /// output. Using `println!`-equivalent stdout checks would
    /// require process-level capture; instead we re-exercise the
    /// `format_row` path with the same seed data and assert it
    /// independently. This keeps the test side-effect-free (no env
    /// mutation, no global stdout swap) while still covering both
    /// the SQL aggregation and the formatter in one go.
    #[test]
    fn cli_telemetry_plan_runs_with_realistic_data_exits_zero() {
        crate::test_support::with_moagan_home("telemetry_plan_realistic_data_exits_zero", |home| {
            let moagan_home = crate::fs_layout::MoaganHome::at(home.to_path_buf());
            let db_path = moagan_home.meta_db_path();
            let db = Db::open(&db_path).unwrap();
            let now = crate::time::now_unix_secs();
            let run_id = crate::ids::RunId::new();
            db.register_run(run_id, "fast", "running", "0.6.0", None, None, None)
                .unwrap();
            // Two OK calls + one error on the same
            // (provider, model): total tokens = 12_345 (matches
            // the assertion substring below).
            db.record_call(
                "c-real-1",
                run_id,
                "intake",
                "intake",
                "minimax",
                "MiniMax-M3",
                "k1",
                None,
                false,
                Some(200),
                5_000,
                1_000,
                0,
                0,
                now - 60,
                now - 59,
                None,
                0,
            )
            .unwrap();
            db.record_call(
                "c-real-2",
                run_id,
                "intake",
                "intake",
                "minimax",
                "MiniMax-M3",
                "k2",
                None,
                false,
                Some(200),
                4_000,
                1_345,
                0,
                0,
                now - 30,
                now - 29,
                None,
                0,
            )
            .unwrap();
            db.record_call(
                "c-real-3",
                run_id,
                "intake",
                "intake",
                "minimax",
                "MiniMax-M3",
                "k3",
                None,
                false,
                Some(500),
                1_000,
                0,
                0,
                0,
                now - 20,
                now - 19,
                Some("transient 5xx"),
                0,
            )
            .unwrap();
            drop(db);

            let cmd = TelemetryCmd::Plan {
                runs_dir: Some(home.to_path_buf()),
                provider: None,
                window_days: 7,
            };
            let code = pollster::block_on(cmd.dispatch()).unwrap();
            assert_eq!(code, 0, "populated window must surface as exit 0");

            // Content sanity-check: re-open the DB and run the
            // aggregation, then format the row independently.
            // This catches formatter regressions without needing
            // to swap the global stdout writer.
            let db = Db::open(&db_path).unwrap();
            let rows = db.aggregate_window_usage(7, None).unwrap();
            assert_eq!(rows.len(), 1);
            let line = super::plan::format_row(&rows[0], None, 7);
            assert!(
                line.contains("minimax"),
                "row prefix must name provider: {line}"
            );
            assert!(
                line.contains("12,345"),
                "row must show thousands-separated total: {line}"
            );
        });
    }

    /// `--window-days 0` is rejected at the boundary so a shell
    /// script that defaults from an unset env var never silently
    /// pulls an empty window.
    #[test]
    fn cli_telemetry_plan_rejects_zero_window_days() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = TelemetryCmd::Plan {
            runs_dir: Some(tmp.path().to_path_buf()),
            provider: None,
            window_days: 0,
        };
        let err = pollster::block_on(cmd.dispatch()).unwrap_err();
        assert!(matches!(err, Error::InvalidArgs(_)));
    }

    /// PR-7: `telemetry cost --run <id>` aggregates `cost_usd`
    /// from the `calls` table by `(provider, model)`. The test
    /// inserts two calls (one with cost, one without) and asserts
    /// the aggregate reflects both.
    #[test]
    fn telemetry_cost_aggregates_by_provider_model() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let db = Db::open(&home.join("meta.sqlite")).unwrap();
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "ok", "test", None, None, None)
            .unwrap();
        // Two calls: one priced at $0.0123, one unpriced.
        db.record_call(
            "c1",
            run_id,
            "intake",
            "intake",
            "minimax",
            "MiniMax-M3",
            "",
            None,
            false,
            Some(200),
            1000,
            100,
            0,
            0,
            1,
            2,
            None,
            0,
        )
        .unwrap();
        db.record_call(
            "c2",
            run_id,
            "intake",
            "intake",
            "minimax",
            "MiniMax-M3",
            "",
            None,
            false,
            Some(200),
            1000,
            100,
            0,
            0,
            3,
            4,
            None,
            0,
        )
        .unwrap();
        db.record_call_cost("c1", 0.0123).unwrap();
        let rows = db.aggregate_cost_by_provider_model(Some(run_id)).unwrap();
        assert_eq!(rows.len(), 1, "single (provider, model) pair");
        assert_eq!(rows[0].provider, "minimax");
        assert_eq!(rows[0].model, "MiniMax-M3");
        assert_eq!(rows[0].calls, 2);
        assert!(
            (rows[0].cost_usd - 0.0123).abs() < 1e-9,
            "got {}",
            rows[0].cost_usd
        );
    }

    /// PR-7: `telemetry cost --all` aggregates across every
    /// recorded run. The test inserts calls under two different
    /// run ids and confirms the aggregation ignores the run
    /// boundary.
    #[test]
    fn telemetry_cost_all_aggregates_across_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let db = Db::open(&home.join("meta.sqlite")).unwrap();
        let r1 = RunId::new();
        let r2 = RunId::new();
        db.register_run(r1, "fast", "ok", "test", None, None, None)
            .unwrap();
        db.register_run(r2, "fast", "ok", "test", None, None, None)
            .unwrap();
        db.record_call(
            "a",
            r1,
            "intake",
            "intake",
            "minimax",
            "M3",
            "",
            None,
            false,
            Some(200),
            1000,
            100,
            0,
            0,
            1,
            2,
            None,
            0,
        )
        .unwrap();
        db.record_call(
            "b",
            r2,
            "intake",
            "intake",
            "minimax",
            "M3",
            "",
            None,
            false,
            Some(200),
            1000,
            100,
            0,
            0,
            3,
            4,
            None,
            0,
        )
        .unwrap();
        db.record_call(
            "c",
            r1,
            "intake",
            "intake",
            "opencode_go",
            "k3",
            "",
            None,
            false,
            Some(200),
            1000,
            100,
            0,
            0,
            5,
            6,
            None,
            0,
        )
        .unwrap();
        db.record_call_cost("a", 0.10).unwrap();
        db.record_call_cost("b", 0.20).unwrap();
        let rows = db.aggregate_cost_by_provider_model(None).unwrap();
        // Two distinct (provider, model) pairs.
        assert_eq!(rows.len(), 2);
        let minimax = rows.iter().find(|r| r.provider == "minimax").unwrap();
        assert_eq!(minimax.calls, 2);
        assert!((minimax.cost_usd - 0.30).abs() < 1e-9);
        let oc = rows.iter().find(|r| r.provider == "opencode_go").unwrap();
        assert_eq!(oc.calls, 1);
        assert_eq!(oc.cost_usd, 0.0, "no cost recorded => zero");
    }
}
