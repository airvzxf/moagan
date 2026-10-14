//! D.17.7: CSV summary writer.

use std::path::Path;

use crate::error::Result;

/// One row of the CSV summary: `(model, thesis_count, total_tokens)`.
#[allow(missing_docs)]
pub type ThesisSummaryRow = (String, u64, u64);

/// Write the CSV summary next to the existing JSONL telemetry files.
/// Creates the `<run_dir>/telemetry/` directory if missing.
pub fn write_theses_summary(run_dir: &Path, rows: &[ThesisSummaryRow]) -> Result<()> {
    tracing::debug!(
        run_dir = %run_dir.display(),
        row_count = rows.len(),
        "write_theses_summary: enter"
    );
    let dir = run_dir.join("telemetry");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("theses_summary.csv");
    let mut out = String::from("model,thesis_count,total_tokens\n");
    for (model, count, tokens) in rows {
        out.push_str(&format!("{},{},{}\n", model, count, tokens));
    }
    std::fs::write(&path, out)?;
    tracing::trace!(path = %path.display(), "write_theses_summary: ok");
    Ok(())
}
