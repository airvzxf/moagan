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
///
/// Matching strategy: pair each sidecar `request`+`response` with one
/// internal `calls` record. We prefer to match by
/// `(body_sha256, started_unix)` when the internal call carries a
/// `body_canonical` string (the canonical form of the request body)
/// or a `body_sha256` field. If neither is present we fall back to a
/// `started_unix` ±2 s window — this still detects orphans and CRC
/// corruption, just without a per-call body match.
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
    // external pairs: (body_sha256, ts) -> response record
    let mut external_pairs: Vec<(String, i64, AuditRecord)> = Vec::new();
    // external ts list, kept separately for the timestamp fallback
    let mut external_ts: Vec<i64> = Vec::new();
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
            external_pairs.push((r.body_sha256.clone(), r.ts as i64, s.clone()));
            external_ts.push(r.ts as i64);
            let _ = id;
        }
    }

    let calls_text = if calls_jsonl_path.exists() {
        read_path(calls_jsonl_path)?
    } else {
        String::new()
    };
    // internal calls: keep them in a Vec so we can match by index when
    // timestamps collide. Each entry is the started_unix and the
    // optional body hash (if available).
    let mut internal_calls: Vec<(i64, Option<String>, serde_json::Value)> = Vec::new();
    for line in calls_text.lines() {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let started = v.get("started_unix").and_then(|x| x.as_i64()).unwrap_or(0);
        // Try a body hash in this order: explicit body_sha256,
        // explicit body_canonical string, hash of an empty string
        // (legacy calls records). The verify matches by the same
        // key the sidecar used.
        let body_hash = v
            .get("body_sha256")
            .and_then(|x| x.as_str())
            .map(String::from)
            .or_else(|| {
                v.get("body_canonical")
                    .and_then(|x| x.as_str())
                    .map(|s| super::format::sha256_hex(s.as_bytes()))
            });
        internal_calls.push((started, body_hash, v));
    }

    // For each external pair, find the best internal match: prefer a
    // body-hash match in the same timestamp window, then fall back to
    // a timestamp-only match. Each internal call can only be consumed
    // once (so we don't double-count when many calls happen in the
    // same second).
    let mut internal_used = vec![false; internal_calls.len()];
    let mut external_matched = vec![false; external_pairs.len()];
    let mut body_mismatch_count = 0usize;

    // Pass 1: body-hash match within ±2 s.
    for (ei, (e_sha, e_ts, _resp)) in external_pairs.iter().enumerate() {
        let mut best: Option<usize> = None;
        for (ii, (_i_ts, i_sha, _v)) in internal_calls.iter().enumerate() {
            if internal_used[ii] {
                continue;
            }
            if let Some(i_sha) = i_sha
                && i_sha == e_sha
                && (_i_ts - e_ts).abs() <= 2
            {
                best = Some(ii);
                break;
            }
        }
        if let Some(ii) = best {
            internal_used[ii] = true;
            external_matched[ei] = true;
            report.match_count += 1;
        }
    }

    // Pass 2: timestamp-only fallback for everything still unmatched.
    // This is intentionally lenient because moagan's internal
    // CallEvent v0.1 does not carry a body hash, so body-hash
    // matching is impossible without an upgrade. We only count a
    // match if both sides are unconsumed and timestamps overlap.
    for (ei, (_e_sha, e_ts, _resp)) in external_pairs.iter().enumerate() {
        if external_matched[ei] {
            continue;
        }
        let mut best: Option<usize> = None;
        for (ii, (i_ts, _i_sha, _v)) in internal_calls.iter().enumerate() {
            if internal_used[ii] {
                continue;
            }
            if (i_ts - e_ts).abs() <= 2 {
                best = Some(ii);
                break;
            }
        }
        if let Some(ii) = best {
            internal_used[ii] = true;
            external_matched[ei] = true;
            report.match_count += 1;
        }
    }

    // Pass 3: body_mismatch_count — for internal calls that have a
    // body hash that disagrees with an external pair, count the
    // disagreement (so the operator can spot a real divergence).
    for (e_sha, e_ts, _resp) in &external_pairs {
        for (i_ts, i_sha, _v) in &internal_calls {
            if let Some(i_sha) = i_sha
                && i_sha != e_sha
                && (i_ts - e_ts).abs() <= 2
            {
                body_mismatch_count += 1;
                break;
            }
        }
    }

    report.unmatched_external_count = external_matched.iter().filter(|m| !**m).count();
    report.unmatched_internal_count = internal_used.iter().filter(|u| !**u).count();
    report.body_mismatch_count = body_mismatch_count;

    // Touch external_ts so the binding isn't unused if the future
    // pass 2 stops using it.
    let _ = external_ts;

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
