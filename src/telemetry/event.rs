//! D.17.1: `TelemetryEvent` enum with 17 canonical variants that
//! cover Run/Phase/Call lifecycle, discovery saturation, cache,
//! circuit, budget, cancel, stale artifacts, warnings, and hostile
//! prompts. Each variant serializes to snake_case JSON via the
//! `kind` tag so downstream consumers can match uniformly.

#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TelemetryEvent {
    /// A run started. Emitted once per `moagan run` invocation.
    RunStart {
        run_id: String,
        mode: String,
        at_unix: i64,
    },
    /// A run ended (any terminal state: `ok`, `error`, `cancelled`).
    RunEnd {
        run_id: String,
        status: String,
        at_unix: i64,
    },
    /// A phase started.
    PhaseStart {
        run_id: String,
        phase: String,
        at_unix: i64,
    },
    /// A phase ended. `tokens` is the bill attributed to the phase.
    PhaseEnd {
        run_id: String,
        phase: String,
        at_unix: i64,
        tokens: u64,
    },
    /// A single LLM call started (before HTTP).
    CallStart {
        run_id: String,
        role: String,
        model: String,
        at_unix: i64,
    },
    /// A single LLM call ended.
    CallEnd {
        run_id: String,
        role: String,
        status: String,
        at_unix: i64,
        tokens: u64,
    },
    /// Discovery loop saturated. `coverage` is 0..=1.
    DiscoverySaturated {
        run_id: String,
        coverage: f32,
        at_unix: i64,
    },
    /// Cache hit for a role.
    CacheHit {
        run_id: String,
        role: String,
        at_unix: i64,
    },
    /// Cache miss (HTTP issued).
    CacheMiss {
        run_id: String,
        role: String,
        at_unix: i64,
    },
    /// Circuit breaker opened for a provider.
    CircuitOpen { provider: String, at_unix: i64 },
    /// Circuit breaker closed for a provider.
    CircuitClose { provider: String, at_unix: i64 },
    /// Soft budget threshold reached (e.g. 80% of planned tokens).
    BudgetSoft {
        run_id: String,
        used: u64,
        planned: u64,
        at_unix: i64,
    },
    /// Hard budget cap hit. The run must stop promptly.
    BudgetHard {
        run_id: String,
        used: u64,
        planned: u64,
        at_unix: i64,
    },
    /// Cancellation requested at a specific tier.
    Cancel {
        run_id: String,
        tier: String,
        at_unix: i64,
    },
    /// Stale artifact detected on disk beyond the retention window.
    StaleArtifact {
        path: String,
        age_secs: u64,
        at_unix: i64,
    },
    /// Generic warning. `code` is the warning key, `message` the
    /// human-readable detail.
    Warning {
        run_id: String,
        code: String,
        message: String,
        at_unix: i64,
    },
    /// Hostile prompt detected by guard rails.
    HostilePrompt {
        run_id: String,
        verdict: String,
        at_unix: i64,
    },
}

impl TelemetryEvent {
    /// Emit the event via `tracing::info!` with the JSON payload.
    pub fn emit(&self) {
        let json = serde_json::to_string(self).unwrap_or_default();
        tracing::info!(event = %json, "TelemetryEvent");
    }
}
