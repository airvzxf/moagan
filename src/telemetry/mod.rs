//! Telemetry layer. Two append-only JSONL streams (phases, calls), each
//! piped through a `RedactWriter` so secrets never land on disk.
//!
//! Compliance: T01-06 §27 + 10-integrada-v0 §D.17 (heartbeat stub),
//! §D.27 (telemetry redact on write).
//!
//! Phase I (v0.3) added the read-only consumer side: [`export`]
//! bundles a run into a portable archive with a SHA256SUMS
//! manifest (T01-06 §10.9), and the dashboard + view / verify /
//! cleanup subcommands land in subsequent sub-fase-I commits.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use parking_lot::Mutex;
use serde::Serialize;

use crate::coverage::CoverageRecorder;
use crate::error::Result;
use crate::fs_layout::RunDir;
use crate::ids::RunId;
use crate::redact::{RedactPolicy, RedactWriter, Surface};
use crate::storage::outbox_tx::{OutboxEvent, record_with};
use crate::storage::sqlite::Db;
use crate::time::{now_unix_millis, now_unix_secs};

pub mod cross_run_sweep;
pub mod csv_summary;
pub mod daily_rotation;
pub mod dashboard;
pub mod dashboard_static;
pub mod event;
pub mod export;
pub mod heartbeat;
pub mod lineage_graph;
pub mod redact;
pub mod retention;
pub mod saturation;
pub mod stdout_events;
pub mod verify;

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
    /// `true` when the event was emitted by a pipeline that was
    /// produced via [`crate::phases::Pipeline::resume`]; `false`
    /// for fresh pipeline runs. Defaults to `false` so legacy
    /// JSONL files written before v0.5 PR-24 deserialize cleanly.
    /// v0.5 PR-24 (V4 §6.11, T01-06 §10.2): lets `moagan continue
    /// --kind discovery` distinguish the resumed `discover_matrix`
    /// fan-out from the original one in
    /// `telemetry/phases.jsonl.gz`.
    #[serde(default)]
    pub resume: bool,
    /// ADR-0002: path to the most recent coverage `profraw`
    /// snapshot at the moment the phase event was emitted. `None`
    /// when the binary is not instrumented (default build, or the
    /// `coverage` feature off) or the runtime had not flushed any
    /// counters yet. `serde(default)` keeps legacy rows
    /// deserialisable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_snapshot: Option<String>,
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
    /// SHA-256 of the exact HTTP request body for non-cached calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_sha256: Option<String>,
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
    /// Derived outcome enum (`ok|error|timeout|cancelled|truncated`).
    /// Mirrors the SQLite `calls.status` column so JSONL and SQL stay
    /// in lock-step. Existing JSONL files written before this field
    /// was added deserialize with `status = None`; readers must treat
    /// that as `unknown` (see migration v003).
    pub status: Option<String>,
    /// Zero-indexed retry attempt for this call. `0` is the first
    /// attempt, `1` is the second, etc. Persisted by the canonical
    /// retry loop in `phases::phase::call_with_retry_parse` so
    /// `calls.jsonl.gz` rows expose the same `retry_count` the
    /// warnings stream carries on `attempt`. Defaults to `0` for
    /// pre-migration rows so legacy files deserialize cleanly.
    #[serde(default)]
    pub retry_count: u32,
    /// ADR-0002: path to the most recent coverage `profraw`
    /// snapshot at the moment the call event was emitted.
    /// `serde(default)` so legacy rows deserialize cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_snapshot: Option<String>,
}

/// One warning event. Streamed to `telemetry/warnings.jsonl` and
/// mirrored to the SQLite `warnings` table (when the index is
/// enabled). Surfaces model auto-corrections, retries, and
/// truncation events so post-execution review can detect new
/// failure patterns without scraping stderr.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct WarningEvent {
    /// Run id.
    pub run_id: String,
    /// Unix milliseconds (ms resolution, not seconds).
    pub at_unix_ms: i64,
    /// Warning code (e.g. `model.json_repair_applied`).
    pub code: String,
    /// Severity (`warn` or `info`).
    pub level: String,
    /// Phase name, if known.
    pub phase: Option<String>,
    /// LLM role, if known.
    pub role: Option<String>,
    /// Call id, if known.
    pub call_id: Option<String>,
    /// Attempt number (0-indexed), if known.
    pub attempt: Option<u32>,
    /// Human-readable message (one line, no payload).
    pub message: String,
    /// Structured details (JSON-encoded). Never contains the raw
    /// model output — only counts, repair kinds, byte deltas.
    pub details: serde_json::Value,
}

/// One row written to `telemetry/checkpoints.jsonl` per checkpoint
/// capture. Mirrors the `HumanCheckpoint` JSON sidecar verbatim so
/// the dashboard can tail the stream live.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct CheckpointEvent {
    /// Run id.
    pub run_id: String,
    /// Stable checkpoint id (`h_<uuid7>`).
    pub ckp_id: String,
    /// Kind, mirroring the SQLite enum:
    /// `intake | clarify | final | custom`.
    pub kind: String,
    /// Question shown verbatim to the user.
    pub question: String,
    /// Raw response captured from stdin (the
    /// `<skipped:non_interactive>` marker when `interactive=false`).
    pub response: String,
    /// True when the user accepted the default by hitting enter.
    pub accepted_default: bool,
    /// Unix seconds at capture time (mirrors
    /// `HumanCheckpoint.at_unix`).
    pub at_unix: i64,
}

/// Re-export of the saturation event payload. The JSONL record on
/// disk uses the same shape as [`crate::telemetry::saturation::SaturationEvent`];
/// the alias lives here so `Telemetry::record_saturation` accepts the
/// type without forcing callers to import the inner module.
pub use crate::telemetry::saturation::SaturationEvent;

/// Context for a warning. Carries the optional phase/role/call_id
/// so the warning can be correlated with the call record.
#[derive(Debug, Clone, Default)]
pub struct WarningContext {
    /// Phase name.
    pub phase: Option<String>,
    /// LLM role.
    pub role: Option<String>,
    /// Call id.
    pub call_id: Option<String>,
    /// Attempt number.
    pub attempt: Option<u32>,
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
    /// Path to `telemetry/warnings.jsonl`.
    warnings_path: PathBuf,
    /// Path to `telemetry/checkpoints.jsonl` (Phase D sub-fase #6).
    /// Plain JSONL because checkpoints are tiny and the dashboard
    /// tails them live (no gzip overhead).
    checkpoints_path: PathBuf,
    /// Path to `telemetry/saturation.jsonl` (catalog §D.23 + §D.27,
    /// v0.8 push-side). Plain JSONL for parity with `warnings`:
    /// events are tiny and the alerts consumer streams them live.
    saturation_path: PathBuf,
    phases: Mutex<Option<RedactWriter<Box<dyn Write + Send>>>>,
    calls: Mutex<Option<RedactWriter<Box<dyn Write + Send>>>>,
    warnings: Mutex<Option<RedactWriter<Box<dyn Write + Send>>>>,
    checkpoints: Mutex<Option<RedactWriter<Box<dyn Write + Send>>>>,
    saturation: Mutex<Option<RedactWriter<Box<dyn Write + Send>>>>,
    /// Optional SQLite index. When present, every `phase()` and
    /// `call()` mirrors the JSONL record into the corresponding
    /// table so `moagan inspect` returns live data.
    db: Option<Db>,
    /// ADR-0002: runtime coverage recorder. `Some(_)` when the
    /// binary was built with the `coverage` feature (operator
    /// opted in), `None` for the default build. The recorder is
    /// cheap to clone and read; the `phase()` and `call()` methods
    /// call `snapshot()` to capture the active `profraw` path and
    /// record it on the JSONL row.
    coverage: Option<CoverageRecorder>,
    /// Periodic flush support. Every record method bumps
    /// `flush_counter`; when `(counter + 1) % flush_every == 0`
    /// the recorder auto-flushes so a killed-before-flush process
    /// does not lose the most recent events to the gzip buffer.
    flush_counter: AtomicU64,
    /// Flush threshold. Read once from `MOAGAN_TELEMETRY_FLUSH_EVERY`
    /// at construction; `0` disables auto-flush.
    flush_every: u64,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("run_id", &self.run_id)
            .field("phases_path", &self.phases_path)
            .field("calls_path", &self.calls_path)
            .field("warnings_path", &self.warnings_path)
            .field("db_indexed", &self.db.is_some())
            .field("coverage_active", &self.coverage.is_some())
            .finish()
    }
}

impl Telemetry {
    /// Open the telemetry streams for a run. Creates the directory
    /// if it does not exist. When `db` is provided, every record is
    /// mirrored to SQLite as well as the JSONL files.
    ///
    /// ADR-0002: when the `coverage` Cargo feature is enabled, the
    /// call also wires a [`CoverageRecorder`] into the SanCov
    /// runtime so per-phase `profraw` snapshots are emitted into
    /// `<run_dir>/telemetry/coverage/`. The recorder is a graceful
    /// no-op when the binary is not instrumented, so the default
    /// build keeps the exact same behaviour it had before this
    /// method gained the extra line.
    pub fn open(
        run_id: RunId,
        run: &RunDir<'_>,
        policy: RedactPolicy,
        db: Option<Db>,
    ) -> Result<Self> {
        tracing::info!(%run_id, telemetry = %run.telemetry().display(), "Telemetry::open: enter");
        run.telemetry(); // ensures the path is computed
        std::fs::create_dir_all(run.telemetry())?;
        tracing::debug!("Telemetry::open: telemetry dir ensured");
        // Spec §1.5 declares `gz` as the default compression for the
        // two append-only streams (`phases.jsonl` and `calls.jsonl`).
        // AGENTS.md's smoke gate #2 then names the on-disk file
        // literally as `telemetry/calls.jsonl.gz`. Warnings stay
        // uncompressed because they are tiny and frequently tailed.
        let phases_path: PathBuf = run.telemetry().join("phases.jsonl.gz");
        let calls_path: PathBuf = run.telemetry().join("calls.jsonl.gz");
        let warnings_path: PathBuf = run.telemetry().join("warnings.jsonl");
        let checkpoints_path: PathBuf = run.telemetry().join("checkpoints.jsonl");
        let saturation_path: PathBuf = run.telemetry().join("saturation.jsonl");
        let phases_writer = crate::storage::compression::open_gz_append(&phases_path)?;
        let calls_writer = crate::storage::compression::open_gz_append(&calls_path)?;
        let warnings_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&warnings_path)?;
        let checkpoints_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&checkpoints_path)?;
        let saturation_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&saturation_path)?;
        // ADR-0002: wire the coverage recorder. The recorder
        // is always constructed via `enable` so the
        // `LLVM_PROFILE_FILE` env var is set; if the binary is
        // not instrumented, the env var is a no-op and the
        // runtime simply does not write any `profraw`. A
        // future change can gate this on a `MOAGAN_COVERAGE=1`
        // env var if operators want to opt out per-run.
        let coverage = CoverageRecorder::enable(run, run_id)?;
        // PR-2: start the rotation thread so the active `profraw`
        // cannot grow unbounded. The thread checks the file size
        // every `MOAGAN_COVERAGE_ROTATION_INTERVAL_SECS` (default
        // 60 s) and triggers a snapshot when the file exceeds
        // `MOAGAN_COVERAGE_PROFRAW_BYTES_MAX` (default 1 GiB).
        // Without this, a long-running discovery test (e.g. run8
        // 5h 40m on 2026-08-19) produced a 66 GB `active.profraw`
        // that filled `/home` to 96 %. The thread is a no-op when
        // the runtime was not actually wired (feature off, RUSTFLAGS
        // missing).
        coverage.start_rotation(
            std::env::var("MOAGAN_COVERAGE_PROFRAW_BYTES_MAX")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1_073_741_824),
            std::env::var("MOAGAN_COVERAGE_ROTATION_INTERVAL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(60),
        );
        tracing::info!(%run_id, "Telemetry::open: ok");
        Ok(Self {
            inner: Arc::new(Inner {
                run_id,
                phases_path,
                calls_path,
                warnings_path,
                checkpoints_path,
                saturation_path,
                phases: Mutex::new(Some(RedactWriter::new(
                    phases_writer,
                    policy.clone(),
                    Surface::Telemetry,
                ))),
                calls: Mutex::new(Some(RedactWriter::new(
                    calls_writer,
                    policy.clone(),
                    Surface::Telemetry,
                ))),
                warnings: Mutex::new(Some(RedactWriter::new(
                    Box::new(warnings_file),
                    policy.clone(),
                    Surface::Telemetry,
                ))),
                checkpoints: Mutex::new(Some(RedactWriter::new(
                    Box::new(checkpoints_file),
                    policy.clone(),
                    Surface::Telemetry,
                ))),
                saturation: Mutex::new(Some(RedactWriter::new(
                    Box::new(saturation_file),
                    policy,
                    Surface::Telemetry,
                ))),
                db,
                coverage: Some(coverage),
                flush_counter: AtomicU64::new(0),
                flush_every: std::env::var("MOAGAN_TELEMETRY_FLUSH_EVERY")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(50),
            }),
        })
    }

    /// Borrow the SQLite index. `None` when the run was opened in
    /// no-index mode (legacy tests, the dashboard's read-only path,
    /// or a run that explicitly skipped the index). Phases can use
    /// this to mirror sidecar content into SQLite; the mirror is
    /// best-effort and never blocks the phase.
    pub fn db(&self) -> Option<&Db> {
        self.inner.db.as_ref()
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
                warnings_path: PathBuf::from("/dev/null"),
                checkpoints_path: PathBuf::from("/dev/null"),
                saturation_path: PathBuf::from("/dev/null"),
                phases: Mutex::new(Some(RedactWriter::new(
                    Box::new(NullWriter),
                    policy.clone(),
                    Surface::Telemetry,
                ))),
                calls: Mutex::new(Some(RedactWriter::new(
                    Box::new(NullWriter),
                    policy.clone(),
                    Surface::Telemetry,
                ))),
                warnings: Mutex::new(Some(RedactWriter::new(
                    Box::new(NullWriter),
                    policy.clone(),
                    Surface::Telemetry,
                ))),
                checkpoints: Mutex::new(Some(RedactWriter::new(
                    Box::new(NullWriter),
                    policy.clone(),
                    Surface::Telemetry,
                ))),
                saturation: Mutex::new(Some(RedactWriter::new(
                    Box::new(NullWriter),
                    policy,
                    Surface::Telemetry,
                ))),
                db: None,
                // ADR-0002: the no-op telemetry stays a true
                // no-op for tests that do not care about
                // coverage. `noop()` does not set
                // `LLVM_PROFILE_FILE` and does not touch the
                // filesystem.
                coverage: Some(CoverageRecorder::noop()),
                flush_counter: AtomicU64::new(0),
                flush_every: 0, // tests do not auto-flush
            }),
        }
    }

    /// Borrow the runtime coverage recorder, if any. `None` only
    /// for an explicit `coverage = None` initialisation; the
    /// default `open` and `noop` both populate it.
    pub fn coverage(&self) -> Option<&CoverageRecorder> {
        self.inner.coverage.as_ref()
    }

    /// Path to the phases log.
    pub fn phases_path(&self) -> &Path {
        &self.inner.phases_path
    }

    /// Path to the checkpoints log (a JSONL of every checkpoint
    /// for the dashboard to tail without parsing the sidecar).
    pub fn checkpoints_path(&self) -> &Path {
        &self.inner.checkpoints_path
    }

    /// Record a human checkpoint. The JSON sidecar
    /// `checkpoints/h_<uuid>.json` is the canonical record (already
    /// written by `src/checkpoint/human.rs::persist`); this method
    /// is the SQLite mirror used by `moagan inspect` to query
    /// "which runs had rejected checkpoints?" without touching the
    /// filesystem.
    ///
    /// Best-effort: failures are logged as `tracing::warn!` and do
    /// not abort the run (the JSON sidecar remains the source of
    /// truth).
    pub fn record_checkpoint(&self, cp: &crate::domain::HumanCheckpoint) -> Result<()> {
        tracing::debug!(
            ckp_id = %cp.id,
            kind = %cp.kind,
            "record_checkpoint: enter"
        );
        let event = CheckpointEvent {
            run_id: self.inner.run_id.to_string(),
            ckp_id: cp.id.clone(),
            kind: cp.kind.clone(),
            question: cp.question.clone(),
            response: cp.response.clone(),
            accepted_default: cp.accepted_default,
            at_unix: cp.at_unix,
        };
        let bytes = serde_json::to_vec(&event).map_err(crate::Error::from)?;
        let mut g = self.inner.checkpoints.lock();
        if let Some(w) = g.as_mut() {
            w.write_all(&bytes)?;
            w.write_all(b"\n")?;
        }
        drop(g);
        if let Some(db) = &self.inner.db
            && let Err(e) = db.record_checkpoint(
                self.inner.run_id,
                &cp.id,
                &cp.kind,
                &cp.question,
                &cp.response,
                cp.accepted_default,
                cp.at_unix,
            )
        {
            tracing::warn!(
                ckp_id = %cp.id,
                kind = %cp.kind,
                error = %e,
                "SQLite checkpoint mirror failed"
            );
        }
        self.flush_if_due();
        tracing::trace!(ckp_id = %cp.id, "record_checkpoint: ok");
        Ok(())
    }

    /// Path to the calls log.
    pub fn calls_path(&self) -> &Path {
        &self.inner.calls_path
    }

    /// Path to the warnings log.
    pub fn warnings_path(&self) -> &Path {
        &self.inner.warnings_path
    }

    /// Path to the saturation events JSONL stream
    /// (`telemetry/saturation.jsonl`). Used by the alerts consumer
    /// to tail recent events without parsing the SQLite mirror.
    pub fn saturation_path(&self) -> &Path {
        &self.inner.saturation_path
    }

    /// Record a phase event. The `resume` flag is `true` when the
    /// event is emitted by a pipeline produced via
    /// [`crate::phases::Pipeline::resume`]; `false` for fresh
    /// pipeline runs. The flag flows into
    /// `telemetry/phases.jsonl.gz` and the SQLite `phases` mirror so
    /// post-execution review can distinguish resumed runs from
    /// fresh ones (v0.5 PR-24).
    pub fn phase(
        &self,
        phase: &str,
        seq: i64,
        status: &str,
        error: Option<&str>,
        resume: bool,
    ) -> Result<()> {
        tracing::trace!(phase, seq, status, resume, "Telemetry::phase: enter");
        // ADR-0002: capture the active `profraw` snapshot so the
        // post-mortem story can correlate the phase event with
        // the lines that were visited up to and including this
        // point. The snapshot is best-effort: a failure here is
        // logged at debug level and the event still gets
        // written with `coverage_snapshot = None`.
        let coverage_snapshot = self.inner.coverage.as_ref().and_then(|rec| {
            match rec.snapshot(&format!("phase-{seq}")) {
                Ok(snap) => Some(snap.path.to_string_lossy().into_owned()),
                Err(e) => {
                    tracing::debug!(phase, seq, error = %e, "coverage snapshot failed");
                    None
                }
            }
        });
        let ev = PhaseEvent {
            run_id: self.inner.run_id.to_string(),
            phase: phase.to_owned(),
            seq,
            status: status.to_owned(),
            at_unix: now_unix_secs(),
            error: error.map(str::to_owned),
            resume,
            coverage_snapshot,
        };
        let bytes = serde_json::to_vec(&ev).map_err(crate::Error::from)?;
        let mut g = self.inner.phases.lock();
        if let Some(w) = g.as_mut() {
            w.write_all(&bytes)?;
            w.write_all(b"\n")?;
        }
        drop(g);
        if let Some(db) = &self.inner.db
            && let Err(e) = db.record_phase(self.inner.run_id, phase, seq, status, error)
        {
            tracing::warn!(phase, seq, status, error = %e, "SQLite phase mirror failed");
        }
        self.flush_if_due();
        tracing::trace!(phase, seq, status, "Telemetry::phase: ok");
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
        body_sha256: Option<&str>,
        cache_hit: bool,
        http_status: Option<u16>,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        started_unix: i64,
        ended_unix: i64,
        error: Option<&str>,
        retry_count: u32,
    ) -> Result<()> {
        tracing::trace!(
            call_id,
            phase,
            role,
            provider,
            model,
            cache_hit,
            http_status = ?http_status,
            input_tokens,
            output_tokens,
            retry_count,
            "Telemetry::call: enter"
        );
        // ADR-0002: snapshot coverage at the call boundary so the
        // post-mortem story can correlate a failed LLM call with
        // the lines of code that ran up to and including the
        // failure. The snapshot is best-effort: a failure here is
        // logged at debug level and the event still gets written
        // with `coverage_snapshot = None`.
        let coverage_snapshot = self.inner.coverage.as_ref().and_then(|rec| {
            match rec.snapshot(&format!("call-{call_id}")) {
                Ok(snap) => Some(snap.path.to_string_lossy().into_owned()),
                Err(e) => {
                    tracing::debug!(call_id, phase, error = %e, "coverage snapshot failed");
                    None
                }
            }
        });
        let ev = CallEvent {
            run_id: self.inner.run_id.to_string(),
            call_id: call_id.to_owned(),
            phase: phase.to_owned(),
            role: role.to_owned(),
            provider: provider.to_owned(),
            model: model.to_owned(),
            cache_key: cache_key.to_owned(),
            body_sha256: body_sha256.map(str::to_owned),
            cache_hit,
            http_status,
            input_tokens,
            output_tokens,
            cache_read,
            cache_creation,
            started_unix,
            ended_unix,
            error: error.map(str::to_owned),
            status: Some(crate::storage::sqlite::call_status(http_status, error).to_string()),
            retry_count,
            coverage_snapshot,
        };
        let bytes = serde_json::to_vec(&ev).map_err(crate::Error::from)?;
        let mut g = self.inner.calls.lock();
        if let Some(w) = g.as_mut() {
            w.write_all(&bytes)?;
            w.write_all(b"\n")?;
        }
        drop(g);
        if let Some(db) = &self.inner.db {
            let record_call = || {
                db.record_call(
                    call_id,
                    self.inner.run_id,
                    phase,
                    role,
                    provider,
                    model,
                    cache_key,
                    body_sha256,
                    cache_hit,
                    http_status.map(i64::from),
                    input_tokens,
                    output_tokens,
                    cache_read,
                    cache_creation,
                    started_unix,
                    ended_unix,
                    error,
                    retry_count,
                )
            };
            let call_result = if cache_hit {
                record_call()
            } else {
                let events = [OutboxEvent {
                    run_id: self.inner.run_id,
                    event_type: "call.completed".into(),
                    payload: format!(
                        "{{\"call_id\":\"{call_id}\",\"phase\":\"{phase}\",\"role\":\"{role}\",\"input_tokens\":{input_tokens},\"output_tokens\":{output_tokens}}}"
                    ),
                }];
                record_with(db, &events, record_call)
            };
            if let Err(e) = call_result {
                tracing::warn!(call_id, phase, error = %e, "SQLite call/outbox mirror failed");
            }
        }
        // U1: emit outbox_events + provider_rollups for every real
        // call (cache hits are skipped so the rollup counts reflect
        // actual LLM traffic). Both writes are best-effort: a SQLite
        // failure is logged and never aborts the call. Schema is
        // v008_add_ons.sql.
        if let Some(db) = &self.inner.db
            && !cache_hit
            && let Err(e) = db.increment_provider_rollup(
                provider,
                model,
                input_tokens,
                output_tokens,
                error.is_some(),
            )
        {
            tracing::warn!(call_id, error = %e, "provider_rollups write failed");
        }
        self.flush_if_due();
        tracing::trace!(call_id, phase, "Telemetry::call: ok");
        Ok(())
    }

    /// Record a heartbeat. v0.1 writes a `phases.jsonl` event with
    /// phase=`heartbeat` so external scrapers can detect liveness
    /// without depending on a separate file. Full `last_heartbeat`
    /// column in SQLite and zombie detection land in v0.2.
    pub fn heartbeat(&self) -> Result<()> {
        // Heartbeats are not part of a pipeline so they always
        // emit `resume: false`. The flag would only matter inside a
        // resumed pipeline and the heartbeat fires per-tick from a
        // background task; it is never "the resumed phase".
        tracing::trace!("Telemetry::heartbeat: tick");
        self.phase("heartbeat", 0, "tick", None, false)
    }

    /// Record a warning event. The event is appended to
    /// `telemetry/warnings.jsonl` and mirrored to the SQLite
    /// `warnings` table (when the index is enabled). Used by the
    /// parser, the retry loop, and the provider to surface
    /// auto-corrections and recovery events that would otherwise be
    /// silently swallowed.
    ///
    /// `code` is the canonical warning key (e.g.
    /// `model.json_repair_applied`). `level` is `warn` or `info`.
    /// `ctx` carries optional phase/role/call_id/attempt so the
    /// warning can be correlated with the call record. `details`
    /// is the structured payload — never include the raw model
    /// output here, only counts and kinds.
    pub fn warn(
        &self,
        code: &str,
        level: &str,
        message: &str,
        details: serde_json::Value,
        ctx: WarningContext,
    ) -> Result<()> {
        // Clone the WarningContext fields we need to keep around
        // after the move into WarningEvent below. We need them for
        // the stdout Warning event mirror.
        let stdout_ctx_phase = ctx.phase.clone();
        let stdout_details = details.clone();
        tracing::trace!(
            code,
            level,
            phase = ?ctx.phase,
            call_id = ?ctx.call_id,
            "Telemetry::warn: enter"
        );
        let ev = WarningEvent {
            run_id: self.inner.run_id.to_string(),
            at_unix_ms: now_unix_millis(),
            code: code.to_owned(),
            level: level.to_owned(),
            phase: ctx.phase,
            role: ctx.role,
            call_id: ctx.call_id,
            attempt: ctx.attempt,
            message: message.to_owned(),
            details,
        };
        let bytes = serde_json::to_vec(&ev).map_err(crate::Error::from)?;
        let mut g = self.inner.warnings.lock();
        if let Some(w) = g.as_mut() {
            w.write_all(&bytes)?;
            w.write_all(b"\n")?;
        }
        drop(g);
        if let Some(db) = &self.inner.db {
            let details_str = ev.details.to_string();
            if let Err(e) = db.record_warning(
                self.inner.run_id,
                ev.at_unix_ms,
                &ev.code,
                &ev.level,
                ev.phase.as_deref(),
                ev.role.as_deref(),
                ev.call_id.as_deref(),
                ev.attempt.map(i64::from),
                &ev.message,
                &details_str,
            ) {
                tracing::warn!(code = ev.code, error = %e, "SQLite warning mirror failed");
            }
            // U1.3: when the message contains a known secret pattern,
            // mirror the categorised match into redact_audit so the
            // dashboard's "leaks per run" view can answer without
            // re-scanning the filesystem. We use the legacy `apply`
            // (not the categorised pass) because the message is a
            // single string; the policy already covered it.
            if let Some(kind) = detect_redact_kind(&ev.message)
                && let Err(e) = db.record_redact_audit(&crate::storage::sqlite::RedactAuditRow {
                    run_id: Some(self.inner.run_id.to_string()),
                    source_path: format!("telemetry/warnings.jsonl#{}", ev.code),
                    pattern_kind: kind.to_string(),
                    match_count: 1,
                    at_unix: ev.at_unix_ms / 1000,
                })
            {
                tracing::warn!(code = ev.code, error = %e, "redact_audit write failed");
            }
        }
        self.flush_if_due();
        tracing::trace!(code, "Telemetry::warn: ok");

        // Stdout Warning event mirror. Auto-silenced on TTY so the
        // operator's terminal stays clean.
        if stdout_events::resolve_event_format(stdout_events::EventFormat::Jsonl) {
            stdout_events::STDOUT_EVENTS.emit(stdout_events::Event::Warning {
                schema: stdout_events::SCHEMA_VERSION,
                ts: stdout_events::now_rfc3339(),
                code,
                level,
                phase: stdout_ctx_phase.as_deref(),
                details: stdout_details,
            });
        }
        Ok(())
    }

    /// Flush both streams. Idempotent.
    pub fn flush(&self) -> Result<()> {
        tracing::trace!("Telemetry::flush: enter");
        if let Some(w) = self.inner.phases.lock().as_mut() {
            w.flush()?;
        }
        if let Some(w) = self.inner.calls.lock().as_mut() {
            w.flush()?;
        }
        if let Some(w) = self.inner.warnings.lock().as_mut() {
            w.flush()?;
        }
        if let Some(w) = self.inner.saturation.lock().as_mut() {
            w.flush()?;
        }
        tracing::debug!(
            phases = self.inner.phases_path.display().to_string(),
            calls = self.inner.calls_path.display().to_string(),
            "telemetry: explicit flush"
        );
        Ok(())
    }

    /// Auto-flush helper. The gzip/RedactWriter pipeline is buffered,
    /// so a long-running pipeline that never calls `flush()` (e.g.
    /// killed before `main` returns) can lose the most recent phase
    /// events. The threshold is read once at construction from
    /// `MOAGAN_TELEMETRY_FLUSH_EVERY` (default 50). Every N-th call
    /// to a record method on this `Telemetry` invokes `flush()`.
    fn flush_if_due(&self) {
        let n = self
            .inner
            .flush_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let every = self.inner.flush_every;
        if every > 0 && (n + 1).is_multiple_of(every) {
            tracing::trace!(n = n + 1, every, "flush_if_due: triggering auto-flush");
            if let Err(e) = self.flush() {
                tracing::warn!(error = %e, "telemetry: auto-flush failed");
            }
        }
    }

    /// Record a saturation event fired by the runtime
    /// (catalog §D.23 + §D.27, v0.8 push-side). The event is
    /// appended to `telemetry/saturation.jsonl` and mirrored to the
    /// `saturation_events` SQLite table (when the index is enabled).
    ///
    /// Best-effort: a SQLite failure is logged as `tracing::warn!`
    /// and never aborts the call. The JSONL stream is the canonical
    /// timeline, so a SQLite miss does not lose data — operators
    /// can replay the JSONL into SQLite through the `moagan
    /// telemetry alerts list` consumer or a future repair tool.
    ///
    /// `event.run_id` is normally `Some(self.run_id)` for events
    /// fired inside the pipeline; pre-pipeline probes (e.g. a
    /// discovery call before the run is registered) leave it
    /// `None` so the SQLite mirror still accepts the row.
    pub fn saturation(&self, event: &SaturationEvent) -> Result<()> {
        tracing::debug!(
            provider = %event.provider,
            model = %event.model,
            kind = %event.kind,
            threshold_pct = event.threshold_pct,
            "Telemetry::saturation: enter"
        );
        let bytes = serde_json::to_vec(event).map_err(crate::Error::from)?;
        let mut g = self.inner.saturation.lock();
        if let Some(w) = g.as_mut() {
            w.write_all(&bytes)?;
            w.write_all(b"\n")?;
        }
        drop(g);
        if let Some(db) = &self.inner.db
            && let Err(e) = db.record_saturation(event)
        {
            tracing::warn!(
                provider = %event.provider,
                kind = %event.kind,
                error = %e,
                "SQLite saturation_events mirror failed"
            );
        }
        self.flush_if_due();
        tracing::trace!("Telemetry::saturation: ok");
        Ok(())
    }

    /// Convenience wrapper: build and record a `SaturationKind::Error`
    /// event from a circuit breaker opening. Used by the
    /// [`crate::llm::provider::BreakeredProvider`] hook.
    pub fn record_circuit_open(
        &self,
        provider: &str,
        model: &str,
        failure_count: u32,
    ) -> Result<()> {
        tracing::info!(
            provider,
            model,
            failure_count,
            "record_circuit_open: dispatching saturation event"
        );
        let event = SaturationEvent::from_circuit_breaker(
            provider,
            model,
            Some(self.inner.run_id.to_string()),
            failure_count,
        );
        self.saturation(&event)
    }

    /// Convenience wrapper: build and record a
    /// `SaturationKind::RateLimit` event from a token-bucket
    /// rejection. Used by the
    /// [`crate::llm::provider::BreakeredProvider`] hook.
    #[allow(clippy::too_many_arguments)]
    pub fn record_rate_limit(
        &self,
        provider: &str,
        model: &str,
        threshold_pct: f32,
        capacity: u32,
        refill_per_sec: u32,
    ) -> Result<()> {
        tracing::info!(
            provider,
            model,
            threshold_pct,
            capacity,
            refill_per_sec,
            "record_rate_limit: dispatching saturation event"
        );
        let event = SaturationEvent::from_rate_limit(
            provider,
            model,
            Some(self.inner.run_id.to_string()),
            threshold_pct,
            capacity,
            refill_per_sec,
        );
        self.saturation(&event)
    }
}

impl crate::llm::provider::SaturationSink for Telemetry {
    /// Push-side hook called by
    /// [`crate::llm::provider::BreakeredProvider::send`] when the
    /// wrapper rejects a call because the circuit breaker is open
    /// or the rate-limiter budget is exhausted. The event is built
    /// upstream with `run_id = None` (the wrapper is
    /// telemetry-agnostic); here we re-stamp it with the current
    /// run id so the SQLite mirror can join against the `runs`
    /// table.
    ///
    /// Best-effort: a failure here is logged and swallowed (the
    /// wrapper's caller was already on the error path; the
    /// rejection must not be hidden by a sink failure).
    fn on_saturation(&self, event: &SaturationEvent) {
        tracing::trace!(
            provider = %event.provider,
            kind = %event.kind,
            "on_saturation: stamping run_id and forwarding"
        );
        let mut stamped = event.clone();
        if stamped.run_id.is_none() {
            stamped.run_id = Some(self.inner.run_id.to_string());
        }
        if let Err(e) = self.saturation(&stamped) {
            tracing::warn!(
                provider = %stamped.provider,
                kind = %stamped.kind,
                error = %e,
                "saturation sink failed; event dropped"
            );
        }
    }
}

/// Best-effort pattern kind detector for a single string.
/// Walks the active pattern set and returns the first match's
/// `PatternKind` (or `None` if nothing matched). Used by the
/// `warn` path to populate `redact_audit` without re-running
/// the categorised pass.
#[allow(dead_code)]
fn detect_redact_kind(text: &str) -> Option<&'static str> {
    tracing::trace!(len = text.len(), "detect_redact_kind: enter");
    use crate::redact::apply::{RedactPolicy, Surface, apply};
    let policy = RedactPolicy::default();
    // The fastest possible detection: if the text wasn't redacted
    // at all under the default policy, it has no secret pattern.
    if apply(&policy, Surface::Telemetry, text)
        .ok()
        .map(|c| matches!(c, std::borrow::Cow::Borrowed(_)))
        .unwrap_or(true)
    {
        tracing::trace!("detect_redact_kind: no redaction -> None");
        return None;
    }
    // Match each pattern id and return the first mapped kind.
    for p in policy.active_patterns() {
        if p.re.is_match(text) {
            let kind = match p.id {
                "minimax_sk_cp" => "sk_cp_api_key",
                "anthropic_key" => "anthropic_api_key",
                "openai_key" => "openai_api_key",
                "gemini_key" => "gemini_api_key",
                "github_pat" | "github_oauth" | "github_app" => "github_pat",
                "huggingface_token" => "huggingface_token",
                "aws_access_key" | "aws_secret_key" => "aws_access_key",
                "bearer" => "bearer_header",
                "jwt" => "jwt",
                "ssh_private_key" | "pem_certificate" => "private_key",
                "connection_string" => "conn_string",
                "slack_token" => "slack_token",
                "credit_card" => "credit_card",
                "email" => "email",
                "private_ip" | "ip_v4" => "private_ip",
                "ssn_like" => "ssn_like",
                _ => continue,
            };
            tracing::trace!(pattern = p.id, kind, "detect_redact_kind: hit");
            return Some(kind);
        }
    }
    tracing::trace!("detect_redact_kind: miss");
    None
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
            resume: false,
            coverage_snapshot: None,
        };
        let j = serde_json::to_string(&ev).unwrap();
        let back: PhaseEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back.phase, "intake");
        assert!(!back.resume, "default resume flag must be false");
        assert!(
            back.coverage_snapshot.is_none(),
            "default coverage_snapshot must be None"
        );
    }

    /// ADR-0002: a `coverage_snapshot` field set to a relative
    /// path round-trips through serde without alteration.
    #[test]
    fn phase_event_round_trip_with_coverage_snapshot() {
        let ev = PhaseEvent {
            run_id: "abc".into(),
            phase: "intake".into(),
            seq: 2,
            status: "start".into(),
            at_unix: 1,
            error: None,
            resume: false,
            coverage_snapshot: Some("telemetry/coverage/run-abc-intake-start-0.profraw".into()),
        };
        let j = serde_json::to_string(&ev).unwrap();
        let back: PhaseEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(
            back.coverage_snapshot.as_deref(),
            Some("telemetry/coverage/run-abc-intake-start-0.profraw")
        );
    }

    /// v0.5 PR-24: legacy JSONL written before the `resume` field
    /// was added must still deserialize (with `resume = false`).
    /// The `serde(default)` on the field handles the missing-key
    /// case; this test pins the contract.
    #[test]
    fn phase_event_round_trip_legacy_jsonl_without_resume() {
        let legacy_json =
            r#"{"run_id":"abc","phase":"intake","seq":1,"status":"end","at_unix":1,"error":null}"#;
        let back: PhaseEvent = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(back.phase, "intake");
        assert!(
            !back.resume,
            "legacy JSONL without `resume` field must default to false"
        );
    }

    #[test]
    fn noop_telemetry_doesnt_panic() {
        let t = Telemetry::noop();
        t.phase("intake", 1, "end", None, false).unwrap();
        t.call(
            "c1",
            "intake",
            "intake",
            "mock",
            "m",
            "k",
            None,
            false,
            Some(200),
            0,
            0,
            0,
            0,
            1,
            2,
            None,
            0,
        )
        .unwrap();
        t.heartbeat().unwrap();
        t.flush().unwrap();
    }

    #[test]
    fn open_writes_to_run_dir() {
        // Use `with_moagan_home` to serialise the MOAGAN_HOME env
        // mutation with the rest of the test suite and to
        // auto-restore the previous value. The previous direct
        // `unsafe { std::env::set_var(...) }` calls (with no
        // cleanup) raced with sibling tests under default cargo
        // parallelism; that race surfaced as a flake in
        // `open_writes_to_run_dir` itself on the 10×-iteration
        // diagnostic loop (1/10 fail in CI-rerun analysis).
        crate::test_support::with_moagan_home("open_writes_to_run_dir", |_home| {
            let home = crate::fs_layout::MoaganHome::resolve().unwrap();
            let run_dir = home.run_dir(RunId::new());
            run_dir.ensure().unwrap();
            let t = Telemetry::open(RunId::new(), &run_dir, RedactPolicy::default(), None).unwrap();
            t.phase("intake", 1, "end", None, false).unwrap();
            t.flush().unwrap();
            let content = crate::storage::compression::read_to_string(t.phases_path()).unwrap();
            assert!(content.contains("intake"));
        });
    }

    /// U1: every real (non-cache-hit) LLM call must produce a row in
    /// `outbox_events` and increment the (provider, model) rollup in
    /// `provider_rollups`. Cache hits must NOT inflate the rollup.
    #[test]
    fn call_emits_outbox_event_and_provider_rollup() {
        crate::test_support::with_moagan_home(
            "call_emits_outbox_event_and_provider_rollup",
            |_home| {
                let home = crate::fs_layout::MoaganHome::resolve().unwrap();
                let run_id = RunId::new();
                let run_dir = home.run_dir(run_id);
                run_dir.ensure().unwrap();
                let db = Db::open(&home.meta_db_path()).unwrap();
                db.register_run(run_id, "fast", "running", "0.1.0", None, None, None)
                    .unwrap();
                let t =
                    Telemetry::open(run_id, &run_dir, RedactPolicy::default(), Some(db.clone()))
                        .unwrap();
                // Real call: should write outbox + rollup.
                t.call(
                    "call-1",
                    "intake",
                    "intake",
                    "minimax",
                    "MiniMax-M3",
                    "ck-1",
                    Some("hash1"),
                    false,
                    Some(200),
                    100,
                    50,
                    0,
                    0,
                    1,
                    2,
                    None,
                    0,
                )
                .unwrap();
                t.flush().unwrap();
                let ob_count = db
                    .list_outbox_events_for_run(&run_id.to_string())
                    .unwrap()
                    .len();
                assert_eq!(ob_count, 1, "expected one outbox_events row");
                // Check the provider rollup via a public read path. The
                // public surface for rollups is the `provider_usage_for_run`
                // view per-run; for the global rollup we use a small
                // public helper below.
                let rollup = db
                    .get_provider_rollup("minimax", "MiniMax-M3")
                    .unwrap()
                    .expect("rollup must exist");
                assert_eq!(rollup.calls, 1);
                assert_eq!(rollup.input_tokens, 100);
                assert_eq!(rollup.output_tokens, 50);

                // Cache hit: must NOT add another outbox row or rollup tick.
                t.call(
                    "call-2",
                    "intake",
                    "intake",
                    "minimax",
                    "MiniMax-M3",
                    "ck-2",
                    Some("hash2"),
                    true,
                    Some(200),
                    100,
                    50,
                    0,
                    0,
                    3,
                    4,
                    None,
                    0,
                )
                .unwrap();
                t.flush().unwrap();
                let ob_after_hit = db
                    .list_outbox_events_for_run(&run_id.to_string())
                    .unwrap()
                    .len();
                assert_eq!(
                    ob_after_hit, 1,
                    "cache hit must not produce a second outbox row"
                );
                let rollup2 = db
                    .get_provider_rollup("minimax", "MiniMax-M3")
                    .unwrap()
                    .expect("rollup must exist");
                assert_eq!(rollup2.calls, 1, "cache hit must not bump the rollup");
            },
        );
    }

    /// U1.3: a warning whose message contains a known secret pattern
    /// must land a row in `redact_audit` with the matching
    /// `pattern_kind`.
    #[test]
    fn warn_writes_redact_audit_row_when_message_contains_secret() {
        crate::test_support::with_moagan_home(
            "warn_writes_redact_audit_row_when_message_contains_secret",
            |_home| {
                let home = crate::fs_layout::MoaganHome::resolve().unwrap();
                let run_id = RunId::new();
                let run_dir = home.run_dir(run_id);
                run_dir.ensure().unwrap();
                let db = Db::open(&home.meta_db_path()).unwrap();
                db.register_run(run_id, "fast", "running", "0.1.0", None, None, None)
                    .unwrap();
                let t =
                    Telemetry::open(run_id, &run_dir, RedactPolicy::default(), Some(db.clone()))
                        .unwrap();
                t.warn(
                    "secret_in_payload",
                    "error",
                    "API key=sk-cp-aaaaaaaaaaaaaaaaaaaa leaked into the prompt",
                    serde_json::Value::Null,
                    WarningContext {
                        phase: Some("intake".into()),
                        role: Some("intake".into()),
                        call_id: Some("call-x".into()),
                        attempt: Some(1),
                    },
                )
                .unwrap();
                t.flush().unwrap();
                let count = db
                    .list_redact_audit_for_run(&run_id.to_string())
                    .unwrap()
                    .iter()
                    .filter(|r| r.pattern_kind == "sk_cp_api_key")
                    .count();
                assert_eq!(count, 1, "expected one redact_audit row for sk_cp_api_key");
            },
        );
    }

    #[test]
    fn redacts_secrets_in_phase() {
        crate::test_support::with_moagan_home("redacts_secrets_in_phase", |_home| {
            let home = crate::fs_layout::MoaganHome::resolve().unwrap();
            let run_dir = home.run_dir(RunId::new());
            run_dir.ensure().unwrap();
            let t = Telemetry::open(RunId::new(), &run_dir, RedactPolicy::default(), None).unwrap();
            t.phase(
                "intake",
                1,
                "error",
                Some("key=sk-cp-aaaaaaaaaaaaaaaaaaaa"),
                false,
            )
            .unwrap();
            t.flush().unwrap();
            let content = crate::storage::compression::read_to_string(t.phases_path()).unwrap();
            assert!(content.contains("[REDACTED:minimax_sk_cp]"));
            assert!(!content.contains("aaaaaaaaaaaaaaaaaaaa"));
        });
    }

    /// W1 fix: `RedactPolicy::allow_all()` (the runtime equivalent of
    /// `Config::redact_in_telemetry = false`) must keep the raw
    /// payload on disk. The `apply` helper short-circuits when
    /// `is_enabled(surface) == false`, so the RedactWriter becomes a
    /// pass-through; the bytes the operator wrote are the bytes that
    /// land on disk.
    #[test]
    fn redact_in_telemetry_false_keeps_raw_secrets_on_disk() {
        crate::test_support::with_moagan_home(
            "redact_in_telemetry_false_keeps_raw_secrets_on_disk",
            |_home| {
                let home = crate::fs_layout::MoaganHome::resolve().unwrap();
                let run_dir = home.run_dir(RunId::new());
                run_dir.ensure().unwrap();
                let t = Telemetry::open(RunId::new(), &run_dir, RedactPolicy::allow_all(), None)
                    .unwrap();
                let secret = "key=sk-cp-aaaaaaaaaaaaaaaaaaaa";
                t.phase("intake", 1, "error", Some(secret), false).unwrap();
                t.flush().unwrap();
                let content = crate::storage::compression::read_to_string(t.phases_path()).unwrap();
                assert!(
                    content.contains(secret),
                    "raw secret should remain visible when redact_in_telemetry=false"
                );
                assert!(
                    !content.contains("[REDACTED:"),
                    "no redaction marker should appear when the policy is disabled"
                );
            },
        );
    }

    #[test]
    fn warn_writes_jsonl_and_mirrors_to_sqlite() {
        crate::test_support::with_moagan_home("warn_writes_jsonl_and_mirrors_to_sqlite", |_home| {
            let home = crate::fs_layout::MoaganHome::resolve().unwrap();
            let run_id = RunId::new();
            let run_dir = home.run_dir(run_id);
            run_dir.ensure().unwrap();
            let db = Db::open(&home.meta_db_path()).unwrap();
            db.register_run(run_id, "fast", "running", "0.1.0", None, None, None)
                .unwrap();
            let t = Telemetry::open(run_id, &run_dir, RedactPolicy::default(), Some(db.clone()))
                .unwrap();
            let ctx = WarningContext {
                phase: Some("critique".into()),
                role: Some("critique".into()),
                call_id: Some("c1".into()),
                attempt: Some(0),
            };
            t.warn(
                "model.json_repair_applied",
                "warn",
                "colon repair",
                serde_json::json!({"repair_kind": "colon", "bytes_before": 42, "bytes_after": 43}),
                ctx,
            )
            .unwrap();
            t.flush().unwrap();

            let content = std::fs::read_to_string(t.warnings_path()).unwrap();
            assert!(content.contains("model.json_repair_applied"));
            assert!(content.contains("colon"));
            assert!(content.contains("\"phase\":\"critique\""));

            let summary = db.warnings_summary(run_id).unwrap();
            assert_eq!(summary.len(), 1);
            assert_eq!(summary[0].code, "model.json_repair_applied");
            assert_eq!(summary[0].count, 1);
            assert_eq!(summary[0].first_message, "colon repair");
        });
    }

    #[test]
    fn warn_redacts_secrets_in_message() {
        crate::test_support::with_moagan_home("warn_redacts_secrets_in_message", |_home| {
            let home = crate::fs_layout::MoaganHome::resolve().unwrap();
            let run_dir = home.run_dir(RunId::new());
            run_dir.ensure().unwrap();
            let t = Telemetry::open(RunId::new(), &run_dir, RedactPolicy::default(), None).unwrap();
            t.warn(
                "model.retry_provider",
                "warn",
                "got 401 with key=sk-cp-aaaaaaaaaaaaaaaaaaaa",
                serde_json::json!({}),
                WarningContext::default(),
            )
            .unwrap();
            t.flush().unwrap();
            let content = std::fs::read_to_string(t.warnings_path()).unwrap();
            assert!(content.contains("[REDACTED:minimax_sk_cp]"));
            assert!(!content.contains("aaaaaaaaaaaaaaaaaaaa"));
        });
    }
}

#[cfg(test)]
#[path = "event_tests.rs"]
mod event_tests;

#[cfg(test)]
#[path = "csv_summary_tests.rs"]
mod csv_summary_tests;

#[cfg(test)]
#[path = "dashboard_static_tests.rs"]
mod dashboard_static_tests;

#[cfg(test)]
#[path = "daily_rotation_tests.rs"]
mod daily_rotation_tests;
