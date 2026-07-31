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
    pub fn dispatch(self) -> Result<i32> {
        match self {
            Self::List { .. } => list::run(&self).map(|_| 0),
            Self::Summary { .. } => summary::run(&self).map(|_| 0),
            Self::Compare { .. } => compare::run(&self).map(|_| 0),
            Self::Provider { .. } => provider::run(&self).map(|_| 0),
            Self::View { .. } => view::run(&self).map(|_| 0),
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

macro_rules! not_yet {
    ($variant:literal) => {
        Err(Error::InvalidState(format!(
            "moagan telemetry {}: not yet implemented in v0.3 sub-fase I",
            $variant
        )))
    };
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

    pub(super) fn run(_cmd: &TelemetryCmd) -> Result<()> {
        not_yet!("summary")
    }
}

mod compare {
    use super::{Error, Result, TelemetryCmd};

    pub(super) fn run(_cmd: &TelemetryCmd) -> Result<()> {
        not_yet!("compare")
    }
}

mod provider {
    use super::{Error, Result, TelemetryCmd};

    pub(super) fn run(_cmd: &TelemetryCmd) -> Result<()> {
        not_yet!("provider")
    }
}

mod view {
    use super::{Error, Result, TelemetryCmd};

    pub(super) fn run(_cmd: &TelemetryCmd) -> Result<()> {
        not_yet!("view")
    }
}

mod export {
    use super::{Error, Result, TelemetryCmd};

    pub(super) fn run(_cmd: &TelemetryCmd) -> Result<()> {
        not_yet!("export")
    }
}

mod cleanup {
    use super::{Error, Result, TelemetryCmd};

    pub(super) fn run(_cmd: &TelemetryCmd) -> Result<()> {
        not_yet!("cleanup")
    }
}

mod verify {
    use super::{Error, Result, TelemetryCmd};

    pub(super) fn run(_cmd: &TelemetryCmd) -> Result<()> {
        not_yet!("verify")
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
        let code = cmd.dispatch().unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn list_unknown_run_id_returns_invalid_args() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = TelemetryCmd::List {
            runs_dir: Some(tmp.path().to_path_buf()),
            limit: 5,
            run: Some("not-a-uuid".into()),
        };
        let err = cmd.dispatch().unwrap_err();
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
        let err = cmd.dispatch().unwrap_err();
        assert!(matches!(err, Error::InvalidState(_)));
    }
}
