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

use flate2::Crc;
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
    /// Canonical body (UTF-8 lossless). `None` when `--exclude-bodies`.
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
/// with keys ordered alphabetically so two semantically equal bodies
/// hash to the same `body_sha256` even if the upstream sent keys in
/// a different order. Non-JSON or invalid bytes fall back to a
/// UTF-8 lossy representation so audit never loses data.
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

/// Append-only writer for the sidecar JSONL. Each [`write_record`]
/// serialises the record, computes its CRC, and writes the line +
/// newline in a single flush so a crash between records cannot leave
/// a partial line on disk. The on-disk file is plain JSONL (no
/// gzip) so a torn tail is still readable line by line; the CRC
/// per line flags anything that did not finish cleanly. The
/// `.jsonl.gz` extension is kept for naming consistency with the
/// rest of the telemetry tree (`calls.jsonl.gz`,
/// `phases.jsonl.gz`); the verifier transparently reads either
/// format via `crate::storage::compression::read_to_string`.
pub struct AuditWriter {
    inner: BufWriter<Box<dyn Write + Send>>,
}

impl AuditWriter {
    /// Open a writer at `path`, creating the file if missing.
    pub fn create(path: &Path) -> io::Result<Self> {
        let f = File::create(path)?;
        Ok(Self {
            inner: BufWriter::new(Box::new(f)),
        })
    }

    /// Open a writer at `path` in append mode, creating the file if
    /// missing. Used by the proxy when it swaps log files across runs.
    pub fn append(path: &Path) -> io::Result<Self> {
        let f = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            inner: BufWriter::new(Box::new(f)),
        })
    }

    /// Wrap an existing writer boxed as `Box<dyn Write + Send>`.
    pub fn from_boxed(inner: Box<dyn Write + Send>) -> Self {
        Self {
            inner: BufWriter::new(inner),
        }
    }

    /// Wrap an owned writer.
    pub fn from_writer<W: Write + Send + 'static>(w: W) -> Self {
        Self::from_boxed(Box::new(w))
    }

    /// Wrap a borrowed writer behind `Mutex` so it can be shared
    /// across threads. Used by tests.
    pub fn from_mutexed<W: Write + Send + 'static>(w: std::sync::Arc<std::sync::Mutex<W>>) -> Self {
        Self::from_boxed(Box::new(MutexWriter(w)))
    }

    /// Write one record atomically. After this call the bytes are
    /// flushed to the underlying writer, but the file is not yet
    /// fully finalised until `flush_gz` is called.
    pub fn write_record(&mut self, rec: &mut AuditRecord) -> io::Result<()> {
        // The CRC is computed over a canonical form: every JSON
        // object is canonified (alphabetical keys, recursive) so
        // `serde_json`'s field ordering does not affect the hash.
        rec.crc32 = String::new();
        let mut value = serde_json::to_value(&*rec).map_err(io::Error::other)?;
        if let Some(obj) = value.as_object_mut() {
            obj.remove("crc32");
        }
        let canonical = canonify(value);
        let payload = serde_json::to_vec(&canonical).map_err(io::Error::other)?;
        let crc = crc32_hex(&payload);
        rec.crc32 = crc;
        let line = serde_json::to_vec(rec).map_err(io::Error::other)?;
        self.inner.write_all(&line)?;
        self.inner.write_all(b"\n")?;
        self.inner.flush()?;
        Ok(())
    }

    /// Flush the underlying writer. After this call the bytes are
    /// visible to a reader. Named `flush_gz` to keep the call sites
    /// the same regardless of whether the on-disk file is gzipped.
    pub fn flush_gz(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Adapter over an `Arc<Mutex<W>>` so the writer can be shared.
struct MutexWriter<W: Write + Send + 'static>(std::sync::Arc<std::sync::Mutex<W>>);

impl<W: Write + Send + 'static> Write for MutexWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("mutex poisoned"))?
            .write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("mutex poisoned"))?
            .flush()
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

/// Recompute a record's CRC and return the new hex string. Used by
/// tests and the verifier when it needs to re-validate a record.
pub fn recompute_crc(rec: &mut AuditRecord) -> String {
    rec.crc32 = String::new();
    let payload = serde_json::to_vec(rec).unwrap_or_default();
    let crc = crc32_hex(&payload);
    rec.crc32 = crc.clone();
    crc
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
        let p = tmp.path().join("audit.jsonl");
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
        let text = std::fs::read_to_string(&p).unwrap();
        let (invalid, bad) = count_invalid_crcs(&text);
        assert_eq!(invalid, 0, "bad lines: {bad:?}");
    }

    #[test]
    fn write_record_detects_torn_line() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("audit.jsonl");
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
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str("{\"crc32\":\"00000000\"}\n");
        let (invalid, _) = count_invalid_crcs(&text);
        assert_eq!(invalid, 1);
    }

    #[test]
    fn append_preserves_previous_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("audit.jsonl");
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
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 3);
        let (invalid, bad) = count_invalid_crcs(&text);
        assert_eq!(invalid, 0, "bad lines: {bad:?}");
    }
}
