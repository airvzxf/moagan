//! Cross-check the sidecar stream against Moagan call telemetry.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::error::{Error, Result};
use crate::fs_layout::RunDir;
use crate::ids::RunId;
use crate::storage::compression;
use crate::storage::sqlite::Db;
use crate::telemetry::CallEvent;

use super::format::AuditRecord;

const MATCH_WINDOW_SECS: i64 = 1;

/// Verifier output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Calls that matched exactly.
    pub match_count: usize,
    /// Calls whose request hashes differ.
    pub body_mismatch_count: usize,
    /// Requests without a terminal event.
    pub orphan_request_count: usize,
    /// Terminal events without a request.
    pub orphan_response_count: usize,
    /// Internal calls without an external match.
    pub unmatched_internal_count: usize,
    /// External pairs without an internal match.
    pub unmatched_external_count: usize,
    /// Audit records with invalid CRC or syntax.
    pub crc_invalid_count: usize,
    /// External audit input was absent.
    pub audit_file_missing: bool,
    /// Internal calls input was absent.
    pub internal_file_missing: bool,
    /// Internal calls input could not be decoded or lacked fingerprints.
    pub internal_file_invalid: bool,
}

impl VerifyReport {
    /// Aggregate result.
    pub fn summary(&self) -> &'static str {
        if self.audit_file_missing
            || self.internal_file_missing
            || self.internal_file_invalid
            || self.crc_invalid_count > 0
        {
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

    /// Translate the result to the audit CLI contract.
    pub fn exit_code(&self) -> i32 {
        match self.summary() {
            "ok" => 0,
            "mismatch" => 1,
            _ => 2,
        }
    }
}

/// Read and decode the external audit stream.
pub fn read_records(path: &Path) -> Result<Vec<AuditRecord>> {
    if !path.exists() {
        return Err(Error::Io(crate::error::IoError::Raw(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("audit file {} not found", path.display()),
        ))));
    }
    let text = read_path(path)?;
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).map_err(Error::from))
        .collect()
}

fn read_path(path: &Path) -> Result<String> {
    if path.extension().and_then(|value| value.to_str()) == Some("gz") {
        compression::read_to_string(path)
    } else {
        std::fs::read_to_string(path).map_err(Error::from)
    }
}

struct ExternalCall {
    id: String,
    body_sha256: String,
    started_unix: i64,
}

struct ExternalPair {
    request: Option<AuditRecord>,
    terminal: Option<AuditRecord>,
    duplicate: bool,
}

impl ExternalPair {
    fn new() -> Self {
        Self {
            request: None,
            terminal: None,
            duplicate: false,
        }
    }
}

struct InternalCall {
    call_id: String,
    body_sha256: String,
    started_unix: i64,
}

/// Verify using the filesystem telemetry as the source of truth.
pub fn verify(run_dir: &RunDir<'_>, calls_jsonl_path: &Path) -> Result<VerifyReport> {
    verify_inner(run_dir, calls_jsonl_path, None)
}

/// Verify and also confirm that SQLite mirrors the filesystem calls.
pub fn verify_with_db(
    run_dir: &RunDir<'_>,
    calls_jsonl_path: &Path,
    db: &Db,
) -> Result<VerifyReport> {
    verify_inner(run_dir, calls_jsonl_path, Some(db))
}

fn verify_inner(
    run_dir: &RunDir<'_>,
    calls_jsonl_path: &Path,
    db: Option<&Db>,
) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();
    let audit_path = run_dir.external_audit_path();
    if !audit_path.exists() {
        report.audit_file_missing = true;
        return Ok(report);
    }
    if !calls_jsonl_path.exists() {
        report.internal_file_missing = true;
        return Ok(report);
    }

    let audit_text = match read_path(&audit_path) {
        Ok(text) => text,
        Err(_) => {
            report.crc_invalid_count = 1;
            return Ok(report);
        }
    };
    let (invalid_crc, _) = super::format::count_invalid_crcs(&audit_text);
    report.crc_invalid_count = invalid_crc;
    let records = parse_audit_records(&audit_text, &mut report);
    let external = pair_external_records(records, &mut report);

    let calls_text = match read_path(calls_jsonl_path) {
        Ok(text) => text,
        Err(_) => {
            report.internal_file_invalid = true;
            return Ok(report);
        }
    };
    let internal = parse_internal_calls(&calls_text, &mut report);
    if report.internal_file_invalid {
        return Ok(report);
    }

    let db_discrepancies = match db {
        Some(db) => compare_sqlite(run_dir, db, &internal)?,
        None => 0,
    };
    match_calls(external, internal, &mut report);
    report.unmatched_internal_count += db_discrepancies;
    Ok(report)
}

fn parse_audit_records(text: &str, _report: &mut VerifyReport) -> Vec<AuditRecord> {
    text.lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn pair_external_records(
    records: Vec<AuditRecord>,
    report: &mut VerifyReport,
) -> Vec<ExternalCall> {
    let mut pairs: HashMap<String, ExternalPair> = HashMap::new();
    for record in records {
        let pair = pairs
            .entry(record.id.clone())
            .or_insert_with(ExternalPair::new);
        match record.event.as_str() {
            "request" => {
                if pair.request.replace(record).is_some() {
                    pair.duplicate = true;
                }
            }
            "response" | "upstream_error" => {
                if pair.terminal.replace(record).is_some() {
                    pair.duplicate = true;
                }
            }
            _ => pair.duplicate = true,
        }
    }

    let mut external = Vec::new();
    for (id, pair) in pairs {
        if pair.duplicate {
            report.crc_invalid_count += 1;
            continue;
        }
        match (pair.request, pair.terminal) {
            (Some(request), Some(_)) if !request.body_sha256.is_empty() => {
                external.push(ExternalCall {
                    id,
                    body_sha256: request.body_sha256,
                    started_unix: request.ts.floor() as i64,
                });
            }
            (Some(_), None) => report.orphan_request_count += 1,
            (None, Some(_)) => report.orphan_response_count += 1,
            (Some(_), Some(_)) => report.crc_invalid_count += 1,
            (None, None) => {}
        }
    }
    external
        .sort_by(|left, right| (left.started_unix, &left.id).cmp(&(right.started_unix, &right.id)));
    external
}

fn parse_internal_calls(text: &str, report: &mut VerifyReport) -> Vec<InternalCall> {
    let mut internal = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let event: CallEvent = match serde_json::from_str(line) {
            Ok(event) => event,
            Err(_) => {
                report.internal_file_invalid = true;
                continue;
            }
        };
        if event.cache_hit {
            continue;
        }
        let body_sha256 = match event.body_sha256 {
            Some(body_sha256) if !body_sha256.is_empty() => body_sha256,
            None if event.provider != "minimax" => continue,
            _ => {
                report.internal_file_invalid = true;
                continue;
            }
        };
        internal.push(InternalCall {
            call_id: event.call_id,
            body_sha256,
            started_unix: event.started_unix,
        });
    }
    internal.sort_by(|left, right| {
        (left.started_unix, &left.call_id).cmp(&(right.started_unix, &right.call_id))
    });
    internal
}

fn compare_sqlite(run_dir: &RunDir<'_>, db: &Db, internal: &[InternalCall]) -> Result<usize> {
    let run_id = run_dir
        .root()
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| Error::InvalidState("run directory has no run id".into()))?
        .parse::<RunId>()
        .map_err(|e| Error::InvalidState(format!("invalid run directory id: {e}")))?;
    let rows = db.list_calls_for_run(run_id)?;
    let expected = internal
        .iter()
        .map(|call| (call.call_id.as_str(), call.body_sha256.as_str()))
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::new();
    let mut discrepancies = 0usize;
    for row in rows.into_iter().filter(|row| row.cache_hit == 0) {
        seen.insert(row.call_id.clone());
        match (
            expected.get(row.call_id.as_str()),
            row.body_sha256.as_deref(),
        ) {
            (Some(expected_hash), Some(actual_hash)) if *expected_hash == actual_hash => {}
            _ => discrepancies += 1,
        }
    }
    discrepancies += internal
        .iter()
        .filter(|call| !seen.contains(&call.call_id))
        .count();
    Ok(discrepancies)
}

fn match_calls(
    external: Vec<ExternalCall>,
    internal: Vec<InternalCall>,
    report: &mut VerifyReport,
) {
    let mut external_used = vec![false; external.len()];
    let mut internal_used = vec![false; internal.len()];

    for (external_index, external_call) in external.iter().enumerate() {
        if let Some(internal_index) = internal.iter().enumerate().find_map(|(index, call)| {
            (!internal_used[index]
                && call.body_sha256 == external_call.body_sha256
                && within_window(call.started_unix, external_call.started_unix))
            .then_some(index)
        }) {
            external_used[external_index] = true;
            internal_used[internal_index] = true;
            report.match_count += 1;
        }
    }

    for (external_index, external_call) in external.iter().enumerate() {
        if external_used[external_index] {
            continue;
        }
        if let Some(internal_index) = internal.iter().enumerate().find_map(|(index, call)| {
            (!internal_used[index]
                && call.body_sha256 != external_call.body_sha256
                && within_window(call.started_unix, external_call.started_unix))
            .then_some(index)
        }) {
            external_used[external_index] = true;
            internal_used[internal_index] = true;
            report.body_mismatch_count += 1;
        }
    }

    report.unmatched_external_count += external_used.iter().filter(|used| !**used).count();
    report.unmatched_internal_count += internal_used.iter().filter(|used| !**used).count();
}

fn within_window(left: i64, right: i64) -> bool {
    left.abs_diff(right) <= MATCH_WINDOW_SECS as u64
}

/// Write the verification summary as TSV.
pub fn write_tsv(report: &VerifyReport, dest: &Path) -> Result<()> {
    let body = render_tsv(report);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, body)?;
    Ok(())
}

/// Render the verification summary as TSV.
pub fn render_tsv(report: &VerifyReport) -> String {
    format!(
        "metric\tvalue\n\
         match_count\t{}\n\
         body_mismatch_count\t{}\n\
         orphan_request_count\t{}\n\
         orphan_response_count\t{}\n\
         unmatched_internal_count\t{}\n\
         unmatched_external_count\t{}\n\
         crc_invalid_count\t{}\n\
         audit_file_missing\t{}\n\
         internal_file_missing\t{}\n\
         internal_file_invalid\t{}\n\
         summary\t{}\n",
        report.match_count,
        report.body_mismatch_count,
        report.orphan_request_count,
        report.orphan_response_count,
        report.unmatched_internal_count,
        report.unmatched_external_count,
        report.crc_invalid_count,
        report.audit_file_missing,
        report.internal_file_missing,
        report.internal_file_invalid,
        report.summary()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_exit_codes_are_closed_over_the_contract() {
        let mut report = VerifyReport::default();
        assert_eq!(report.exit_code(), 0);
        report.body_mismatch_count = 1;
        assert_eq!(report.exit_code(), 1);
        report.body_mismatch_count = 0;
        report.crc_invalid_count = 1;
        assert_eq!(report.exit_code(), 2);
        report.crc_invalid_count = 0;
        report.internal_file_missing = true;
        assert_eq!(report.exit_code(), 2);
    }
}
