//! Freshness of the auto-probe sidecars (#1003).
//!
//! The registry auto-probes (`max_tokens`, temperature, `top_p`,
//! `top_k`) used to re-verify every cached row on every invocation:
//! ~24 HTTP requests, ~30 s on a `continue` that had nothing to do. A
//! row whose `verified_at` is younger than the TTL is now trusted as
//! is and its probe is not spawned. The TTL comes from
//! `MOAGAN_PROBE_TTL_SECS` (default one day); `0` verifies on every
//! invocation, as before. `moagan probe <kind>` still probes on demand.

use std::time::Duration;

use chrono::{DateTime, Utc};

/// Env var that overrides [`DEFAULT_PROBE_TTL`], in whole seconds.
pub const PROBE_TTL_ENV: &str = "MOAGAN_PROBE_TTL_SECS";

/// How long a verified sidecar row is trusted without a new probe.
pub const DEFAULT_PROBE_TTL: Duration = Duration::from_secs(86_400);

/// The TTL for a raw `MOAGAN_PROBE_TTL_SECS` value: unset or blank →
/// [`DEFAULT_PROBE_TTL`]; a whole number of seconds (surrounding
/// whitespace allowed, `0` included) → that many seconds; anything
/// else → [`DEFAULT_PROBE_TTL`] with a `warn!` naming the value.
pub fn parse_ttl(raw: Option<&str>) -> Duration {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return DEFAULT_PROBE_TTL;
    };
    match raw.parse::<u64>() {
        Ok(secs) => Duration::from_secs(secs),
        Err(_) => {
            tracing::warn!(
                env = PROBE_TTL_ENV,
                value = raw,
                default_secs = DEFAULT_PROBE_TTL.as_secs(),
                "probe_ttl: not a whole number of seconds; using the default"
            );
            DEFAULT_PROBE_TTL
        }
    }
}

/// The TTL from the process environment ([`PROBE_TTL_ENV`]).
pub fn ttl_from_env() -> Duration {
    parse_ttl(std::env::var(PROBE_TTL_ENV).ok().as_deref())
}

/// Whether a row last verified at `verified_at` (RFC 3339) is still
/// fresh at `now`: `0 <= now - verified_at < ttl`. A timestamp that
/// does not parse, or lies in the future, is not fresh (the probe
/// runs), and a zero TTL is never fresh.
pub fn is_fresh(verified_at: &str, now: DateTime<Utc>, ttl: Duration) -> bool {
    let Ok(verified) = DateTime::parse_from_rfc3339(verified_at.trim()) else {
        return false;
    };
    let Ok(age) = now
        .signed_duration_since(verified.with_timezone(&Utc))
        .to_std()
    else {
        return false;
    };
    age < ttl
}

#[cfg(test)]
mod tests;
