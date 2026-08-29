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

/// Decision-event verbosity. Decoupled from [`EventFormat`] so the
/// `Decision` event stream can be silenced (`Off`) or saturated
/// (`All`) independently of the rest of the bus.
///
/// Three values, three audiences:
///
/// - `Summary` (default) — only the curated low-volume decisions
///   that summarise a run in a handful of lines (`winner_picked`,
///   `low_confidence_winner`, `cluster_skipped`, `repair_applied`,
///   `portfolio_finalized`). Operators get a tight summary by
///   default; the high-volume decisions are opt-in.
/// - `All` — every decision, including the per-LLM-call cache
///   observability (`cache_hit` / `cache_miss`), per-proposal judge
///   verdicts (`judge_verdict`), and per-sketch category assignments
///   (`category_assigned`). Volatile; intended for dashboards and
///   audits, NOT for run-of-the-mill console output.
/// - `Off` — silence every decision event. Use when the caller only
///   wants phase / llm_call events and is happy to drop the audit
///   trail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecisionFormat {
    /// Silence every `Decision` event.
    Off,
    /// Emit only the curated, low-volume decisions (default).
    Summary,
    /// Emit every decision, including per-LLM-call cache events.
    All,
}

/// Resolve the active [`DecisionFormat`]. Honours
/// `MOAGAN_DECISION_FORMAT` if set; falls through to the explicit
/// flag value otherwise. Mirrors [`resolve_event_format`]'s
/// precedence: env var > flag > default. Symmetry with
/// `resolve_event_format` keeps the operator mental model
/// consistent.
pub fn resolve_decision_format(explicit: DecisionFormat) -> DecisionFormat {
    if let Ok(s) = std::env::var("MOAGAN_DECISION_FORMAT") {
        return match s.to_ascii_lowercase().as_str() {
            "off" | "none" | "disable" => DecisionFormat::Off,
            "all" | "verbose" | "trace" => DecisionFormat::All,
            // Default to Summary for anything unrecognised: the
            // Summary level is the safe default for unknown / typo
            // values; an operator who actually wanted `All` will
            // notice the missing cache_hit events and correct.
            _ => DecisionFormat::Summary,
        };
    }
    explicit
}

/// Required emit-level for each [`decision_kind`] string. Internal
/// to this module — the public surface is the
/// `(DecisionFormat, &str) -> bool` helper [`should_emit_decision`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecisionLevel {
    /// Visible in [`DecisionFormat::Summary`] and `All`. Default
    /// for low-volume, run-summary decisions.
    Summary,
    /// Only visible in [`DecisionFormat::All`]. Default for
    /// high-volume events (cache, judge, category assignment) that
    /// would otherwise drown stdout under the default verbosity.
    AllOnly,
}

/// Map every curated `decision_kind` string to its required emit
/// level. New `decision_kind`s added by future commits MUST update
/// this table; an unknown kind defaults to `Summary` (the safe
/// side — every kind is visible until classified).
fn classification(kind: &str) -> DecisionLevel {
    match kind {
        // Low-volume, run-summary events — always visible.
        "winner_picked"
        | "low_confidence_winner"
        | "cluster_skipped"
        | "repair_applied"
        | "portfolio_finalized" => DecisionLevel::Summary,
        // High-volume events — opt-in via --decision-format=all.
        "category_assigned" | "judge_verdict" | "cache_hit" | "cache_miss" => {
            DecisionLevel::AllOnly
        }
        // Unknown kinds default to Summary: every kind is visible
        // until the table is updated.
        _ => DecisionLevel::Summary,
    }
}

/// Should the emitter emit a [`Event::Decision`] for the given
/// `kind` under the active [`DecisionFormat`]? Pure function; no
/// env-var lookups, no I/O. Call sites should prefer
/// [`emit_decision`] which wraps this helper plus the payload
/// construction.
pub fn should_emit_decision(level: DecisionFormat, kind: &str) -> bool {
    match (level, classification(kind)) {
        (DecisionFormat::Off, _) => false,
        (DecisionFormat::Summary, DecisionLevel::Summary) => true,
        (DecisionFormat::Summary, DecisionLevel::AllOnly) => false,
        (DecisionFormat::All, _) => true,
    }
}

/// Emit one [`Event::Decision`] line if [`should_emit_decision`]
/// approves the active level for `kind`. The `payload_fn` is
/// lazy-evaluated so callers don't pay the JSON construction cost
/// when the level is `Off` (the common case for the high-volume
/// `AllOnly` kinds under the default `Summary` verbosity).
///
/// Call sites look like:
///
/// ```ignore
/// crate::telemetry::stdout_events::emit_decision("winner_picked", || {
///     serde_json::json!({
///         "proposal_id": winner_id,
///         "score": top_score,
///     })
/// });
/// ```
///
/// The helper hides the `should_emit_decision` + `STDOUT_EVENTS.emit`
/// ceremony so the nine emit sites stay compact and uniform.
pub fn emit_decision<F>(kind: &'static str, payload_fn: F)
where
    F: FnOnce() -> serde_json::Value,
{
    let level = resolve_decision_format(DecisionFormat::Summary);
    if !should_emit_decision(level, kind) {
        return;
    }
    STDOUT_EVENTS.emit(Event::Decision {
        schema: SCHEMA_VERSION,
        ts: now_rfc3339(),
        decision_kind: kind,
        payload: payload_fn(),
    });
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

    /// Serialises the three `MOAGAN_DECISION_FORMAT`-mutating tests
    /// below. Without this lock, the parallel test runner lets sibling
    /// tests observe env state owned by a test mid-flight — for
    /// example, the `unknown_env_defaults_to_summary` test can fail
    /// when another test overwrites the env var to `"all"` between
    /// this test's `set_var("verbose-not-real")` and the read inside
    /// `resolve_decision_format`. Pinning the lock to the entire
    /// set-var → read → restore critical section eliminates the race.
    /// Same pattern as `src/phases/deliver.rs:621` and the sibling
    /// `src/llm/provider.rs:2123` `env_lock()` helper; see
    /// `docs/test-skips.md` §3 for the historical flake (PR #246).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    // -- Decision format helpers --------------------------------------

    /// `Off` silences every kind, including the curated Summary
    /// ones. The env-var precedence path is exercised separately
    /// (this test pins the in-process resolution).
    #[test]
    fn should_emit_decision_off_silences_every_kind() {
        assert!(!should_emit_decision(DecisionFormat::Off, "winner_picked"));
        assert!(!should_emit_decision(
            DecisionFormat::Off,
            "portfolio_finalized"
        ));
        assert!(!should_emit_decision(DecisionFormat::Off, "cache_hit"));
        assert!(!should_emit_decision(DecisionFormat::Off, "cache_miss"));
        assert!(!should_emit_decision(DecisionFormat::Off, "judge_verdict"));
        assert!(!should_emit_decision(
            DecisionFormat::Off,
            "category_assigned"
        ));
    }

    /// `Summary` admits the five curated low-volume kinds and
    /// suppresses the four AllOnly ones. Pins the curated split
    /// documented in `docs/events-v1.md`.
    #[test]
    fn should_emit_decision_summary_classification() {
        // Summary-level kinds are visible.
        for kind in [
            "winner_picked",
            "low_confidence_winner",
            "cluster_skipped",
            "repair_applied",
            "portfolio_finalized",
        ] {
            assert!(
                should_emit_decision(DecisionFormat::Summary, kind),
                "kind {kind:?} must be visible under Summary"
            );
        }
        // AllOnly kinds are hidden under Summary.
        for kind in [
            "category_assigned",
            "judge_verdict",
            "cache_hit",
            "cache_miss",
        ] {
            assert!(
                !should_emit_decision(DecisionFormat::Summary, kind),
                "kind {kind:?} must be hidden under Summary"
            );
        }
    }

    /// `All` admits every classified kind AND every unknown kind
    /// (the safe default for new emit sites the table hasn't been
    /// updated for yet).
    #[test]
    fn should_emit_decision_all_admits_every_kind() {
        for kind in [
            "winner_picked",
            "portfolio_finalized",
            "cache_hit",
            "cache_miss",
            "judge_verdict",
            "category_assigned",
            "future_kind_not_yet_classified",
        ] {
            assert!(
                should_emit_decision(DecisionFormat::All, kind),
                "kind {kind:?} must be visible under All"
            );
        }
    }

    /// Unknown kinds default to Summary-safe (visible under Summary
    /// and All; hidden under Off). Pins the contract for future
    /// emit sites that pre-date an update to the classification
    /// table.
    #[test]
    fn should_emit_decision_unknown_kind_defaults_to_summary_safe() {
        assert!(should_emit_decision(
            DecisionFormat::Summary,
            "future_kind_v0"
        ));
        assert!(should_emit_decision(DecisionFormat::All, "future_kind_v0"));
        assert!(!should_emit_decision(DecisionFormat::Off, "future_kind_v0"));
    }

    /// Env-var precedence: `MOAGAN_DECISION_FORMAT=off` wins over
    /// the explicit `All` flag (and over every other explicit
    /// value). Symmetric with `MOAGAN_EVENT_FORMAT` precedence.
    #[test]
    fn resolve_decision_format_env_var_wins_over_explicit() {
        // Serialise against sibling env-var tests; see `ENV_LOCK`
        // doc comment above.
        let _g = match ENV_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        // Save and restore to keep the test hermetic.
        let prev = std::env::var("MOAGAN_DECISION_FORMAT").ok();
        unsafe {
            std::env::set_var("MOAGAN_DECISION_FORMAT", "off");
        }
        let resolved = resolve_decision_format(DecisionFormat::All);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("MOAGAN_DECISION_FORMAT", v),
                None => std::env::remove_var("MOAGAN_DECISION_FORMAT"),
            }
        }
        assert_eq!(
            resolved,
            DecisionFormat::Off,
            "MOAGAN_DECISION_FORMAT=off must override explicit All"
        );
    }

    /// Env-var precedence: `MOAGAN_DECISION_FORMAT=all` wins over
    /// the explicit `Summary` flag.
    #[test]
    fn resolve_decision_format_env_var_all_wins_over_summary() {
        let _g = match ENV_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let prev = std::env::var("MOAGAN_DECISION_FORMAT").ok();
        unsafe {
            std::env::set_var("MOAGAN_DECISION_FORMAT", "all");
        }
        let resolved = resolve_decision_format(DecisionFormat::Summary);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("MOAGAN_DECISION_FORMAT", v),
                None => std::env::remove_var("MOAGAN_DECISION_FORMAT"),
            }
        }
        assert_eq!(
            resolved,
            DecisionFormat::All,
            "MOAGAN_DECISION_FORMAT=all must override explicit Summary"
        );
    }

    /// Unrecognised env values default to Summary (the safe
    /// default for typos / unfamiliar names).
    #[test]
    fn resolve_decision_format_unknown_env_defaults_to_summary() {
        let _g = match ENV_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let prev = std::env::var("MOAGAN_DECISION_FORMAT").ok();
        unsafe {
            std::env::set_var("MOAGAN_DECISION_FORMAT", "verbose-not-real");
        }
        let resolved = resolve_decision_format(DecisionFormat::Off);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("MOAGAN_DECISION_FORMAT", v),
                None => std::env::remove_var("MOAGAN_DECISION_FORMAT"),
            }
        }
        assert_eq!(
            resolved,
            DecisionFormat::Summary,
            "unrecognised env value must default to Summary"
        );
    }

    /// The `Decision` event serialises with `decision_kind` and
    /// `payload` exactly as the wire schema promises.
    #[test]
    fn decision_event_serializes_with_payload() {
        let ev = Event::Decision {
            schema: SCHEMA_VERSION,
            ts: "2026-01-01T00:00:00.000Z".to_owned(),
            decision_kind: "winner_picked",
            payload: serde_json::json!({
                "proposal_id": "p_000",
                "score": 8.4,
            }),
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["kind"], "decision");
        assert_eq!(json["decision_kind"], "winner_picked");
        assert_eq!(json["payload"]["proposal_id"], "p_000");
        assert!((json["payload"]["score"].as_f64().unwrap() - 8.4).abs() < 1e-9);
        assert_eq!(json["schema"], 1);
    }
}
