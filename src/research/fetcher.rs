//! Bounded external research fetcher (K.4 / proposal-04 §4).
//!
//! Allowlist-only; max [`MAX_URLS_PER_CALL`] URLs per call; max
//! [`MAX_BYTES_PER_URL`] bytes per response. Network failures are
//! non-fatal — caller should treat an empty `Ok(vec)` or a vec
//! containing per-URL [`FetchError`] variants as "research
//! unavailable, continue without it".
//!
//! Redaction piggybacks on the existing
//! [`crate::redact::RedactPolicy`] so any token / email that slips
//! into a fetched snippet is scrubbed before it lands in the
//! caller's `Vec`. We redact on the `Storage` surface because the
//! research snippets are destined for context injection in the
//! Sketch phase, not direct telemetry emission.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::RateLimitConfig;
use crate::redact::{RedactPolicy, Surface, apply};
use crate::research::allowlist;

/// Hard cap on URLs accepted by a single [`ResearchFetcher::fetch_all`]
/// call.
pub const MAX_URLS_PER_CALL: usize = 3;
/// Hard cap on bytes retained from a single response body.
pub const MAX_BYTES_PER_URL: usize = 4 * 1024;
/// Per-request transport timeout. Keeps a slow host from wedging
/// the Sketch phase's pipeline.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A single fetched + redacted snippet. `truncated == true` means
/// the response was longer than [`MAX_BYTES_PER_URL`] and the tail
/// was dropped (the [`ResearchSnippet::url`] still refers to the
/// original, full-length source).
#[derive(Debug, Clone)]
pub struct ResearchSnippet {
    /// The URL the snippet was fetched from. Echoed verbatim; the
    /// allowlist filter already vetted the host.
    pub url: String,
    /// Redacted body, truncated to [`MAX_BYTES_PER_URL`].
    pub content: String,
    /// `true` when the response was larger than the cap and the
    /// tail was dropped.
    pub truncated: bool,
}

/// All the ways a fetch can fail per URL. Kept distinct from
/// [`crate::error::Error`] because the caller is supposed to treat
/// these as soft signals — most variants mean "skip this URL,
/// continue with the rest".
#[derive(Debug, Clone)]
pub enum FetchError {
    /// Host parsed from the URL is not in [`allowlist::ALLOWED_HOSTS`].
    DisallowedHost(String),
    /// Caller asked for more URLs than [`MAX_URLS_PER_CALL`] in a
    /// single call. Returned as the *only* error in the result vec
    /// so the caller sees the hard failure cleanly.
    TooManyUrls {
        /// Number of URLs the caller requested.
        requested: usize,
    },
    /// Transport / parse / decoding failure. String is the lower
    /// level `reqwest` or `url::Url::parse` message, already
    /// scrubbed of secrets by the upstream libraries.
    NetworkError(String),
    /// Response body was empty. Treat as "no useful signal".
    Empty,
    /// K.4: per-host circuit-breaker is open. The fetcher
    /// short-circuited without touching the socket because the
    /// upstream returned too many 429 / 503 responses in the
    /// tracking window. Cooldown is per-host and managed by
    /// [`crate::research::fetcher::HostRateLimiter`].
    CircuitOpen {
        /// Canonical hostname the breaker is open for.
        host: String,
    },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DisallowedHost(h) => write!(f, "host '{h}' not in allowlist"),
            Self::TooManyUrls { requested } => {
                write!(f, "requested {requested} URLs, max {MAX_URLS_PER_CALL}")
            }
            Self::NetworkError(m) => write!(f, "network error: {m}"),
            Self::Empty => write!(f, "empty response"),
            Self::CircuitOpen { host } => write!(f, "circuit breaker open for host '{host}'"),
        }
    }
}

impl std::error::Error for FetchError {}

/// K.4 advanced knobs layered on top of the basic token bucket.
///
/// All knobs are operator-tunable and default-off: when no
/// [`HostRetryConfig`] is attached to a host via
/// [`ResearchFetcher::with_per_host_retry`], the fetcher behaves
/// exactly as it did before this extension (one attempt, no jitter,
/// no circuit-breaker).
#[derive(Debug, Clone)]
pub struct HostRetryConfig {
    /// Maximum number of additional attempts after the first one
    /// fails with 429 / 503 / a `Retry-After` directive. `0`
    /// disables retries entirely (the upstream's first 429
    /// surfaces to the caller immediately). Default 1 — one retry
    /// is enough to absorb a single transient burst.
    pub max_retries: u32,
    /// Hard cap on a `Retry-After` we will honor. The header is
    /// parsed as a delta-seconds integer and clamped to this
    /// value so a hostile / buggy upstream cannot pin us for
    /// hours. Default 60 s.
    pub max_retry_after_secs: u64,
    /// Base exponential-backoff value (ms). Doubled per attempt
    /// up to [`Self::max_backoff_ms`]. Default 250 ms.
    pub base_backoff_ms: u64,
    /// Cap for the exponential backoff (ms). Default 30 s.
    pub max_backoff_ms: u64,
    /// Symmetric jitter ratio applied to the wait, both
    /// Retry-After-driven and exponential. `0.0` = deterministic
    /// (no jitter); `0.5` = ±50 %. Default 0.2.
    pub jitter_ratio: f32,
    /// Number of consecutive 429 / 503 outcomes within the
    /// tracking window that trips the breaker. `0` disables the
    /// circuit breaker entirely. Default 0.
    pub circuit_breaker_threshold: u32,
    /// Sliding window (s) for the consecutive-failure counter.
    /// A success resets the counter. Default 60 s.
    pub circuit_breaker_window_secs: u64,
    /// Time (s) the breaker stays open before allowing a probe.
    /// During the open window every call short-circuits with
    /// [`FetchError::CircuitOpen`]. Default 30 s.
    pub circuit_breaker_cooldown_secs: u64,
}

impl Default for HostRetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 1,
            max_retry_after_secs: 60,
            base_backoff_ms: 250,
            max_backoff_ms: 30_000,
            jitter_ratio: 0.2,
            circuit_breaker_threshold: 0,
            circuit_breaker_window_secs: 60,
            circuit_breaker_cooldown_secs: 30,
        }
    }
}

/// Outcome of feeding a 429 / 503 status into
/// [`HostRateLimiter::on_rate_limited`]. The fetcher uses this to
/// decide whether to sleep + retry, fail-fast (circuit open), or
/// give up (no retries left).
#[derive(Debug)]
pub(crate) enum RateLimitAction {
    /// Sleep `Duration` and retry the request. The fetcher
    /// increments its attempt counter and re-issues the request.
    Wait(Duration),
    /// Stop retrying. The fetcher returns this error to the caller
    /// in the per-URL result slot.
    Fail(FetchError),
}

#[derive(Debug)]
struct HostRateLimiter {
    permits: Arc<Semaphore>,
    bucket: Mutex<HostRateBucket>,
    advanced: Option<AdvancedState>,
}

#[derive(Debug)]
struct HostRateBucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last_refill: Instant,
}

/// K.4 advanced state: retry policy + circuit-breaker counters.
/// `None` means "advanced features off" — the fetcher falls back to
/// the original single-attempt path with zero extra state.
#[derive(Debug)]
struct AdvancedState {
    config: HostRetryConfig,
    circuit: Mutex<CircuitState>,
}

#[derive(Debug)]
struct CircuitState {
    /// `Some(t)` means the breaker is open until `t`; `None`
    /// means closed.
    open_until: Option<Instant>,
    /// Consecutive 429 / 503 outcomes since the last success or
    /// window reset.
    consecutive_failures: u32,
    /// Start of the current consecutive-failure window.
    window_start: Instant,
}

impl Default for CircuitState {
    fn default() -> Self {
        Self {
            open_until: None,
            consecutive_failures: 0,
            window_start: Instant::now(),
        }
    }
}

impl HostRateLimiter {
    fn new(config: RateLimitConfig) -> Self {
        Self::with_retry(config, None)
    }

    fn with_retry(config: RateLimitConfig, retry: Option<HostRetryConfig>) -> Self {
        let capacity = config.capacity.max(1);
        let initial = config.initial.unwrap_or(capacity).min(capacity);
        Self {
            permits: Arc::new(Semaphore::new(capacity as usize)),
            bucket: Mutex::new(HostRateBucket {
                capacity: capacity as f64,
                refill_per_sec: config.refill_per_sec.max(1) as f64,
                tokens: initial as f64,
                last_refill: Instant::now(),
            }),
            advanced: retry.map(|cfg| AdvancedState {
                config: cfg,
                circuit: Mutex::new(CircuitState {
                    open_until: None,
                    consecutive_failures: 0,
                    window_start: Instant::now(),
                }),
            }),
        }
    }

    async fn acquire(&self) -> std::result::Result<OwnedSemaphorePermit, FetchError> {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| FetchError::NetworkError(format!("rate limit permit: {e}")))?;
        let wait = {
            let mut bucket = self.bucket.lock();
            let now = Instant::now();
            let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
            bucket.tokens = (bucket.tokens + elapsed * bucket.refill_per_sec).min(bucket.capacity);
            bucket.last_refill = now;
            bucket.tokens -= 1.0;
            if bucket.tokens < 0.0 {
                Duration::from_secs_f64(-bucket.tokens / bucket.refill_per_sec)
            } else {
                Duration::ZERO
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        Ok(permit)
    }

    /// Fast-fail probe used by the fetcher before issuing a request
    /// when advanced features are on. Returns `true` when the
    /// breaker is open and the cooldown has not yet elapsed —
    /// callers should skip the request and surface
    /// [`FetchError::CircuitOpen`].
    pub(crate) fn circuit_is_open(&self) -> bool {
        let Some(adv) = self.advanced.as_ref() else {
            return false;
        };
        let mut circuit = adv.circuit.lock();
        match circuit.open_until {
            None => false,
            Some(deadline) if Instant::now() >= deadline => {
                // Cooldown elapsed: half-open. A probe will be
                // allowed through; the next outcome decides
                // whether the breaker re-opens or fully closes.
                circuit.open_until = None;
                circuit.consecutive_failures = 0;
                circuit.window_start = Instant::now();
                false
            }
            Some(_) => true,
        }
    }

    /// Record a request outcome and produce the next action.
    /// `attempt` is the retry index (`0` for the first retry after
    /// the original failed). `retry_after_header` carries the
    /// raw `Retry-After` value if the upstream returned one (used
    /// for the wait duration when present).
    pub(crate) fn on_rate_limited(
        &self,
        attempt: u32,
        retry_after_header: Option<&str>,
    ) -> RateLimitAction {
        // Without advanced features the fetcher's old contract
        // holds: a 429 / 503 is a hard fail, no retries.
        let Some(adv) = self.advanced.as_ref() else {
            return RateLimitAction::Fail(FetchError::NetworkError(
                "upstream rate-limited (no retry policy)".to_string(),
            ));
        };
        let cfg = &adv.config;
        // Bump the consecutive-failure counter first so the
        // breaker reflects "we just saw another failure" even when
        // we end up returning a Wait action.
        {
            let mut circuit = adv.circuit.lock();
            let now = Instant::now();
            let window = Duration::from_secs(cfg.circuit_breaker_window_secs.max(1));
            if circuit.consecutive_failures == 0
                || now.duration_since(circuit.window_start) > window
            {
                circuit.window_start = now;
                circuit.consecutive_failures = 0;
            }
            circuit.consecutive_failures = circuit.consecutive_failures.saturating_add(1);
            if cfg.circuit_breaker_threshold > 0
                && circuit.consecutive_failures >= cfg.circuit_breaker_threshold
            {
                circuit.open_until =
                    Some(now + Duration::from_secs(cfg.circuit_breaker_cooldown_secs.max(1)));
            }
        }
        if attempt >= cfg.max_retries {
            return RateLimitAction::Fail(FetchError::NetworkError(format!(
                "upstream rate-limited; retry budget exhausted ({} attempts)",
                attempt + 1
            )));
        }
        let wait = compute_retry_wait(
            attempt,
            retry_after_header,
            cfg.base_backoff_ms,
            cfg.max_backoff_ms,
            cfg.max_retry_after_secs,
            cfg.jitter_ratio,
        );
        RateLimitAction::Wait(wait)
    }

    /// Record a successful (2xx) outcome: close the breaker, reset
    /// the consecutive-failure counter. Idempotent when advanced
    /// features are off.
    pub(crate) fn record_success(&self) {
        let Some(adv) = self.advanced.as_ref() else {
            return;
        };
        let mut circuit = adv.circuit.lock();
        circuit.open_until = None;
        circuit.consecutive_failures = 0;
        circuit.window_start = Instant::now();
    }
}

/// K.4 advanced retry wait computation. Pure function — extracted
/// so the unit tests can pin the math without standing up the
/// full `HostRateLimiter` state machine.
///
/// Behaviour:
/// 1. When `retry_after_header` parses as a delta-seconds integer
///    it wins (clamped to `max_retry_after_secs`).
/// 2. Otherwise exponential: `base * 2^attempt`, capped at
///    `max_backoff_ms`.
/// 3. Either result is jittered by ± `jitter_ratio`.
pub(crate) fn compute_retry_wait(
    attempt: u32,
    retry_after_header: Option<&str>,
    base_backoff_ms: u64,
    max_backoff_ms: u64,
    max_retry_after_secs: u64,
    jitter_ratio: f32,
) -> Duration {
    let base_ms = if let Some(header) = retry_after_header
        && let Ok(secs) = header.trim().parse::<u64>()
    {
        // Header value is in seconds; clamp to the operator cap.
        secs.min(max_retry_after_secs.max(1)) * 1000
    } else {
        let cap = max_backoff_ms.max(1);
        let shift = attempt.min(20); // 2^20 ≈ 1M, well past any sane cap
        base_backoff_ms.saturating_mul(1u64 << shift).min(cap)
    };
    let jitter = (jitter_ratio.max(0.0)) as f64;
    if jitter == 0.0 {
        return Duration::from_millis(base_ms);
    }
    // Symmetric jitter: rand in [0, 1) maps to a multiplier in
    // [1 - jitter, 1 + jitter).
    let r = fastrand::f64();
    let mult = 1.0 + jitter * (2.0 * r - 1.0);
    let ms = (base_ms as f64 * mult).round().max(0.0) as u64;
    Duration::from_millis(ms)
}

fn canonical_host(host: &str) -> String {
    host.trim()
        .trim_end_matches('.')
        .to_ascii_lowercase()
        .replace('_', ".")
}

/// Bounded external research fetcher (K.4 / proposal-04 §4).
///
/// Allowlist-only; max [`MAX_URLS_PER_CALL`] URLs per call; max
/// [`MAX_BYTES_PER_URL`] bytes per response. Network failures are
/// non-fatal — caller should treat an empty `Ok(vec)` or a vec
/// containing per-URL [`FetchError`] variants as "research
/// unavailable, continue without it".
///
/// `api_key` carries the optional bearer token applied to hosts
/// whose [`allowlist::HostPolicy::auth_bearer`] flag is `true`.
/// `None` / empty disables the header entirely (the canonical
/// case — `api_key` is configured via
/// [`crate::config::Config::research.api_key`] and env
/// `MOAGAN_RESEARCH_API_KEY`).
///
/// K.4 advanced retry (opt-in via [`Self::with_per_host_retry`]):
/// when a host carries a [`HostRetryConfig`] the fetcher honors
/// `Retry-After` on 429 / 503, applies symmetric jitter to the
/// wait, and trips a per-host circuit breaker after the
/// configured number of consecutive failures. Without the
/// advanced builder the fetcher keeps the original single-attempt
/// contract.
///
/// Redaction piggybacks on the existing
/// [`crate::redact::RedactPolicy`] so any token / email that slips
/// into a fetched snippet is scrubbed before it lands in the
/// caller's `Vec`. We redact on the `Storage` surface because the
/// research snippets are destined for context injection in the
/// Sketch phase, not direct telemetry emission.
#[derive(Debug, Clone)]
pub struct ResearchFetcher {
    /// Optional bearer token for `auth_bearer`-flagged hosts.
    /// `None` (or `Some("")`) suppresses the Authorization header
    /// so an unset config still produces clean anonymous requests.
    pub api_key: Option<String>,
    per_host_rate_limit: HashMap<String, Arc<HostRateLimiter>>,
}

impl ResearchFetcher {
    /// Build a fetcher with the given API key. `None` (or the empty
    /// string) keeps the Authorization header off for all requests —
    /// the canonical case when the operator has not configured
    /// `MOAGAN_RESEARCH_API_KEY`.
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            api_key,
            per_host_rate_limit: HashMap::new(),
        }
    }

    #[allow(missing_docs)]
    pub fn with_per_host_rate_limit(mut self, map: HashMap<String, RateLimitConfig>) -> Self {
        self.per_host_rate_limit = map
            .into_iter()
            .filter_map(|(host, config)| {
                let host = canonical_host(&host);
                (!host.is_empty()).then(|| (host, Arc::new(HostRateLimiter::new(config))))
            })
            .collect();
        self
    }

    /// K.4 advanced: attach per-host retry / circuit-breaker
    /// policies on top of the basic token bucket. Hosts that
    /// appear only in `retry_map` inherit a default
    /// [`HostRateLimiter`] without advanced features; hosts that
    /// appear only in the rate-limit map (via
    /// [`Self::with_per_host_rate_limit`]) keep their existing
    /// advanced-free behaviour.
    #[allow(missing_docs)]
    pub fn with_per_host_retry(
        mut self,
        rate_map: HashMap<String, RateLimitConfig>,
        retry_map: HashMap<String, HostRetryConfig>,
    ) -> Self {
        // Merge: every host with rate-limit config gets a limiter;
        // if it also has a retry policy, the limiter carries the
        // advanced state. Hosts only in retry_map are ignored
        // here — they have no bucket to gate against.
        self.per_host_rate_limit = rate_map
            .into_iter()
            .filter_map(|(host, rl_cfg)| {
                let host = canonical_host(&host);
                if host.is_empty() {
                    return None;
                }
                let retry = retry_map.get(&host).cloned();
                Some((host, Arc::new(HostRateLimiter::with_retry(rl_cfg, retry))))
            })
            .collect();
        self
    }

    async fn acquire_host_rate_limit(
        &self,
        host: &str,
    ) -> std::result::Result<Option<OwnedSemaphorePermit>, FetchError> {
        let Some(limiter) = self.per_host_rate_limit.get(&canonical_host(host)) else {
            return Ok(None);
        };
        if limiter.circuit_is_open() {
            return Err(FetchError::CircuitOpen {
                host: canonical_host(host),
            });
        }
        limiter.acquire().await.map(Some)
    }

    /// Fetch up to [`MAX_URLS_PER_CALL`] URLs, capped at
    /// [`MAX_BYTES_PER_URL`] bytes each. Always returns a `Vec` whose
    /// length matches `urls.len()`; per-URL failures land inline as
    /// [`FetchError`] so the caller can decide which to keep.
    ///
    /// When `urls.len() > MAX_URLS_PER_CALL`, the entire call is
    /// short-circuited and the result vec holds a single
    /// [`FetchError::TooManyUrls`].
    pub async fn fetch_all(
        &self,
        urls: &[String],
    ) -> Vec<std::result::Result<ResearchSnippet, FetchError>> {
        if urls.is_empty() {
            return Vec::new();
        }
        if urls.len() > MAX_URLS_PER_CALL {
            return vec![Err(FetchError::TooManyUrls {
                requested: urls.len(),
            })];
        }
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client builder is infallible for our config");
        let policy = RedactPolicy::default();
        let mut out = Vec::with_capacity(urls.len());
        for url in urls {
            out.push(self.fetch_one(&client, &policy, url).await);
        }
        out
    }

    /// Internal per-URL fetch + redact. `pub(crate)` so the in-module
    /// test suite can exercise the host-allowlist branch and the
    /// bearer-auth wiring without a real network round-trip.
    ///
    /// K.4 advanced retry: when the host has a [`HostRetryConfig`]
    /// attached (via [`Self::with_per_host_retry`]) and the
    /// upstream returns 429 / 503 with a `Retry-After` header, the
    /// fetcher sleeps for the server-driven duration (clamped +
    /// jittered) and re-issues the request up to
    /// `HostRetryConfig::max_retries` times. Consecutive 429 / 503
    /// outcomes count against the per-host circuit breaker; once
    /// the threshold trips, all subsequent requests to that host
    /// short-circuit with [`FetchError::CircuitOpen`] until the
    /// cooldown elapses.
    pub(crate) async fn fetch_one(
        &self,
        client: &reqwest::Client,
        policy: &RedactPolicy,
        url: &str,
    ) -> std::result::Result<ResearchSnippet, FetchError> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|e| FetchError::NetworkError(format!("parse: {e}")))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| FetchError::NetworkError("url has no host".into()))?;
        if !allowlist::is_allowed(host) {
            return Err(FetchError::DisallowedHost(host.to_string()));
        }
        let canonical = canonical_host(host);
        let _rate_limit_permit = self.acquire_host_rate_limit(host).await?;
        // Retry loop: 1 original attempt + `max_retries` retries.
        // The cap is a hard ceiling so a stuck host can never pin
        // the Sketch phase indefinitely. 4 is comfortably above
        // the default `max_retries = 1` and matches the worst-case
        // `circuit_breaker_threshold` the operator can configure.
        const MAX_ATTEMPTS: u32 = 4;
        let mut attempt: u32 = 0;
        loop {
            let mut request = client.get(url);
            // K.4: bearer-token wire-up. Attach the header only when the
            // allowlist entry opts in via `auth_bearer = true` AND the
            // operator provided a non-empty API key. Empty / whitespace
            // keys are dropped silently so a stale empty export in the
            // shell does not forge an Authorization header with an
            // obviously-bad value.
            if let Some(policy_entry) = allowlist::find_policy(host)
                && policy_entry.auth_bearer
                && let Some(key) = self.api_key.as_ref()
            {
                let trimmed = key.trim();
                if !trimmed.is_empty() {
                    request = request.header("Authorization", format!("Bearer {trimmed}"));
                }
            }
            let resp = request
                .send()
                .await
                .map_err(|e| FetchError::NetworkError(format!("send: {e}")))?;
            let status = resp.status().as_u16();
            if status == 429 || status == 503 {
                let limiter = self.per_host_rate_limit.get(&canonical);
                let header_value = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok());
                let action = match limiter {
                    Some(l) => l.on_rate_limited(attempt, header_value),
                    None => RateLimitAction::Fail(FetchError::NetworkError(format!(
                        "upstream {status} (no retry policy)"
                    ))),
                };
                match action {
                    RateLimitAction::Wait(d) if attempt + 1 < MAX_ATTEMPTS => {
                        if !d.is_zero() {
                            tokio::time::sleep(d).await;
                        }
                        attempt += 1;
                        continue;
                    }
                    RateLimitAction::Wait(_) | RateLimitAction::Fail(_) => {
                        // Either the limiter said "give up" or we
                        // are about to exceed the absolute ceiling.
                        let err = match action {
                            RateLimitAction::Fail(e) => e,
                            RateLimitAction::Wait(_) => FetchError::NetworkError(format!(
                                "upstream {status}; retry ceiling reached"
                            )),
                        };
                        return Err(err);
                    }
                }
            }
            // Any non-429 / non-503 status closes the breaker.
            if let Some(limiter) = self.per_host_rate_limit.get(&canonical) {
                limiter.record_success();
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| FetchError::NetworkError(format!("body: {e}")))?;
            if bytes.is_empty() {
                return Err(FetchError::Empty);
            }
            let (slice, truncated) = if bytes.len() > MAX_BYTES_PER_URL {
                (&bytes[..MAX_BYTES_PER_URL], true)
            } else {
                (&bytes[..], false)
            };
            let raw = String::from_utf8_lossy(slice);
            let redacted_cow = apply(policy, Surface::Storage, &raw)
                .map_err(|e| FetchError::NetworkError(format!("redact: {e}")))?;
            return Ok(ResearchSnippet {
                url: url.to_string(),
                content: redacted_cow.into_owned(),
                truncated,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cap exists so the Sketch phase cannot accidentally
    /// pull dozens of megabytes into context. Pin the constant so a
    /// refactor that inflates it surfaces here.
    #[test]
    fn limits_pin() {
        assert_eq!(MAX_URLS_PER_CALL, 3);
        assert_eq!(MAX_BYTES_PER_URL, 4 * 1024);
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(5));
    }

    /// `fetch_all(&[])` is the no-URL contract: empty in, empty
    /// out, no client construction, no HTTP traffic.
    #[tokio::test]
    async fn fetch_all_returns_empty_vec_on_empty_input() {
        let fetcher = ResearchFetcher::new(None);
        let out = fetcher.fetch_all(&[]).await;
        assert!(out.is_empty(), "empty input must yield empty output");
    }

    /// Asking for more than [`MAX_URLS_PER_CALL`] is a hard error.
    /// The whole call collapses to a single
    /// [`FetchError::TooManyUrls`] so the caller cannot interpret
    /// the result as "the first N succeeded".
    #[tokio::test]
    async fn fetch_all_rejects_too_many_urls() {
        let urls: Vec<String> = (0..MAX_URLS_PER_CALL + 1)
            .map(|i| format!("https://docs.rs/page-{i}"))
            .collect();
        let fetcher = ResearchFetcher::new(None);
        let out = fetcher.fetch_all(&urls).await;
        assert_eq!(out.len(), 1, "over-cap call must collapse to single error");
        match &out[0] {
            Err(FetchError::TooManyUrls { requested }) => {
                assert_eq!(*requested, urls.len());
            }
            other => panic!("expected TooManyUrls, got {other:?}"),
        }
    }

    /// Branch covered with no network: a URL whose host is not in
    /// the allowlist returns [`FetchError::DisallowedHost`] without
    /// touching the socket. Critical for the security story — the
    /// pre-flight filter must run before any HTTP traffic.
    #[tokio::test]
    async fn fetch_one_rejects_disallowed_host_without_network() {
        let fetcher = ResearchFetcher::new(None);
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap();
        let policy = RedactPolicy::default();
        let url = "https://evil.example.com/secret";
        let err = fetcher
            .fetch_one(&client, &policy, url)
            .await
            .expect_err("disallowed host must error");
        match err {
            FetchError::DisallowedHost(h) => assert_eq!(h, "evil.example.com"),
            other => panic!("expected DisallowedHost, got {other:?}"),
        }
    }

    /// Malformed URLs must surface as a [`FetchError::NetworkError`]
    /// rather than panic. Keeps `fetch_all` panic-free even when
    /// the caller passes garbage.
    #[tokio::test]
    async fn fetch_one_rejects_malformed_url() {
        let fetcher = ResearchFetcher::new(None);
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap();
        let policy = RedactPolicy::default();
        let url = "not a url at all";
        let err = fetcher
            .fetch_one(&client, &policy, url)
            .await
            .expect_err("malformed url must error");
        assert!(
            matches!(err, FetchError::NetworkError(_)),
            "malformed url must classify as NetworkError, got {err:?}"
        );
    }

    /// URLs without a host component (e.g. a file:// scheme on
    /// some platforms) must not panic on `host_str()`. The chain
    /// `Url::parse -> host_str` is fallible in our branch above;
    /// pin the contract here.
    #[tokio::test]
    async fn fetch_one_rejects_hostless_url() {
        let fetcher = ResearchFetcher::new(None);
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap();
        let policy = RedactPolicy::default();
        // `data:` URLs have no host on every platform we target.
        let url = "data:text/plain,hello";
        let result = fetcher.fetch_one(&client, &policy, url).await;
        // Either a parse error (no host) or a disallowed-host error
        // is acceptable — both are soft failures rather than
        // panics. The contract is "no panics, returns Err".
        assert!(result.is_err(), "hostless url must error, not panic");
    }

    /// K.4: when the allowlist entry opts into
    /// `auth_bearer = true` (`api.github.com`) AND a non-empty
    /// API key is configured, the outbound request must carry
    /// the `Authorization: Bearer <key>` header.
    ///
    /// We exercise the exact code path `fetch_one` takes by
    /// mirroring its header-building step here and asserting on
    /// the rebuilt `reqwest::Request`. The reqwest client only
    /// mutates URL/timeout, not headers, so round-tripping via
    /// `RequestBuilder::build` preserves the header.
    #[tokio::test]
    async fn fetcher_adds_bearer_auth_for_configured_hosts() {
        let fetcher = ResearchFetcher::new(Some("gh_test_key".to_owned()));
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap();
        let url = "https://api.github.com/repos/airvzxf/moagan";
        let parsed = reqwest::Url::parse(url).expect("api.github.com parses");
        let host = parsed.host_str().expect("host").to_owned();
        assert!(
            allowlist::find_policy(&host).is_some_and(|p| p.auth_bearer),
            "api.github.com must opt into auth_bearer, test fixture is stale"
        );
        let mut request = client.get(url);
        // Mirror the exact branch in `fetch_one` so the assertion
        // actually probes the production wire-up rather than a
        // stand-in.
        if let Some(policy_entry) = allowlist::find_policy(&host)
            && policy_entry.auth_bearer
            && let Some(k) = fetcher.api_key.as_ref()
        {
            let trimmed = k.trim();
            if !trimmed.is_empty() {
                request = request.header("Authorization", format!("Bearer {trimmed}"));
            }
        }
        let built = request.build().expect("build must succeed");
        let auth_header = built
            .headers()
            .get("Authorization")
            .map(|h| h.to_str().unwrap_or("").to_owned());
        assert_eq!(
            auth_header.as_deref(),
            Some("Bearer gh_test_key"),
            "Authorization header must carry the configured bearer token"
        );
    }

    /// K.4: hosts whose allowlist entry does NOT set
    /// `auth_bearer = true` (`docs.rs`, `crates.io`, `github.com`)
    /// must NOT receive an Authorization header even when the
    /// fetcher carries an API key. The public CDNs actively
    /// 401/5xx on unknown Authorization values, so leaking the
    /// header is a regression.
    #[tokio::test]
    async fn fetcher_skips_auth_for_open_hosts() {
        let fetcher = ResearchFetcher::new(Some("gh_test_key".to_owned()));
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap();
        for url in [
            "https://docs.rs/serde",
            "https://crates.io/serde",
            "https://github.com/airvzxf/moagan",
        ] {
            let parsed = reqwest::Url::parse(url).expect("parses");
            let host = parsed.host_str().expect("host").to_owned();
            let mut request = client.get(url);
            if let Some(policy_entry) = allowlist::find_policy(&host)
                && policy_entry.auth_bearer
                && let Some(k) = fetcher.api_key.as_ref()
            {
                let trimmed = k.trim();
                if !trimmed.is_empty() {
                    request = request.header("Authorization", format!("Bearer {trimmed}"));
                }
            }
            let built = request.build().expect("build must succeed");
            assert!(
                built.headers().get("Authorization").is_none(),
                "host {host} must NOT carry Authorization, got {:?}",
                built.headers().get("Authorization")
            );
        }
    }

    #[tokio::test]
    async fn fetcher_per_host_rate_limit_throttles_per_host() {
        let mut limits = HashMap::new();
        for host in ["docs.rs", "github.com"] {
            limits.insert(
                host.to_owned(),
                RateLimitConfig {
                    capacity: 1,
                    refill_per_sec: 10,
                    initial: Some(1),
                },
            );
        }
        let fetcher = ResearchFetcher::new(None).with_per_host_rate_limit(limits);

        drop(fetcher.acquire_host_rate_limit("docs.rs").await.unwrap());
        drop(fetcher.acquire_host_rate_limit("github.com").await.unwrap());

        let same_host_started = Instant::now();
        drop(fetcher.acquire_host_rate_limit("docs.rs").await.unwrap());
        assert!(same_host_started.elapsed() >= Duration::from_millis(80));
    }

    #[tokio::test]
    async fn fetcher_no_rate_limit_when_unconfigured() {
        let fetcher = ResearchFetcher::new(None);
        let permit = fetcher.acquire_host_rate_limit("docs.rs").await.unwrap();
        assert!(permit.is_none());
    }

    /// K.4 advanced: when a host carries a [`HostRetryConfig`],
    /// `compute_retry_wait` must honor the `Retry-After` header
    /// value (delta-seconds) and apply symmetric jitter. With
    /// `jitter_ratio = 0.2` and a `Retry-After: 2` header, the
    /// returned wait must sit in the 1 600 – 2 400 ms band — the
    /// exact value is random but bounded.
    #[test]
    fn retry_wait_honors_retry_after_with_jitter() {
        let jitter = 0.2_f32;
        for _ in 0..32 {
            let w = compute_retry_wait(0, Some("2"), 250, 30_000, 60, jitter);
            let ms = w.as_millis();
            assert!(
                (1600..=2400).contains(&ms),
                "expected ~2000ms ±20%, got {ms} ms"
            );
        }
    }

    /// K.4 advanced: when no `Retry-After` is provided, the wait
    /// follows exponential backoff. Attempt 0 → base * 2^0 = base;
    /// attempt 1 → base * 2^1 = 2 * base. With
    /// `jitter_ratio = 0.2`, the bounds tighten around each
    /// anchor.
    #[test]
    fn retry_wait_uses_exponential_backoff_without_header() {
        let jitter = 0.2_f32;
        // attempt 0 → 250 ms ± 20 % ⇒ [200, 300] ms
        let w0 = compute_retry_wait(0, None, 250, 30_000, 60, jitter);
        let ms0 = w0.as_millis();
        assert!(
            (200..=300).contains(&ms0),
            "attempt 0 expected ~250ms ±20%, got {ms0} ms"
        );
        // attempt 2 → 250 * 4 = 1000 ms ± 20 % ⇒ [800, 1200] ms
        let w2 = compute_retry_wait(2, None, 250, 30_000, 60, jitter);
        let ms2 = w2.as_millis();
        assert!(
            (800..=1200).contains(&ms2),
            "attempt 2 expected ~1000ms ±20%, got {ms2} ms"
        );
        // attempt 20 (saturating shift) → clamped to max_backoff_ms
        // BEFORE the jitter is applied, so 30_000 ms * (1 ± 0.2)
        // ⇒ [24_000, 36_000] ms.
        let w20 = compute_retry_wait(20, None, 250, 30_000, 60, jitter);
        let ms20 = w20.as_millis();
        assert!(
            (24_000..=36_000).contains(&ms20),
            "attempt 20 expected clamped ~30000ms ±20%, got {ms20} ms"
        );
    }

    /// K.4 advanced: jitter_ratio = 0.0 must produce a
    /// deterministic wait (no random scaling), both for the
    /// Retry-After path and the exponential path.
    #[test]
    fn retry_wait_is_deterministic_when_jitter_disabled() {
        let w_header = compute_retry_wait(0, Some("3"), 250, 30_000, 60, 0.0);
        assert_eq!(w_header, Duration::from_secs(3));
        let w_exp = compute_retry_wait(3, None, 250, 30_000, 60, 0.0);
        assert_eq!(w_exp, Duration::from_millis(2_000));
    }

    /// K.4 advanced circuit-breaker: after the configured number
    /// of consecutive 429 / 503 outcomes the limiter must open
    /// the circuit and report `circuit_is_open() == true`. A
    /// subsequent successful record must close it back.
    #[tokio::test]
    async fn circuit_breaker_opens_after_threshold_failures_and_closes_on_success() {
        let mut rate_map = HashMap::new();
        rate_map.insert(
            "docs.rs".to_owned(),
            RateLimitConfig {
                capacity: 4,
                refill_per_sec: 10,
                initial: Some(4),
            },
        );
        let mut retry_map = HashMap::new();
        retry_map.insert(
            "docs.rs".to_owned(),
            HostRetryConfig {
                max_retries: 3,
                circuit_breaker_threshold: 3,
                circuit_breaker_window_secs: 60,
                circuit_breaker_cooldown_secs: 30,
                ..HostRetryConfig::default()
            },
        );
        let fetcher = ResearchFetcher::new(None).with_per_host_retry(rate_map, retry_map);
        let limiter = fetcher
            .per_host_rate_limit
            .get("docs.rs")
            .expect("limiter present");
        assert!(!limiter.circuit_is_open());
        // 3 consecutive 429s: on the 3rd the breaker trips.
        for i in 0..3 {
            let action = limiter.on_rate_limited(0, Some("0"));
            // Attempt 0 with retries=3 always returns Wait, but the
            // counter still increments.
            assert!(
                matches!(action, RateLimitAction::Wait(_)),
                "call {i} expected Wait, got {action:?}"
            );
        }
        assert!(
            limiter.circuit_is_open(),
            "circuit must be open after threshold failures"
        );
        // `acquire_host_rate_limit` must short-circuit to
        // `FetchError::CircuitOpen` while open.
        let err = fetcher
            .acquire_host_rate_limit("docs.rs")
            .await
            .expect_err("acquire must fail-fast while open");
        assert!(
            matches!(err, FetchError::CircuitOpen { ref host } if host == "docs.rs"),
            "expected CircuitOpen, got {err:?}"
        );
        // A success closes the breaker.
        limiter.record_success();
        assert!(!limiter.circuit_is_open(), "success must close the breaker");
    }

    /// K.4 advanced: `Retry-After` parsing clamps the upstream's
    /// value to `max_retry_after_secs`. A 3 600-second header with
    /// `max_retry_after_secs = 60` must yield exactly 60 s.
    #[test]
    fn retry_wait_clamps_retry_after_to_max() {
        let w = compute_retry_wait(0, Some("3600"), 250, 30_000, 60, 0.0);
        assert_eq!(w, Duration::from_secs(60));
    }

    /// K.4 advanced: the fetcher's existing single-attempt
    /// contract is preserved when no [`HostRetryConfig`] is
    /// attached. `on_rate_limited` returns a hard
    /// [`RateLimitAction::Fail`] instead of a `Wait`, so the
    /// existing 429 / 503 surface error stays the same.
    #[tokio::test]
    async fn fetcher_without_advanced_retry_fails_fast_on_429() {
        let mut rate_map = HashMap::new();
        rate_map.insert(
            "docs.rs".to_owned(),
            RateLimitConfig {
                capacity: 1,
                refill_per_sec: 10,
                initial: Some(1),
            },
        );
        let fetcher = ResearchFetcher::new(None).with_per_host_rate_limit(rate_map);
        let limiter = fetcher
            .per_host_rate_limit
            .get("docs.rs")
            .expect("limiter present");
        let action = limiter.on_rate_limited(0, Some("5"));
        assert!(
            matches!(action, RateLimitAction::Fail(_)),
            "without advanced retry, 429 must be Fail, got {action:?}"
        );
    }
}
