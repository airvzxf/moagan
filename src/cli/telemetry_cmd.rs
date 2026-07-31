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
    /// `moagan telemetry cleanup [--dry-run]`.
    Cleanup {
        /// Optional override for `MOAGAN_HOME`.
        #[arg(long)]
        runs_dir: Option<std::path::PathBuf>,
        /// When true, print what would be deleted without touching
        /// the filesystem.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
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
            other => Err(Error::InvalidArgs(format!(
                "invalid export format '{other}' (expected 'tar.gz', 'tar', or 'zip')"
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

mod compare {
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

    fn print_side_by_side(a: &RunRow, agg_a: &RunAggregate, b: &RunRow, agg_b: &RunAggregate) {
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

    fn print_diff(label: &str, a: i64, b: i64) {
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
        for (name, provider) in &cfg.providers {
            let key = (provider.kind.clone(), provider.model.clone());
            let stats = by_key.get(&key).copied().unwrap_or((0, 0, 0));
            println!(
                "{:<14}  {:<24}  {:<14}  {:<10}  {:<10}  {}",
                name,
                trim(&provider.endpoint, 24),
                trim(&provider.model, 14),
                stats.1,
                stats.2,
                stats.0
            );
        }
    }

    fn plan_summary(name: &str, cfg: &Config, db: &Db) -> Result<()> {
        let provider: &ProviderConfig = cfg
            .providers
            .get(name)
            .ok_or_else(|| Error::InvalidArgs(format!("unknown provider plan '{name}'")))?;
        println!("Provider: {}", name);
        println!("Kind: {}", provider.kind);
        println!("Endpoint: {}", provider.endpoint);
        println!("Model: {}", provider.model);
        if let Some(max) = provider.max_tokens {
            println!("Max tokens: {max}");
        }
        if let Some(t) = provider.temperature {
            println!("Temperature: {t}");
        }
        if let Some(p) = provider.top_p {
            println!("Top-p: {p}");
        }
        if !provider.hard_incompatibilities.is_empty() {
            println!(
                "Hard incompatibilities: {}",
                provider.hard_incompatibilities.join(", ")
            );
        }
        println!();
        println!("Recent usage (last 20 runs):");
        let rows = db.recent_runs_for_provider(&provider.kind, 20)?;
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
                trim(&provider.endpoint, 24),
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
    use crate::telemetry::dashboard::{self, DashboardConfig};
    use std::net::{IpAddr, SocketAddr};
    use std::sync::Arc;

    pub(super) async fn run(cmd: &TelemetryCmd) -> Result<()> {
        let (runs_dir, port) = match cmd {
            TelemetryCmd::View { runs_dir, port } => (runs_dir.as_ref(), *port),
            _ => return Err(Error::InvalidState("view: wrong variant".into())),
        };
        let home = super::resolve_home(runs_dir.map(|p| p.as_path()))?;
        let bind = SocketAddr::new(IpAddr::V4("127.0.0.1".parse().unwrap()), port);
        let cfg = DashboardConfig {
            bind,
            home: Arc::new(home),
            db_path: None,
        };
        let handle = dashboard::start(cfg).await?;
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
        // Block until the user hits Ctrl-C. The dashboard task
        // checks its cancellation token on every accept loop
        // iteration; the runtime's signal handler tears the
        // process down cleanly.
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
        }
    }
}

mod cleanup {
    use super::{Error, Result, TelemetryCmd};
    use crate::ids::RunId;
    use crate::storage::sqlite::Db;
    use crate::telemetry::retention::{RetentionConfig, RetentionPolicy, apply};

    pub(super) fn run(cmd: &TelemetryCmd) -> Result<()> {
        let (runs_dir, dry_run) = match cmd {
            TelemetryCmd::Cleanup { runs_dir, dry_run } => (runs_dir.as_ref(), *dry_run),
            _ => return Err(Error::InvalidState("cleanup: wrong variant".into())),
        };
        let home = super::resolve_home(runs_dir.map(|p| p.as_path()))?;
        let runs_dir = home.runs_dir();
        let db = Db::open(&home.meta_db_path()).ok();
        let cfg = RetentionConfig {
            keep_runs_days: 30,
            keep_runs_count: 100,
            max_storage_bytes: 50 * 1024 * 1024 * 1024,
            policy: RetentionPolicy::Delete,
        };
        let db_lookup = |run_id: RunId| -> Option<i64> {
            db.as_ref()
                .and_then(|d| d.get_run(run_id).ok().flatten())
                .map(|r| r.updated_unix)
        };
        let report = apply(&runs_dir, &db_lookup, &cfg, dry_run)?;
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
        assert!("rar".parse::<ExportFormat>().is_err());
    }

    #[test]
    fn list_empty_index_prints_marker() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let cmd = TelemetryCmd::List {
            runs_dir: Some(tmp.path().to_path_buf()),
            limit: 5,
            run: None,
        };
        // Empty DB doesn't exist yet; the open call creates it. We
        // capture stdout via the dispatch returning Ok(0) so the
        // test only checks the no-error / no-panic contract.
        let code = pollster::block_on(cmd.dispatch());
        assert_eq!(code.unwrap(), 0);
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
}
