//! D.17.1: `TelemetryEvent` enum — a parallel tracing-layer stream
//! carrying only the events that have an active producer
//! (`PhaseStart`, `DiscoverySaturated`, `StaleArtifact`). Each variant
//! serializes to snake_case JSON via the `kind` tag so downstream
//! consumers (`src/phases/rank.rs`, operators grepping the JSON log
//! stream, the dashboard) can match uniformly on `kind`.
//!
//! This enum is **not** the canonical NDJSON event surface — that
//! is `crate::telemetry::stdout_events::Event`, documented in
//! [`docs/events-v1.md`](../../docs/events-v1.md). `TelemetryEvent`
//! is emitted via `tracing::info!(event = <json>, …)` from a small
//! number of orchestrator paths; its payload rides the tracing
//! layer (stdout/stderr JSON when `MOAGAN_LOG_FORMAT=json`, pretty
//! text on a TTY) and operators grep their own log capture — it
//! does **not** flow into `telemetry/calls.jsonl.gz`, which is the
//! per-LLM-call `CallEvent` stream (`src/telemetry/mod.rs`) and
//! carries no `kind` discriminator.
//!
//! # Variants removed in earlier audits
//!
//! `BudgetSoft`, `BudgetHard`, `Cancel` were dropped before the
//! v0.14.x sweep (per commit history).
//!
//! # Variants removed in v0.14.x
//!
//! The earlier 14-variant surface (`RunStart`, `RunEnd`, `PhaseEnd`,
//! `CallStart`, `CallEnd`, `CacheHit`, `CacheMiss`, `CircuitOpen`,
//! `CircuitClose`, `Warning`, `HostilePrompt`, plus the older
//! `BudgetSoft` / `BudgetHard` / `Cancel`) was declared for D.17.1
//! but never had a producer — no `Telemetry::run_start`,
//! `Telemetry::call_end`, or equivalent method existed, so no call
//! site dispatched them. The 11 v0.14.x-swept variants
//! (`RunStart`, `RunEnd`, `PhaseEnd`, `CallStart`, `CallEnd`,
//! `CacheHit`, `CacheMiss`, `CircuitOpen`, `CircuitClose`,
//! `Warning`, `HostilePrompt`) had zero producers in `src/` or
//! `tests/`; the JSON wire shapes are recoverable from git
//! history if a future hook needs them.

/// Parallel tracing-layer event surface (see module docs above).
/// Carries only the variants the audit pipeline currently consumes.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TelemetryEvent {
    /// A phase started.
    PhaseStart {
        /// Run id.
        run_id: String,
        /// Phase name.
        phase: String,
        /// Unix timestamp (seconds).
        at_unix: i64,
    },
    /// Discovery loop saturated.
    DiscoverySaturated {
        /// Run id.
        run_id: String,
        /// Coverage fraction in `0..=1`.
        coverage: f32,
        /// Unix timestamp (seconds).
        at_unix: i64,
    },
    /// Stale artifact detected on disk beyond the retention window.
    /// `ttl_secs` is `None` for synthetic emits (rank/refine drop
    /// events where the TTL context is unknown) and `Some(ttl)`
    /// for filesystem-driven emits (resume path, util::detect_stale).
    /// The `Option` is skipped during JSON serialization when absent
    /// so existing consumers (the unit tests pinning the wire form
    /// at `src/phases/rank.rs:1572-1598`) keep working unchanged.
    StaleArtifact {
        /// On-disk path that exceeded the TTL.
        path: String,
        /// Age of the artefact in seconds at the moment of detection.
        age_secs: u64,
        /// Configured TTL in seconds; `None` for synthetic emits.
        #[serde(skip_serializing_if = "Option::is_none")]
        ttl_secs: Option<u64>,
        /// Unix timestamp (seconds).
        at_unix: i64,
    },
}

impl TelemetryEvent {
    /// Emit the event via `tracing::info!` with the JSON payload.
    pub fn emit(&self) {
        let json = serde_json::to_string(self).unwrap_or_default();
        tracing::trace!("TelemetryEvent::emit: emitting");
        tracing::info!(event = %json, "TelemetryEvent");
    }
}
