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

use parking_lot::Mutex;
use serde::Serialize;

use crate::error::Result;
use crate::fs_layout::RunDir;
use crate::ids::RunId;
use crate::redact::{RedactPolicy, RedactWriter, Surface};
use crate::storage::outbox_tx::{OutboxEvent, record_with};
use crate::storage::sqlite::Db;
use crate::time::{now_unix_millis, now_unix_secs};

pub mod csv_summary;
pub mod daily_rotation;
pub mod dashboard;
pub mod dashboard_static;
pub mod event;
pub mod export;
pub mod heartbeat;
pub mod hub;
pub mod lineage_graph;
pub mod redact;
pub mod retention;
pub mod tracing_filter;
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
    phases: Mutex<Option<RedactWriter<Box<dyn Write + Send>>>>,
    calls: Mutex<Option<RedactWriter<Box<dyn Write + Send>>>>,
    warnings: Mutex<Option<RedactWriter<Box<dyn Write + Send>>>>,
    checkpoints: Mutex<Option<RedactWriter<Box<dyn Write + Send>>>>,
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
            .field("warnings_path", &self.warnings_path)
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
        // Spec §1.5 declares `gz` as the default compression for the
        // two append-only streams (`phases.jsonl` and `calls.jsonl`).
        // AGENTS.md's smoke gate #2 then names the on-disk file
        // literally as `telemetry/calls.jsonl.gz`. Warnings stay
        // uncompressed because they are tiny and frequently tailed.
        let phases_path: PathBuf = run.telemetry().join("phases.jsonl.gz");
        let calls_path: PathBuf = run.telemetry().join("calls.jsonl.gz");
        let warnings_path: PathBuf = run.telemetry().join("warnings.jsonl");
        let checkpoints_path: PathBuf = run.telemetry().join("checkpoints.jsonl");
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
        Ok(Self {
            inner: Arc::new(Inner {
                run_id,
                phases_path,
                calls_path,
                warnings_path,
                checkpoints_path,
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
                    policy,
                    Surface::Telemetry,
                ))),
                db,
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
        let ev = PhaseEvent {
            run_id: self.inner.run_id.to_string(),
            phase: phase.to_owned(),
            seq,
            status: status.to_owned(),
            at_unix: now_unix_secs(),
            error: error.map(str::to_owned),
            resume,
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
        Ok(())
    }

    /// Flush both streams. Idempotent.
    pub fn flush(&self) -> Result<()> {
        if let Some(w) = self.inner.phases.lock().as_mut() {
            w.flush()?;
        }
        if let Some(w) = self.inner.calls.lock().as_mut() {
            w.flush()?;
        }
        if let Some(w) = self.inner.warnings.lock().as_mut() {
            w.flush()?;
        }
        Ok(())
    }
}

/// Best-effort pattern kind detector for a single string.
/// Walks the active pattern set and returns the first match's
/// `PatternKind` (or `None` if nothing matched). Used by the
/// `warn` path to populate `redact_audit` without re-running
/// the categorised pass.
#[allow(dead_code)]
fn detect_redact_kind(text: &str) -> Option<&'static str> {
    use crate::redact::apply::{RedactPolicy, Surface, apply};
    let policy = RedactPolicy::default();
    // The fastest possible detection: if the text wasn't redacted
    // at all under the default policy, it has no secret pattern.
    if apply(&policy, Surface::Telemetry, text)
        .ok()
        .map(|c| matches!(c, std::borrow::Cow::Borrowed(_)))
        .unwrap_or(true)
    {
        return None;
    }
    // Match each pattern id and return the first mapped kind.
    for p in policy.active_patterns() {
        if p.re.is_match(text) {
            return Some(match p.id {
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
            });
        }
    }
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
        };
        let j = serde_json::to_string(&ev).unwrap();
        let back: PhaseEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back.phase, "intake");
        assert!(!back.resume, "default resume flag must be false");
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
#[path = "hub_tests.rs"]
mod hub_tests;

#[cfg(test)]
#[path = "csv_summary_tests.rs"]
mod csv_summary_tests;

#[cfg(test)]
#[path = "dashboard_static_tests.rs"]
mod dashboard_static_tests;

#[cfg(test)]
#[path = "tracing_filter_tests.rs"]
mod tracing_filter_tests;

#[cfg(test)]
#[path = "daily_rotation_tests.rs"]
mod daily_rotation_tests;
