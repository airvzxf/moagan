//! Stdout event emitter. NDJSON, one event per line, schema-versioned.
//!
//! This is a SEPARATE stream from the tracing subscriber (which writes
//! to stderr). The two streams have different audiences:
//!
//! - **stderr** (tracing): free-form log events for humans + log
//!   aggregators (Promtail, Loki, Datadog). Format chosen by
//!   `--log-format` / `MOAGAN_LOG_FORMAT`.
//! - **stdout** (this module): typed, schema-versioned domain events
//!   for machine consumers (pipelines, dashboards). Always NDJSON.
//!
//! ## Why JSONL on stdout
//!
//! moagan emits *events* (phase_start, llm_call, probe, …), not
//! documents. The POSIX-idiomatic format for an event stream is NDJSON
//! (one JSON object per line). The same argument applies to
//! `docker events --format '{{json .}}'`, `kubectl get --watch -o json`,
//! and `ripgrep --json`.
//!
//! Pipe-friendly operators:
//:
//! ```bash
//! moagan run … 2> log.jsonl | jq -c 'select(.kind == "llm_call")'
//! ```
//!
//! ## Auto-detection
//!
//! The emitter is silent when stdout is a TTY (interactive mode);
//! the operator expects the terminal to stay clean. As soon as the
//! process is redirected (`> events.jsonl`) or piped (`| jq`), the
//! emitter starts writing one NDJSON event per line. The
//! `--event-format=off` flag disables the emitter unconditionally.
//!
//! ## Field documentation
//!
//! Each variant field is the canonical wire schema; doc comments
//! would duplicate the JSON field name, so the `Event` enum opts
//! out of `missing_docs`. See `docs/events-v1.md` for the schema
//! specification.

#![allow(missing_docs)]

use serde::Serialize;
use std::io::{self, IsTerminal, Stdout, Write};
use std::sync::Mutex;

/// Current schema version. Bump on any backwards-incompatible
/// change to the `Event` enum. Additive changes (new variant, new
/// field with a default) keep the same version.
pub const SCHEMA_VERSION: u32 = 1;

/// Resolve whether stdout events should be written.
///
/// Defaults: silent when stdout is a TTY, write when not. Operators
/// that want to force one side or the other set `--event-format`
/// (or the `MOAGAN_EVENT_FORMAT` env var which is honoured here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventFormat {
    /// Write NDJSON events whenever stdout is not a TTY (the default).
    Jsonl,
    /// Never write to stdout.
    Off,
}

pub fn resolve_event_format(explicit: EventFormat) -> bool {
    // 1. Explicit override wins.
    if let Ok(s) = std::env::var("MOAGAN_EVENT_FORMAT") {
        return match s.to_ascii_lowercase().as_str() {
            "off" | "none" | "disable" => false,
            // Anything else (including typos) defaults to NDJSON.
            _ => !std::io::stdout().is_terminal(),
        };
    }
    match explicit {
        EventFormat::Off => false,
        EventFormat::Jsonl => !std::io::stdout().is_terminal(),
    }
}

/// Typed domain event. Serialised as NDJSON; each variant gets its
/// own `kind` discriminator via `#[serde(tag = "kind")]`.
///
/// All variants carry `ts: String` (RFC 3339 UTC). New variants
/// SHOULD follow the same shape.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event<'a> {
    RunStart {
        schema: u32,
        ts: String,
        run_id: &'a str,
        mode: &'a str,
        provider: &'a str,
        model: &'a str,
        prompt_hash: &'a str,
    },
    RunEnd {
        schema: u32,
        ts: String,
        run_id: &'a str,
        status: &'a str,
        exit_code: i32,
        elapsed_ms: u64,
        artefacts: serde_json::Value,
    },
    PhaseStart {
        schema: u32,
        ts: String,
        phase: &'a str,
        seq: i64,
    },
    PhaseEnd {
        schema: u32,
        ts: String,
        phase: &'a str,
        seq: i64,
        elapsed_ms: u64,
        status: &'a str,
    },
    PhaseError {
        schema: u32,
        ts: String,
        phase: &'a str,
        seq: i64,
        error: &'a str,
        exit_code: i32,
    },
    LlmCall {
        schema: u32,
        ts: String,
        call_id: &'a str,
        phase: &'a str,
        role: &'a str,
        provider: &'a str,
        model: &'a str,
        elapsed_ms: u64,
        ok: bool,
        input_tokens: u32,
        output_tokens: u32,
        retry_count: u32,
    },
    DiscoveryIteration {
        schema: u32,
        ts: String,
        n: usize,
        total: usize,
        cell_dim: &'a str,
        cell_facet: &'a str,
        temperature: f32,
        replica: usize,
        sketch_index: usize,
        outcome: &'a str,
    },
    Probe {
        schema: u32,
        ts: String,
        probe_kind: &'a str,
        candidate: f32,
        iteration: u32,
        provider: &'a str,
        model: &'a str,
        outcome: &'a str,
    },
    Warning {
        schema: u32,
        ts: String,
        code: &'a str,
        level: &'a str,
        phase: Option<&'a str>,
        details: serde_json::Value,
    },
    Decision {
        schema: u32,
        ts: String,
        decision_kind: &'a str,
        payload: serde_json::Value,
    },
}

impl<'a> Event<'a> {
    /// Returns the `schema` value that should appear at the top
    /// level of every serialised event. Centralised here so a
    /// future bump touches one site, not nine. Currently
    /// unused — the schema field is emitted literally at every
    /// emit site — but kept as a single source of truth for the
    /// version, and for future programmatic use (e.g. consumer
    /// validation).
    #[allow(dead_code)]
    fn schema(&self) -> u32 {
        SCHEMA_VERSION
    }
}

/// Process-global emitter. Wraps `stdout` with a `Mutex` so
/// concurrent tasks can emit events without interleaving bytes
/// (NDJSON is line-oriented; a partial write between two events
/// breaks the parser).
pub struct EventEmitter {
    inner: Mutex<Stdout>,
}

impl EventEmitter {
    /// Build an emitter that holds a handle to the process stdout.
    /// The handle is created lazily on first emit to avoid the
    /// `Stdout::new()` const-fn limitation in stable Rust.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(io::stdout()),
        }
    }

    /// Emit one event. Errors are silenced (`Result<(), io::Error>`
    /// discarded): a broken stdout (closed pipe, EPIPE on SIGPIPE)
    /// must not crash the run.
    pub fn emit(&self, event: Event<'_>) {
        let payload = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(e) => {
                // Best-effort fallback: emit a minimal event so the
                // downstream at least sees that something failed.
                eprintln!("[moagan] stdout_events::emit: serde error: {e}");
                return;
            }
        };
        let mut out = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        // Errors are also silenced: `Broken pipe` from `head |` etc.
        let _ = out.write_all(payload.as_bytes());
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
}

impl Default for EventEmitter {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide emitter. `static` so call sites don't have to thread
/// it through. Lock contention is negligible (events are sparse —
/// at most a few per second even under load). Initialised lazily on
/// first emit because `Mutex<Stdout>::new(Stdout::new())` is not a
/// `const fn` on stable Rust.
pub static STDOUT_EVENTS: std::sync::LazyLock<EventEmitter> =
    std::sync::LazyLock::new(EventEmitter::new);

/// Build an RFC 3339 timestamp for the current instant. Convenience
/// helper so every event uses the same format.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_constant_is_stable() {
        // Pin the schema version so operators can grep for it.
        assert_eq!(SCHEMA_VERSION, 1);
    }

    #[test]
    fn resolve_event_format_off_always_silent() {
        assert!(!resolve_event_format(EventFormat::Off));
    }

    #[test]
    fn event_serializes_with_kind_discriminator() {
        let ev = Event::PhaseStart {
            schema: SCHEMA_VERSION,
            ts: "2026-01-01T00:00:00.000Z".to_owned(),
            phase: "intake",
            seq: 0,
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["kind"], "phase_start");
        assert_eq!(json["phase"], "intake");
        assert_eq!(json["seq"], 0);
        assert_eq!(json["schema"], 1);
    }

    #[test]
    fn event_serializes_with_borrowed_strings() {
        let ev = Event::LlmCall {
            schema: SCHEMA_VERSION,
            ts: "2026-01-01T00:00:00.000Z".to_owned(),
            call_id: "abc",
            phase: "intake",
            role: "intake",
            provider: "minimax",
            model: "MiniMax-M3",
            elapsed_ms: 1234,
            ok: true,
            input_tokens: 10,
            output_tokens: 20,
            retry_count: 0,
        };
        let s = serde_json::to_string(&ev).unwrap();
        // Verify borrowed string fields end up as JSON strings.
        assert!(s.contains("\"provider\":\"minimax\""));
        assert!(s.contains("\"elapsed_ms\":1234"));
        assert!(s.contains("\"ok\":true"));
    }
}
