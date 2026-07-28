//! `moagan audit verify` — cross-check the sidecar JSONL against
//! Moagan's internal `calls.jsonl.gz` and SQLite.
//!
//! The verifier pairs each external `request`/`response` with the
//! internal `calls` record by `body_sha256` and `started_unix`
//! (tolerance ±2 s) and counts four outcomes. Exit codes follow the
//! contract documented in `docs/.../audit-design.md`:
//! - 0: perfect match.
//! - 1: mismatches or orphans.
//! - 2: file missing or CRC invalid.

use std::collections::HashMap;
use std::path::Path;

use crate::error::{Error, Result};
use crate::fs_layout::RunDir;
use crate::storage::compression;

use super::format::AuditRecord;

/// Verifier output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Calls that matched in both Moagan and the sidecar.
    pub match_count: usize,
    /// Calls where `body_sha256` differed between Moagan and the sidecar.
    pub body_mismatch_count: usize,
    /// Sidecar `request` events that have no matching `response`.
    pub orphan_request_count: usize,
    /// Sidecar `response` events that have no matching `request`.
    pub orphan_response_count: usize,
    /// Moagan `calls` rows that have no matching sidecar pair.
    pub unmatched_internal_count: usize,
    /// Sidecar pairs that have no matching Moagan `calls` row.
    pub unmatched_external_count: usize,
    /// Audit log lines whose CRC failed the integrity check.
    pub crc_invalid_count: usize,
    /// True when the external audit file was missing entirely.
    pub audit_file_missing: bool,
}

impl VerifyReport {
    /// Aggregate summary string used as the last line of the TSV.
    pub fn summary(&self) -> &'static str {
        if self.audit_file_missing || self.crc_invalid_count > 0 {
            "invalid"
        } else if self.body_mismatch_count == 0
            && self.orphan_request_count == 0
            && self.orphan_response_count == 0
            && self.unmatched_internal_count == 0
            && self.unmatched_external_count == 0
        {
            "ok"
        } else {
            "mismatch"
        }
    }

    /// Translate the report into a Unix exit code.
    pub fn exit_code(&self) -> i32 {
        if self.audit_file_missing || self.crc_invalid_count > 0 {
            2
        } else if self.body_mismatch_count > 0
            || self.orphan_request_count > 0
            || self.orphan_response_count > 0
            || self.unmatched_internal_count > 0
            || self.unmatched_external_count > 0
        {
            1
        } else {
            0
        }
    }
}

/// Read the audit log lines as `AuditRecord`s. Returns
/// `Err(AuditFileMissing)` if the path does not exist.
pub fn read_records(path: &Path) -> Result<Vec<AuditRecord>> {
    if !path.exists() {
        return Err(Error::Io(crate::error::IoError::Raw(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("audit file {} not found", path.display()),
        ))));
    }
    let text = read_path(path)?;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let rec: AuditRecord = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        out.push(rec);
    }
    Ok(out)
}

/// Read a text file, transparently decoding gzip when the path ends
/// in `.gz`. Used by the verifier and the unit tests so a plain
/// `.jsonl` works exactly like a `.jsonl.gz`.
fn read_path(path: &Path) -> Result<String> {
    if path.extension().and_then(|s| s.to_str()) == Some("gz") {
        compression::read_to_string(path)
    } else {
        std::fs::read_to_string(path).map_err(|e| Error::Io(crate::error::IoError::Raw(e)))
    }
}

/// Run the verifier over the sidecar log and the internal calls
/// (read from `calls.jsonl.gz` via [`compression::read_to_string`]).
pub fn verify(run_dir: &RunDir<'_>, calls_jsonl_path: &Path) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();
    let audit_path = run_dir.external_audit_path();
    if !audit_path.exists() {
        report.audit_file_missing = true;
        return Ok(report);
    }
    let text = read_path(&audit_path)?;
    let (invalid_count, _) = super::format::count_invalid_crcs(&text);
    report.crc_invalid_count = invalid_count;

    let records = read_records(&audit_path)?;
    let mut pairs: HashMap<String, (Option<AuditRecord>, Option<AuditRecord>)> = HashMap::new();
    for rec in records {
        let entry = pairs.entry(rec.id.clone()).or_insert((None, None));
        match rec.event.as_str() {
            "request" => entry.0 = Some(rec),
            "response" => entry.1 = Some(rec),
            "upstream_error" => {}
            _ => {}
        }
    }
    let mut external_keys: HashMap<(String, i64), AuditRecord> = HashMap::new();
    for (id, (req, resp)) in &pairs {
        if req.is_none() {
            report.orphan_response_count += 1;
        }
        if resp.is_none() {
            report.orphan_request_count += 1;
        }
        if let (Some(r), Some(s)) = (req, resp)
            && !r.body_sha256.is_empty()
        {
            let key = (r.body_sha256.clone(), r.ts as i64);
            external_keys.insert(key, s.clone());
            let _ = id;
        }
    }

    let calls_text = if calls_jsonl_path.exists() {
        read_path(calls_jsonl_path)?
    } else {
        String::new()
    };
    let mut internal_keys: HashMap<(String, i64), serde_json::Value> = HashMap::new();
    for line in calls_text.lines() {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let body = v
            .get("body_canonical")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let body_str = body.as_str().map(String::from).unwrap_or_default();
        let sha = super::format::sha256_hex(body_str.as_bytes());
        let started = v.get("started_unix").and_then(|x| x.as_i64()).unwrap_or(0);
        internal_keys.insert((sha, started), v);
    }

    let mut matched_external: std::collections::HashSet<(String, i64)> =
        std::collections::HashSet::new();
    let mut matched_internal: std::collections::HashSet<(String, i64)> =
        std::collections::HashSet::new();
    for key in external_keys.keys() {
        let window: Vec<i64> = (key.1 - 2..=key.1 + 2).collect();
        let hit = window.iter().find_map(|t| {
            internal_keys
                .get(&(key.0.clone(), *t))
                .map(|v| (key.0.clone(), *t, v.clone()))
        });
        if let Some((sha, t, _v)) = hit {
            report.match_count += 1;
            matched_external.insert(key.clone());
            matched_internal.insert((sha, t));
        }
    }
    let external_extras: Vec<_> = external_keys
        .keys()
        .filter(|k| !matched_external.contains(*k))
        .cloned()
        .collect();
    report.unmatched_external_count = external_extras.len();
    let internal_extras: Vec<_> = internal_keys
        .keys()
        .filter(|k| !matched_internal.contains(*k))
        .cloned()
        .collect();
    report.unmatched_internal_count = internal_extras.len();

    let mut body_mismatch = 0usize;
    for (sha, _t) in &internal_extras {
        if external_keys.keys().any(|(es, _t2)| es == sha) {
            body_mismatch += 1;
        }
    }
    report.body_mismatch_count = body_mismatch;

    Ok(report)
}

/// Render the report as a TSV block. Includes a header line.
pub fn write_tsv(report: &VerifyReport, dest: &Path) -> Result<()> {
    let body = format!(
        "metric\tvalue\n\
         match_count\t{}\n\
         body_mismatch_count\t{}\n\
         orphan_request_count\t{}\n\
         orphan_response_count\t{}\n\
         unmatched_internal_count\t{}\n\
         unmatched_external_count\t{}\n\
         crc_invalid_count\t{}\n\
         summary\t{}\n",
        report.match_count,
        report.body_mismatch_count,
        report.orphan_request_count,
        report.orphan_response_count,
        report.unmatched_internal_count,
        report.unmatched_external_count,
        report.crc_invalid_count,
        report.summary()
    );
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Io(crate::error::IoError::Raw(e)))?;
    }
    std::fs::write(dest, body).map_err(|e| Error::Io(crate::error::IoError::Raw(e)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::RunId;
    use tempfile::tempdir;

    #[test]
    fn report_summary_and_exit_code() {
        let mut r = VerifyReport::default();
        assert_eq!(r.summary(), "ok");
        assert_eq!(r.exit_code(), 0);
        r.body_mismatch_count = 1;
        assert_eq!(r.summary(), "mismatch");
        assert_eq!(r.exit_code(), 1);
        r.body_mismatch_count = 0;
        r.crc_invalid_count = 1;
        assert_eq!(r.summary(), "invalid");
        assert_eq!(r.exit_code(), 2);
    }

    #[test]
    fn verify_reports_missing_audit_file() {
        let tmp = tempdir().unwrap();
        let home = crate::fs_layout::MoaganHome::at(tmp.path().to_path_buf());
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().unwrap();
        let calls = run_dir.telemetry().join("calls.jsonl.gz");
        let report = verify(&run_dir, &calls).unwrap();
        assert!(report.audit_file_missing);
        assert_eq!(report.exit_code(), 2);
    }
}
