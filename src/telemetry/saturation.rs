//! `SaturationEvent` runtime + storage.
//!
//! Implements the push-side of the catalog §D.23 + §D.27 telemetry
//! contract (add-on `10-integrada-v0`). Three kinds of saturation
//! are surfaced from the runtime to the SQLite mirror
//! (`saturation_events` table, v018):
//!
//! | kind         | trigger                                                    |
//! |--------------|------------------------------------------------------------|
//! | `error`      | provider circuit breaker opened (catalog §D.19.5)         |
//! | `rate_limit` | token-bucket budget exhausted (catalog §D.19.6)          |
//! | `token`      | plan / budget threshold crossed (catalog §D.19.8)         |
//!
//! The runtime fires [`SaturationEvent`] through a callback attached
//! to each [`crate::llm::provider::BreakeredProvider`] so the wrapper
//! stays telemetry-agnostic. The callback is wired to the
//! `Telemetry::saturation` sink at registry construction time, which
//! mirrors the event to both the per-run JSONL stream
//! (`telemetry/saturation.jsonl`) and the SQLite index.
//!
//! The CLI consumer lives in `src/cli/telemetry_cmd.rs::alerts`; the
//! public read path is [`crate::storage::sqlite::Db::list_saturation_events`].

use serde::Serialize;

use crate::time::now_unix_secs;

/// Discriminator for the three runtime saturation signals.
///
/// Stored in the `kind` column of `saturation_events` (v018) and as
/// the `kind` field of the on-disk JSONL record. The set is closed:
/// adding a new variant requires a new migration so legacy rows keep
/// deserialising.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaturationKind {
    /// Provider circuit breaker opened (catalog §D.19.5).
    /// `threshold_pct` is `100.0` (the breaker is fully open).
    Error,
    /// Token-bucket budget exhausted (catalog §D.19.6). The call
    /// was rejected because the next refill would have exceeded the
    /// configured `max_wait`. `threshold_pct` is the bucket level
    /// at the time of rejection.
    RateLimit,
    /// Plan / budget threshold crossed (catalog §D.19.8). The
    /// `window_days` plan consumption is at or above the configured
    /// limit. `threshold_pct` is the consumption percentage.
    Token,
}

impl SaturationKind {
    /// Canonical lowercase token used both on disk and in SQL.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::RateLimit => "rate_limit",
            Self::Token => "token",
        }
    }
}

impl std::fmt::Display for SaturationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SaturationKind {
    type Err = crate::error::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "error" => Ok(Self::Error),
            "rate_limit" => Ok(Self::RateLimit),
            "token" => Ok(Self::Token),
            other => Err(crate::error::Error::InvalidArgs(format!(
                "invalid saturation kind '{other}' (expected 'error', 'rate_limit', or 'token')"
            ))),
        }
    }
}

/// One saturation event fired by the runtime.
///
/// Mirrors the `saturation_events` SQLite row (v018) and the
/// per-run `telemetry/saturation.jsonl` line. The struct is cheap to
/// clone; the runtime typically builds it inline before firing it
/// through the [`crate::llm::provider::BreakeredProvider`] sink.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SaturationEvent {
    /// Run id (optional so pre-pipeline probes still record).
    pub run_id: Option<String>,
    /// Provider name (e.g. `minimax`, `mock`, `opencode_go`).
    pub provider: String,
    /// Model name (e.g. `MiniMax-M3`).
    pub model: String,
    /// Kind discriminator. See [`SaturationKind`].
    pub kind: SaturationKind,
    /// Saturation percentage at trigger time (0.0–100.0). The
    /// runtime computes this per-kind; the constructors below pin
    /// the right value so callers cannot drift.
    pub threshold_pct: f32,
    /// Unix seconds when the event was observed.
    pub observed_at_unix: i64,
    /// Free-form structured details (JSON-encoded). Never contains
    /// raw payloads; only counts and bucket/breaker state at trigger
    /// time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl SaturationEvent {
    /// Build a `SaturationKind::Error` event from a circuit breaker
    /// opening. The threshold is always `100.0` (the breaker is
    /// fully open at this point). `details` carries the breaker
    /// `failure_count` so post-mortem can correlate the event with
    /// the threshold knob in `Config`.
    pub fn from_circuit_breaker(
        provider: impl Into<String>,
        model: impl Into<String>,
        run_id: Option<String>,
        failure_count: u32,
    ) -> Self {
        let provider = provider.into();
        let model = model.into();
        tracing::info!(
            provider = %provider,
            model = %model,
            failure_count,
            has_run_id = run_id.is_some(),
            "SaturationEvent::from_circuit_breaker"
        );
        Self {
            run_id,
            provider,
            model,
            kind: SaturationKind::Error,
            threshold_pct: 100.0,
            observed_at_unix: now_unix_secs(),
            details: Some(serde_json::json!({ "failure_count": failure_count })),
        }
    }

    /// Build a `SaturationKind::RateLimit` event from a token-bucket
    /// rejection. `threshold_pct` is the bucket level at the time
    /// of rejection (e.g. `5.0` for a bucket at 5% when the call
    /// was rejected). `details` carries `capacity` and
    /// `refill_per_sec` for post-mortem correlation.
    pub fn from_rate_limit(
        provider: impl Into<String>,
        model: impl Into<String>,
        run_id: Option<String>,
        threshold_pct: f32,
        capacity: u32,
        refill_per_sec: u32,
    ) -> Self {
        let provider = provider.into();
        let model = model.into();
        tracing::warn!(
            provider = %provider,
            model = %model,
            threshold_pct,
            capacity,
            refill_per_sec,
            "SaturationEvent::from_rate_limit"
        );
        Self {
            run_id,
            provider,
            model,
            kind: SaturationKind::RateLimit,
            threshold_pct,
            observed_at_unix: now_unix_secs(),
            details: Some(serde_json::json!({
                "capacity": capacity,
                "refill_per_sec": refill_per_sec,
            })),
        }
    }

    /// Build a `SaturationKind::Token` event from a plan / budget
    /// threshold crossing. `threshold_pct` is the consumption
    /// percentage (e.g. `85.0` for an 85%-of-limit warning). The
    /// runtime fires this through the same sink so the
    /// `moagan telemetry alerts list` consumer does not need to
    /// special-case it.
    pub fn from_token_saturation(
        provider: impl Into<String>,
        model: impl Into<String>,
        run_id: Option<String>,
        threshold_pct: f32,
    ) -> Self {
        let provider = provider.into();
        let model = model.into();
        tracing::info!(
            provider = %provider,
            model = %model,
            threshold_pct,
            "SaturationEvent::from_token_saturation"
        );
        Self {
            run_id,
            provider,
            model,
            kind: SaturationKind::Token,
            threshold_pct,
            observed_at_unix: now_unix_secs(),
            details: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_round_trip() {
        for k in [
            SaturationKind::Error,
            SaturationKind::RateLimit,
            SaturationKind::Token,
        ] {
            let s = k.as_str();
            let back: SaturationKind = s.parse().unwrap();
            assert_eq!(back, k);
            assert_eq!(format!("{k}"), s);
        }
        assert!("nope".parse::<SaturationKind>().is_err());
    }

    #[test]
    fn from_circuit_breaker_pins_threshold_at_100() {
        let ev = SaturationEvent::from_circuit_breaker(
            "minimax",
            "MiniMax-M3",
            Some("0190-…".into()),
            5,
        );
        assert_eq!(ev.provider, "minimax");
        assert_eq!(ev.model, "MiniMax-M3");
        assert_eq!(ev.kind, SaturationKind::Error);
        assert!((ev.threshold_pct - 100.0).abs() < f32::EPSILON);
        assert!(ev.details.as_ref().unwrap().get("failure_count").is_some());
    }

    #[test]
    fn from_rate_limit_preserves_threshold() {
        let ev = SaturationEvent::from_rate_limit("opencode_go", "go-mini", None, 12.5, 60, 1);
        assert_eq!(ev.kind, SaturationKind::RateLimit);
        assert!((ev.threshold_pct - 12.5).abs() < 1e-3);
        let details = ev.details.as_ref().unwrap();
        assert_eq!(details.get("capacity").unwrap().as_u64().unwrap(), 60);
        assert_eq!(details.get("refill_per_sec").unwrap().as_u64().unwrap(), 1);
    }

    #[test]
    fn from_token_saturation_has_no_details() {
        let ev = SaturationEvent::from_token_saturation("deepseek", "deepseek-chat", None, 85.0);
        assert_eq!(ev.kind, SaturationKind::Token);
        assert!((ev.threshold_pct - 85.0).abs() < f32::EPSILON);
        assert!(ev.details.is_none());
    }

    #[test]
    fn serialise_round_trip_jsonl() {
        let ev =
            SaturationEvent::from_circuit_breaker("minimax", "MiniMax-M3", Some("0190".into()), 5);
        let s = serde_json::to_string(&ev).unwrap();
        let back: SaturationEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back.provider, ev.provider);
        assert_eq!(back.model, ev.model);
        assert_eq!(back.kind, ev.kind);
        assert_eq!(back.observed_at_unix, ev.observed_at_unix);
    }
}
