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

    pub(super) fn run(_cmd: &TelemetryCmd) -> Result<()> {
        not_yet!("list")
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
    fn stubs_return_not_implemented() {
        let cmd = TelemetryCmd::List {
            runs_dir: None,
            limit: 10,
            run: None,
        };
        let err = cmd.clone().dispatch().unwrap_err();
        assert!(matches!(err, Error::InvalidState(_)));
        let s = format!("{err}");
        assert!(s.contains("list"));
        assert!(s.contains("sub-fase I"));
    }

    #[test]
    fn all_eight_subcommands_stubbed() {
        // Each variant dispatches to its own module's stub; this test
        // pins the CLI surface so a refactor cannot silently drop a
        // subcommand before Phase I lands.
        let cases: Vec<TelemetryCmd> = vec![
            TelemetryCmd::List {
                runs_dir: None,
                limit: 1,
                run: None,
            },
            TelemetryCmd::Summary {
                runs_dir: None,
                run: "01900000-0000-0000-0000-000000000000".into(),
            },
            TelemetryCmd::Compare {
                runs_dir: None,
                run_a: "01900000-0000-0000-0000-000000000000".into(),
                run_b: "01900000-0000-0000-0000-000000000001".into(),
            },
            TelemetryCmd::Provider {
                runs_dir: None,
                plan: Some("minimax".into()),
                list: false,
            },
            TelemetryCmd::View {
                runs_dir: None,
                port: 4096,
            },
            TelemetryCmd::Export {
                runs_dir: None,
                run: "01900000-0000-0000-0000-000000000000".into(),
                level: ExportLevel::default(),
                format: ExportFormat::default(),
                out: None,
            },
            TelemetryCmd::Cleanup {
                runs_dir: None,
                dry_run: true,
            },
            TelemetryCmd::Verify {
                runs_dir: None,
                path: std::path::PathBuf::from("/tmp/foo"),
            },
        ];
        assert_eq!(cases.len(), 8);
        for cmd in cases {
            let err = cmd.dispatch().unwrap_err();
            assert!(matches!(err, Error::InvalidState(_)));
        }
    }
}
