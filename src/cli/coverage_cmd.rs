//! `moagan coverage <run_id>` — inspect the SanCov runtime
//! coverage data for one run. ADR-0002.

use std::path::PathBuf;

use clap::Subcommand;

use crate::coverage::{CoverageReport, ensure_instrumented, filter_by_tag, render_text, scan_run};
use crate::error::Result;
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;

/// `moagan coverage` subcommand tree.
#[derive(Debug, Subcommand)]
pub enum CoverageCmd {
    /// Print a coverage report for one run. Without `--format`
    /// defaults to the terminal-friendly `text` view; with
    /// `--format html` and `grcov` on PATH, writes a navigable
    /// HTML report under `<run_dir>/coverage.html`.
    Show {
        /// Run id (UUID v7).
        #[arg(value_name = "RUN_ID")]
        run_id: String,
        /// Filter the snapshot list to files whose name contains
        /// the given tag (case-insensitive). Useful for narrowing
        /// to a single phase or call id.
        #[arg(long)]
        since_tag: Option<String>,
        /// Output format. `text` is the terminal-friendly
        /// summary that always works; `html` shells out to
        /// `grcov` to build a navigable report and errors out
        /// cleanly if `grcov` is not on PATH.
        #[arg(long, value_enum, default_value_t = CoverageFormat::Text)]
        format: CoverageFormat,
        /// Path the HTML report is written to. Defaults to
        /// `<run_dir>/coverage.html`. Ignored when the format is
        /// `text`.
        #[arg(long)]
        html_out: Option<PathBuf>,
    },
}

/// Output format for `moagan coverage show`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CoverageFormat {
    /// Human-readable text view, written to stdout. Always works
    /// (no external tools required).
    Text,
    /// Navigable HTML report. Requires `grcov` on `PATH`; the
    /// command fails with a clean error otherwise.
    Html,
}

/// Dispatch the `moagan coverage` subcommand. Returns the
/// process exit code (0 for success).
pub fn dispatch(home: &MoaganHome, cmd: CoverageCmd) -> Result<i32> {
    match cmd {
        CoverageCmd::Show {
            run_id,
            since_tag,
            format,
            html_out,
        } => {
            let run_id: RunId = run_id
                .parse()
                .map_err(|e| crate::Error::InvalidArgs(format!("{e}")))?;
            show(
                home,
                run_id,
                since_tag.as_deref(),
                format,
                html_out.as_deref(),
            )
        }
    }
}

fn show(
    home: &MoaganHome,
    run_id: RunId,
    since_tag: Option<&str>,
    format: CoverageFormat,
    html_out: Option<&std::path::Path>,
) -> Result<i32> {
    let run_dir = home.run_dir(run_id);
    let mut report = scan_run(&run_dir)?;
    if let Some(tag) = since_tag {
        report = filter_by_tag(&report, tag);
    }
    // The text view always works (it just prints a "not
    // instrumented" hint when the report is empty), so it does
    // NOT call `ensure_instrumented`. The HTML view shells out
    // to `grcov`, so it requires real data; the helper produces a
    // clean error with a copy-pasteable hint.
    match format {
        CoverageFormat::Text => {
            print!("{}", render_text(&report));
            Ok(0)
        }
        CoverageFormat::Html => {
            ensure_instrumented(&report)?;
            render_html(&report, html_out)
        }
    }
}

/// Render the HTML report. Today this is a thin wrapper that
/// shells out to `grcov`; when `grcov` is not on PATH, the
/// command fails with a copy-pasteable error message that lists
/// the exact `grcov` invocation. The HTML render is the only
/// sub-command that depends on an external tool, so a missing
/// `grcov` does not block the text view.
fn render_html(report: &CoverageReport, html_out: Option<&std::path::Path>) -> Result<i32> {
    if !crate::coverage::grcov_available() {
        return Err(crate::Error::InvalidState(
            "grcov is not on PATH; install it with `cargo install grcov` \
             (or `pacman -S grcov` on Arch) to render the HTML report. \
             The text view works without any external tool."
                .to_owned(),
        ));
    }
    let out = html_out.map(PathBuf::from).unwrap_or_else(|| {
        report
            .coverage_dir
            .parent()
            .unwrap_or(&report.coverage_dir)
            .join("coverage.html")
    });
    let status = std::process::Command::new("grcov")
        .arg(report.coverage_dir.as_os_str())
        .arg("--source-dir")
        .arg(".") // The caller is expected to run from a checkout.
        .arg("--branch")
        .arg("--ignore-not-existing")
        .arg("--output-format")
        .arg("html")
        .arg("-o")
        .arg(&out)
        .status()
        .map_err(crate::Error::from)?;
    if !status.success() {
        return Err(crate::Error::InvalidState(format!(
            "grcov exited with status {status}; see stderr above for details"
        )));
    }
    println!("html report written to {}", out.display());
    Ok(0)
}
