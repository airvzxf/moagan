//! Telemetry layer. Two append-only JSONL streams (phases, calls), each
//! piped through a `RedactWriter` so secrets never land on disk.
//!
//! Compliance: T01-06 §27 + 10-integrada-v0 §D.13 (heartbeat stub),
//! §D.27 (telemetry redact on write).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;

use crate::error::Result;
use crate::fs_layout::RunDir;
use crate::ids::RunId;
use crate::redact::{RedactPolicy, RedactWriter, Surface};
use crate::storage::sqlite::Db;
use crate::time::now_unix_secs;

/// One phase event (start/end/error/cancel).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct PhaseEvent {
    /// Run id.
    pub run_id: String,
    /// Phase name.
    pub phase: String,
    /// Sequence within the run.
    pub seq: i64,
    /// Event kind.
    pub status: String,
    /// Unix seconds.
    pub at_unix: i64,
    /// Optional error message.
    pub error: Option<String>,
}

/// One LLM call record.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct CallEvent {
    /// Run id.
    pub run_id: String,
    /// Call id (UUID).
    pub call_id: String,
    /// Phase name.
    pub phase: String,
    /// Role name.
    pub role: String,
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
    /// Cache key (BLAKE3).
    pub cache_key: String,
    /// Whether the response came from cache.
    pub cache_hit: bool,
    /// HTTP status (None if no request was issued).
    pub http_status: Option<u16>,
    /// Input tokens billed.
    pub input_tokens: u64,
    /// Output tokens billed.
    pub output_tokens: u64,
    /// Tokens served from cache.
    pub cache_read: u64,
    /// Tokens written to cache.
    pub cache_creation: u64,
    /// Start unix seconds.
    pub started_unix: i64,
    /// End unix seconds.
    pub ended_unix: i64,
    /// Optional error message.
    pub error: Option<String>,
}

/// Telemetry handle. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Telemetry {
    inner: Arc<Inner>,
}

struct Inner {
    run_id: RunId,
    /// Path to `telemetry/phases.jsonl` (gzip not yet enabled; the
    /// file is plain JSONL in v0.1; v0.2 wraps in gzip).
    phases_path: PathBuf,
    /// Path to `telemetry/calls.jsonl`.
    calls_path: PathBuf,
    phases: Mutex<Option<RedactWriter<Box<dyn Write + Send>>>>,
    calls: Mutex<Option<RedactWriter<Box<dyn Write + Send>>>>,
    /// Optional SQLite index. When present, every `phase()` and
    /// `call()` mirrors the JSONL record into the corresponding
    /// table so `moagan inspect` returns live data.
    db: Option<Db>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("run_id", &self.run_id)
            .field("phases_path", &self.phases_path)
            .field("calls_path", &self.calls_path)
            .field("db_indexed", &self.db.is_some())
            .finish()
    }
}

impl Telemetry {
    /// Open the telemetry streams for a run. Creates the directory
    /// if it does not exist. When `db` is provided, every record is
    /// mirrored to SQLite as well as the JSONL files.
    pub fn open(
        run_id: RunId,
        run: &RunDir<'_>,
        policy: RedactPolicy,
        db: Option<Db>,
    ) -> Result<Self> {
        run.telemetry(); // ensures the path is computed
        std::fs::create_dir_all(run.telemetry())?;
        let phases_path: PathBuf = run.telemetry().join("phases.jsonl");
        let calls_path: PathBuf = run.telemetry().join("calls.jsonl");
        let phases_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&phases_path)?;
        let calls_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&calls_path)?;
        Ok(Self {
            inner: Arc::new(Inner {
                run_id,
                phases_path,
                calls_path,
                phases: Mutex::new(Some(RedactWriter::new(
                    Box::new(phases_file),
                    policy.clone(),
                    Surface::Telemetry,
                ))),
                calls: Mutex::new(Some(RedactWriter::new(
                    Box::new(calls_file),
                    policy,
                    Surface::Telemetry,
                ))),
                db,
            }),
        })
    }

    /// Build a no-op telemetry handle for tests.
    pub fn noop() -> Self {
        struct NullWriter;
        impl Write for NullWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let policy = RedactPolicy::default();
        Self {
            inner: Arc::new(Inner {
                run_id: RunId::default(),
                phases_path: PathBuf::from("/dev/null"),
                calls_path: PathBuf::from("/dev/null"),
                phases: Mutex::new(Some(RedactWriter::new(
                    Box::new(NullWriter),
                    policy.clone(),
                    Surface::Telemetry,
                ))),
                calls: Mutex::new(Some(RedactWriter::new(
                    Box::new(NullWriter),
                    policy,
                    Surface::Telemetry,
                ))),
                db: None,
            }),
        }
    }

    /// Path to the phases log.
    pub fn phases_path(&self) -> &Path {
        &self.inner.phases_path
    }

    /// Path to the calls log.
    pub fn calls_path(&self) -> &Path {
        &self.inner.calls_path
    }

    /// Record a phase event.
    pub fn phase(&self, phase: &str, seq: i64, status: &str, error: Option<&str>) -> Result<()> {
        let ev = PhaseEvent {
            run_id: self.inner.run_id.to_string(),
            phase: phase.to_owned(),
            seq,
            status: status.to_owned(),
            at_unix: now_unix_secs(),
            error: error.map(str::to_owned),
        };
        let bytes = serde_json::to_vec(&ev).map_err(crate::Error::from)?;
        let mut g = self.inner.phases.lock();
        if let Some(w) = g.as_mut() {
            w.write_all(&bytes)?;
            w.write_all(b"\n")?;
        }
        if let Some(db) = &self.inner.db {
            // Mirror into SQLite. Errors here are non-fatal: the
            // JSONL is the canonical timeline; the DB is a queryable
            // index.
            let _ = db.record_phase(self.inner.run_id, phase, seq, status, error);
        }
        Ok(())
    }

    /// Record an LLM call event.
    #[allow(clippy::too_many_arguments)]
    pub fn call(
        &self,
        call_id: &str,
        phase: &str,
        role: &str,
        provider: &str,
        model: &str,
        cache_key: &str,
        cache_hit: bool,
        http_status: Option<u16>,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        started_unix: i64,
        ended_unix: i64,
        error: Option<&str>,
    ) -> Result<()> {
        let ev = CallEvent {
            run_id: self.inner.run_id.to_string(),
            call_id: call_id.to_owned(),
            phase: phase.to_owned(),
            role: role.to_owned(),
            provider: provider.to_owned(),
            model: model.to_owned(),
            cache_key: cache_key.to_owned(),
            cache_hit,
            http_status,
            input_tokens,
            output_tokens,
            cache_read,
            cache_creation,
            started_unix,
            ended_unix,
            error: error.map(str::to_owned),
        };
        let bytes = serde_json::to_vec(&ev).map_err(crate::Error::from)?;
        let mut g = self.inner.calls.lock();
        if let Some(w) = g.as_mut() {
            w.write_all(&bytes)?;
            w.write_all(b"\n")?;
        }
        if let Some(db) = &self.inner.db {
            let _ = db.record_call(
                call_id,
                self.inner.run_id,
                phase,
                role,
                provider,
                model,
                cache_key,
                cache_hit,
                http_status.map(i64::from),
                input_tokens,
                output_tokens,
                cache_read,
                cache_creation,
                error,
            );
        }
        Ok(())
    }

    /// Record a heartbeat. v0.1 writes a `phases.jsonl` event with
    /// phase=`heartbeat` so external scrapers can detect liveness
    /// without depending on a separate file. Full `last_heartbeat`
    /// column in SQLite and zombie detection land in v0.2.
    pub fn heartbeat(&self) -> Result<()> {
        self.phase("heartbeat", 0, "tick", None)
    }

    /// Flush both streams. Idempotent.
    pub fn flush(&self) -> Result<()> {
        if let Some(w) = self.inner.phases.lock().as_mut() {
            w.flush()?;
        }
        if let Some(w) = self.inner.calls.lock().as_mut() {
            w.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_event_round_trip() {
        let ev = PhaseEvent {
            run_id: "abc".into(),
            phase: "intake".into(),
            seq: 1,
            status: "end".into(),
            at_unix: 1,
            error: None,
        };
        let j = serde_json::to_string(&ev).unwrap();
        let back: PhaseEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back.phase, "intake");
    }

    #[test]
    fn noop_telemetry_doesnt_panic() {
        let t = Telemetry::noop();
        t.phase("intake", 1, "end", None).unwrap();
        t.call(
            "c1",
            "intake",
            "intake",
            "mock",
            "m",
            "k",
            false,
            Some(200),
            0,
            0,
            0,
            0,
            1,
            2,
            None,
        )
        .unwrap();
        t.heartbeat().unwrap();
        t.flush().unwrap();
    }

    #[test]
    fn open_writes_to_run_dir() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let home = crate::fs_layout::MoaganHome::resolve().unwrap();
        let run_dir = home.run_dir(RunId::new());
        run_dir.ensure().unwrap();
        let t = Telemetry::open(RunId::new(), &run_dir, RedactPolicy::default(), None).unwrap();
        t.phase("intake", 1, "end", None).unwrap();
        t.flush().unwrap();
        let content = std::fs::read_to_string(t.phases_path()).unwrap();
        assert!(content.contains("intake"));
    }

    #[test]
    fn redacts_secrets_in_phase() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let home = crate::fs_layout::MoaganHome::resolve().unwrap();
        let run_dir = home.run_dir(RunId::new());
        run_dir.ensure().unwrap();
        let t = Telemetry::open(RunId::new(), &run_dir, RedactPolicy::default(), None).unwrap();
        t.phase("intake", 1, "error", Some("key=sk-cp-aaaaaaaaaaaaaaaaaaaa"))
            .unwrap();
        t.flush().unwrap();
        let content = std::fs::read_to_string(t.phases_path()).unwrap();
        assert!(content.contains("[REDACTED:minimax_sk_cp]"));
        assert!(!content.contains("aaaaaaaaaaaaaaaaaaaa"));
    }
}
