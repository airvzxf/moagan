//! `moagan inspect` — list recent runs and show their status, or
//! drill into a single run with its warning summary.

use std::path::PathBuf;

use tracing::{debug, trace, warn};

use crate::error::Result;
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::storage::sqlite::{Db, WarningRow, WarningSummaryRow};

/// Information about one run, as printed by `moagan inspect`.
#[derive(Debug, Clone)]
pub struct InspectEntry {
    /// Run id.
    pub run_id: RunId,
    /// Mode name.
    pub mode: String,
    /// Status string.
    pub status: String,
    /// Created unix seconds.
    pub created_unix: i64,
    /// Updated unix seconds.
    pub updated_unix: i64,
    /// Path to the run directory.
    pub path: PathBuf,
}

/// Warnings summary for a single run, as printed by
/// `moagan inspect <run_id>`.
#[derive(Debug, Clone)]
pub struct RunWarningsSummary {
    /// Run id.
    pub run_id: RunId,
    /// Aggregated counts per warning code.
    pub by_code: Vec<WarningSummaryRow>,
    /// Full ordered list of warnings (if any). Empty when the
    /// caller asks for the summary view only.
    pub all: Vec<WarningRow>,
}

/// List recent runs ordered by creation time, descending.
pub fn list_recent(db: &Db, limit: u32) -> Result<Vec<InspectEntry>> {
    trace!(limit, "inspect::list_recent: enter");
    let rows = db.list_runs(limit)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let run_id: RunId = r.run_id.parse().unwrap_or_default();
            InspectEntry {
                run_id,
                mode: r.mode,
                status: r.status,
                created_unix: r.created_unix,
                updated_unix: r.updated_unix,
                path: PathBuf::new(),
            }
        })
        .collect())
}

/// Look up the warning summary for a single run. Returns
/// `Ok(None)` when the run id is not in the index. The summary is
/// empty (zero rows) when the run finished without any
/// auto-correction or retry events.
pub fn summarize_run(db: &Db, run_id: RunId) -> Result<Option<RunWarningsSummary>> {
    debug!(run_id = %run_id, "inspect::summarize_run: enter");
    if db.get_run(run_id)?.is_none() {
        warn!(run_id = %run_id, "inspect: run not in index");
        return Ok(None);
    }
    let by_code = db.warnings_summary(run_id)?;
    let all = db.list_warnings(run_id)?;
    trace!(
        run_id = %run_id,
        by_code = by_code.len(),
        all = all.len(),
        "inspect::summarize_run: ok"
    );
    Ok(Some(RunWarningsSummary {
        run_id,
        by_code,
        all,
    }))
}

/// Render a one-line summary of the run's warnings to stdout.
/// Used by `moagan inspect <run_id>`. The `verbose` flag also
/// prints every individual warning event.
pub fn print_run_summary(summary: &RunWarningsSummary, verbose: bool) {
    println!(
        "run {}  {} warning event(s) across {} code(s)",
        summary.run_id.short(),
        summary.all.len(),
        summary.by_code.len(),
    );
    if summary.by_code.is_empty() {
        println!("  (no model auto-corrections or retries recorded)");
        return;
    }
    for row in &summary.by_code {
        println!(
            "  [{}] x{}  {}",
            row.code,
            row.count,
            truncate(&row.first_message, 80),
        );
    }
    if verbose {
        println!();
        println!("events:");
        for row in &summary.all {
            let phase = row.phase.as_deref().unwrap_or("-");
            println!(
                "  +{}ms  [{}]  phase={}  {}",
                row.at_unix_ms,
                row.code,
                phase,
                truncate(&row.message, 80),
            );
        }
    }
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

/// PR-7 `moagan inspect <run_id> --capabilities` view. Reads
/// the run's `manifest.json` (the canonical source for
/// `provider` and `model`) and prints a single capability row
/// cross-referenced with the `models.dev` catalog when the
/// on-disk cache is available.
///
/// `manifest.json#provider` and `#model` are the fields every
/// run writes today (T01-06 §33). A manifest without those two
/// fields is treated as "no info available" — the function
/// prints a warning and returns `Ok(())` so the operator can
/// still chain the command into shell scripts.
pub fn print_run_capabilities(home: &MoaganHome, run_id: RunId) -> Result<()> {
    debug!(run_id = %run_id, "inspect::print_run_capabilities: enter");
    let manifest = load_manifest(home, run_id)?;
    let catalog = crate::llm::models_dev::try_load_from_disk(home.root());
    if catalog.is_none() {
        warn!("inspect::capabilities: models_dev catalog missing");
        println!("[WARN] models_dev catalog cache is missing; cells marked `-` are best-effort");
    }
    let provider = manifest.provider.as_str();
    let model = manifest.model.as_str();
    if provider.is_empty() || model.is_empty() {
        println!(
            "run {}  no provider/model recorded on the manifest; \
             the run was likely started before the manifest gained a capability snapshot",
            run_id.short()
        );
        return Ok(());
    }
    let caps = capabilities_for_kind_or_default(provider, model);
    let entry = catalog
        .as_ref()
        .and_then(|c| crate::llm::models_dev::lookup(c, provider, model));
    println!("run {}", run_id.short());
    println!("  provider           : {provider}");
    println!("  model              : {model}");
    println!("  wire_format        : {}", caps.wire_format_id());
    println!(
        "  max_input_tokens   : {}",
        entry
            .as_ref()
            .map(|e| e.limit.context.to_string())
            .or_else(|| caps.max_input_tokens.map(|n| n.to_string()))
            .unwrap_or_else(|| "-".to_owned())
    );
    println!(
        "  max_output_tokens  : {}",
        entry
            .as_ref()
            .map(|e| e.limit.output.to_string())
            .unwrap_or_else(|| "-".to_owned())
    );
    println!("  supports_tools     : {}", yes_no(caps.supports_tools));
    println!("  supports_streaming : {}", yes_no(caps.supports_streaming));
    println!(
        "  attachment         : {}",
        entry.as_ref().map(|e| yes_no(e.attachment)).unwrap_or("-")
    );
    println!(
        "  reasoning          : {}",
        entry.as_ref().map(|e| yes_no(e.reasoning)).unwrap_or("-")
    );
    println!(
        "  temperature        : {}",
        entry.as_ref().map(|e| yes_no(e.temperature)).unwrap_or("-")
    );
    println!(
        "  cost($/M in/out)   : {}",
        entry
            .as_ref()
            .map(|e| format!("{:.2} / {:.2}", e.cost.input, e.cost.output))
            .unwrap_or_else(|| "-".to_owned())
    );
    Ok(())
}

fn load_manifest(home: &MoaganHome, run_id: RunId) -> Result<crate::domain::Manifest> {
    use crate::cli::continue_cmd::load_manifest as load_via_continue;
    load_via_continue(home, run_id)
}

fn capabilities_for_kind_or_default(
    _provider: &str,
    _model: &str,
) -> crate::llm::capabilities::ProviderCapabilities {
    // The manifest does not persist the per-provider `kind`, so
    // the doctor view here falls back to the OpenAI-compat
    // baseline. The `wire_format_id` printed below is therefore
    // a best-effort hint; the authoritative source is the
    // provider's runtime `capabilities()` call, which the
    // pipeline consulted when the run was originally
    // dispatched.
    crate::llm::capabilities::ProviderCapabilities::default()
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}
