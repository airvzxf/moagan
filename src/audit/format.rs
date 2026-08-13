//! External audit format for the `moagan audit proxy` sidecar.
//!
//! Each line on the JSONL stream is an [`AuditRecord`] with a per-line
//! CRC32 checksum that covers everything except the `crc32` field
//! itself. The on-disk file is a sequence of complete gzip members
//! (one per line) produced by [`AuditWriter`], so a process crash
//! between writes only truncates the trailing member; every prior
//! line is fully recoverable and CRC-verifiable.
//!
//! `body_canonical` is the only field that contains user/LLM data.
//! Headers are always redacted by name (see [`redact_header`]) and
//! `body_canonical` is included only when the operator does not
//! pass `--exclude-bodies` to `moagan audit proxy`. The hash
//! `body_sha256` is always recorded so `moagan audit verify` can
//! cross-check the sidecar against Moagan's internal `calls.jsonl.gz`
//! without needing the raw body on disk.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use flate2::write::GzEncoder;
use flate2::{Compression, Crc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
/// One JSONL record emitted by the sidecar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditRecord {
    /// Wall-clock timestamp in seconds (fractional).
    pub ts: f64,
    /// `request`, `response`, or `upstream_error`.
    pub event: String,
    /// Pairing key shared by the request and its response.
    pub id: String,
    /// HTTP method (request only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Upstream URL the request was forwarded to (request only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// HTTP status (response only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Sanitized headers (names lowercased, sensitive values redacted).
    /// `BTreeMap` keeps keys sorted so the CRC stays stable across
    /// hashmap re-orderings.
    pub headers: std::collections::BTreeMap<String, String>,
    /// Canonical UTF-8 body. Invalid byte sequences use replacement
    /// characters. `None` when `--exclude-bodies`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_canonical: Option<String>,
    /// SHA-256 of the raw body bytes. Always present.
    pub body_sha256: String,
    /// Raw body size in bytes.
    pub body_size: u64,
    /// End-to-end latency (response only), milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// CRC32 of every other field of this record (hex).
    pub crc32: String,
    /// Free-form error string for `upstream_error` events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Produce a canonical body string. JSON inputs are re-serialised
/// with keys ordered alphabetically. Non-JSON or invalid bytes fall
/// back to a UTF-8 lossy representation.
pub fn body_canonical(bytes: &[u8]) -> String {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(v) => serde_json::to_string(&canonify(v))
            .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned()),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn canonify(v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<String, Value> =
                map.into_iter().map(|(k, v)| (k, canonify(v))).collect();
            let mut out = serde_json::Map::new();
            for (k, v) in sorted {
                out.insert(k, v);
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(canonify).collect()),
        other => other,
    }
}

/// SHA-256 of a byte slice, hex-encoded.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Redact sensitive header values. Names compared case-insensitively.
pub fn redact_header(name: &str, value: &str) -> String {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "x-api-key" | "authorization" | "proxy-authorization" | "cookie" | "set-cookie" => {
            "***REDACTED***".to_owned()
        }
        _ => value.to_owned(),
    }
}

/// Compute the CRC32 of a record payload, hex-encoded. The CRC is
/// taken over the JSON serialisation of the record with `crc32`
/// excluded; the value is then injected back into the record so
/// readers can detect torn writes.
pub fn crc32_hex(payload: &[u8]) -> String {
    let mut crc = Crc::new();
    crc.update(payload);
    format!("{:08x}", crc.sum())
}

fn record_crc(rec: &AuditRecord) -> io::Result<String> {
    let mut value = serde_json::to_value(rec).map_err(io::Error::other)?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("crc32");
    }
    let payload = serde_json::to_vec(&canonify(value)).map_err(io::Error::other)?;
    Ok(crc32_hex(&payload))
}

/// Append-only writer for the sidecar JSONL. Each [`Self::write_record`]
/// serialises one line into a complete gzip member and flushes it to
/// the underlying file. A torn trailing member cannot invalidate any
/// previously completed record.
pub struct AuditWriter {
    inner: BufWriter<Box<dyn Write + Send>>,
    sync_file: Option<File>,
}

impl AuditWriter {
    /// Open a writer at `path`, creating the file if missing.
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = File::create(path)?;
        let sync_file = file.try_clone()?;
        Ok(Self {
            inner: BufWriter::new(Box::new(file)),
            sync_file: Some(sync_file),
        })
    }

    /// Open a writer at `path` in append mode, creating the file if
    /// missing. Used by the proxy when it swaps log files across runs.
    pub fn append(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let sync_file = file.try_clone()?;
        Ok(Self {
            inner: BufWriter::new(Box::new(file)),
            sync_file: Some(sync_file),
        })
    }

    /// Write one record as a complete gzip member.
    pub fn write_record(&mut self, rec: &mut AuditRecord) -> io::Result<()> {
        rec.crc32 = record_crc(rec)?;
        let line_value = canonify(serde_json::to_value(&*rec).map_err(io::Error::other)?);
        let mut line = serde_json::to_vec(&line_value).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&line)?;
        let member = encoder.finish()?;
        self.inner.write_all(&member)?;
        self.inner.flush()?;
        if let Some(file) = &self.sync_file {
            file.sync_data()?;
        }
        Ok(())
    }

    /// Flush the underlying writer.
    pub fn flush_gz(&mut self) -> io::Result<()> {
        self.inner.flush()?;
        if let Some(file) = &self.sync_file {
            file.sync_data()?;
        }
        Ok(())
    }
}

/// Verifier for the per-line CRC. Returns the number of lines that
/// failed the check, useful for `moagan audit verify`.
pub fn count_invalid_crcs(jsonl: &str) -> (usize, Vec<String>) {
    let mut invalid = 0usize;
    let mut bad = Vec::new();
    for line in jsonl.lines() {
        if line.is_empty() {
            continue;
        }
        let mut value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                invalid += 1;
                bad.push(line.to_owned());
                continue;
            }
        };
        let reported = match value.get("crc32").and_then(|v| v.as_str()) {
            Some(s) => s.to_owned(),
            None => {
                invalid += 1;
                bad.push(line.to_owned());
                continue;
            }
        };
        value.as_object_mut().map(|m| m.remove("crc32"));
        let canonical = canonify(value.clone());
        let payload = serde_json::to_vec(&canonical).unwrap_or_default();
        let expected = crc32_hex(&payload);
        if expected != reported {
            invalid += 1;
            bad.push(line.to_owned());
        }
    }
    (invalid, bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_canonical_round_trips_json() {
        let raw = br#"{"b":2,"a":1}"#;
        let canon = body_canonical(raw);
        assert_eq!(canon, r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn body_canonical_preserves_chinese_text() {
        let raw = "{\"msg\":\"中文 café\"}".as_bytes();
        let canon = body_canonical(raw);
        assert!(canon.contains("中文"), "lost CJK in {canon}");
        assert!(canon.contains("café"), "lost accented chars in {canon}");
    }

    #[test]
    fn body_canonical_falls_back_to_lossy_for_binary() {
        let raw = [0xff, 0xfe, 0x00, 0x01];
        let canon = body_canonical(&raw);
        assert!(!canon.is_empty());
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn redact_header_covers_secrets() {
        for n in [
            "x-api-key",
            "X-API-Key",
            "Authorization",
            "proxy-authorization",
            "Cookie",
            "Set-Cookie",
        ] {
            assert_eq!(redact_header(n, "secret"), "***REDACTED***", "{n}");
        }
        assert_eq!(
            redact_header("content-type", "application/json"),
            "application/json"
        );
        assert_eq!(redact_header("user-agent", "ua"), "ua");
    }

    #[test]
    fn crc32_hex_is_stable() {
        let h = crc32_hex(b"{}");
        assert_eq!(h.len(), 8);
        let h2 = crc32_hex(b"{}");
        assert_eq!(h, h2);
    }

    #[test]
    fn write_record_round_trips_with_valid_crc() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("audit.jsonl.gz");
        {
            let mut w = AuditWriter::create(&p).unwrap();
            let mut r = AuditRecord {
                ts: 1.0,
                event: "request".into(),
                id: "id-1".into(),
                method: Some("POST".into()),
                url: Some("http://upstream".into()),
                status: None,
                headers: std::collections::BTreeMap::new(),
                body_canonical: Some("{}".into()),
                body_sha256: sha256_hex(b"{}"),
                body_size: 2,
                elapsed_ms: None,
                crc32: String::new(),
                error: None,
            };
            w.write_record(&mut r).unwrap();
            w.flush_gz().unwrap();
        }
        let text = crate::storage::compression::read_to_string(&p).unwrap();
        let (invalid, bad) = count_invalid_crcs(&text);
        assert_eq!(invalid, 0, "bad lines: {bad:?}");
    }

    #[test]
    fn write_record_detects_torn_line() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("audit.jsonl.gz");
        {
            let mut w = AuditWriter::create(&p).unwrap();
            let mut r = AuditRecord {
                ts: 1.0,
                event: "request".into(),
                id: "id-1".into(),
                method: None,
                url: None,
                status: None,
                headers: std::collections::BTreeMap::new(),
                body_canonical: None,
                body_sha256: sha256_hex(b"hello"),
                body_size: 5,
                elapsed_ms: None,
                crc32: String::new(),
                error: None,
            };
            w.write_record(&mut r).unwrap();
            w.flush_gz().unwrap();
        }
        let mut text = crate::storage::compression::read_to_string(&p).unwrap();
        text.push_str("{\"crc32\":\"00000000\"}\n");
        let (invalid, _) = count_invalid_crcs(&text);
        assert_eq!(invalid, 1);
    }

    #[test]
    fn append_preserves_previous_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("audit.jsonl.gz");
        for i in 0..3 {
            let mut w = if i == 0 {
                AuditWriter::create(&p).unwrap()
            } else {
                AuditWriter::append(&p).unwrap()
            };
            let mut r = AuditRecord {
                ts: i as f64,
                event: "request".into(),
                id: format!("id-{i}"),
                method: Some("POST".into()),
                url: Some("http://upstream".into()),
                status: None,
                headers: Default::default(),
                body_canonical: None,
                body_sha256: sha256_hex(b"x"),
                body_size: 1,
                elapsed_ms: None,
                crc32: String::new(),
                error: None,
            };
            w.write_record(&mut r).unwrap();
            w.flush_gz().unwrap();
        }
        let text = crate::storage::compression::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 3);
        let (invalid, bad) = count_invalid_crcs(&text);
        assert_eq!(invalid, 0, "bad lines: {bad:?}");
    }
}
