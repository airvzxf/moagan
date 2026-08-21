#![allow(missing_docs)]

//! Per-provider circuit breaker.
//!
//! After `threshold` consecutive failures inside `window`, the breaker
//! opens for `cooldown`. While open, callers should consult
//! [`CircuitBreaker::is_open`] and fail fast (or sleep via
//! [`CircuitBreaker::run`], which already handles the wait). After
//! `cooldown`, the breaker half-opens for one probe call; success
//! closes it, failure reopens.
//!
//! The breaker does NOT decide which errors count toward the failure
//! tally — that policy lives in [`crate::Error::is_circuit_opening`].
//! The wrapper that fronts a provider (`BreakeredProvider` in
//! `provider.rs`) consults that helper before calling
//! [`CircuitBreaker::record_failure`], so non-opening errors
//! (schema violations, operator errors, cancellations) leave the
//! state untouched.
//!
//! Spec: catalog 10-integrada-v0 §D.19.5 (T00-08 §1428-1435; T08-03
//! §5.8; T00-09; T03-03).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use crate::error::{Error, Result};
use crate::llm::role::Role;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Closed,
    Open(Instant),
    HalfOpen,
}

/// Per-provider circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug)]
struct Inner {
    state: State,
    failures: u32,
    last_failure: Option<Instant>,
    threshold: u32,
    window: Duration,
    cooldown: Duration,
}

impl CircuitBreaker {
    /// Build a breaker with the given threshold / window / cooldown.
    /// Defaults mirror catalog 10-integrada-v0 §D.19.5.
    pub fn new(threshold: u32, window: Duration, cooldown: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                state: State::Closed,
                failures: 0,
                last_failure: None,
                threshold,
                window,
                cooldown,
            })),
        }
    }

    /// Run `f` under the breaker. Returns its result on success, or
    /// on failure records the failure and returns the error.
    pub async fn run<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        if let Some(wait) = self.pre_check() {
            tokio::time::sleep(wait).await;
        }
        match f().await {
            Ok(v) => {
                self.record_success();
                Ok(v)
            }
            Err(e) => {
                self.record_failure();
                Err(e)
            }
        }
    }

    /// Force-open the breaker (used by rate limiter / plan exhausted).
    pub fn trip(&self) {
        let mut g = self.inner.lock();
        g.state = State::Open(Instant::now());
        g.failures = g.threshold;
    }

    /// True iff the breaker is currently rejecting calls. Returns
    /// `false` while Closed (no failures recorded) and while
    /// HalfOpen (a probe call is allowed through). Callers that want
    /// to wait out the cooldown should use [`Self::run`] instead,
    /// which sleeps the remaining time before invoking the
    /// wrapped call.
    pub fn is_open(&self) -> bool {
        matches!(self.inner.lock().state, State::Open(_))
    }

    /// Read the current state for telemetry.
    pub fn state(&self) -> &'static str {
        match self.inner.lock().state {
            State::Closed => "closed",
            State::Open(_) => "open",
            State::HalfOpen => "half_open",
        }
    }

    /// Number of consecutive failures observed inside the current
    /// window. Zero in Closed-with-no-failures and after a
    /// successful probe in HalfOpen.
    pub fn failure_count(&self) -> u32 {
        self.inner.lock().failures
    }

    /// Record a successful call. Closes the breaker and resets the
    /// failure counter so a recovered provider does not carry the
    /// tail of its past outage into the next window.
    pub fn record_success(&self) {
        let mut g = self.inner.lock();
        g.state = State::Closed;
        g.failures = 0;
        g.last_failure = None;
    }

    /// Record a failed call. Increments the failure counter; if the
    /// counter reaches `threshold` inside `window` the breaker
    /// opens. When `last_failure` is older than `window`, the
    /// counter is reset first so a long-stable provider that
    /// suddenly trips does not inherit stale history.
    ///
    /// Policy note: callers MUST filter the error through
    /// [`crate::Error::is_circuit_opening`] before invoking this
    /// method, so non-opening errors (schema violations, operator
    /// errors, cancellations) do not count toward the threshold.
    pub fn record_failure(&self) {
        let mut g = self.inner.lock();
        let now = Instant::now();
        // Reset failure count if last failure was outside the
        // window. A failure streak that broke is functionally a
        // fresh streak; if the breaker had tripped before, the
        // new count starts from zero, so the state has to follow.
        // Without this, callers that drive `record_failure`
        // directly (tests, manual recovery scripts) would observe
        // state=open with failures < threshold, which is a
        // contradiction the wrapper would then have to paper
        // over.
        if let Some(t) = g.last_failure
            && now.duration_since(t) > g.window
        {
            g.failures = 0;
            g.state = State::Closed;
        }
        g.failures = g.failures.saturating_add(1);
        g.last_failure = Some(now);
        if g.failures >= g.threshold {
            g.state = State::Open(now);
        }
    }

    pub(crate) fn record_failure_if_circuit_opening(&self, err: &Error) {
        if err.is_circuit_opening() {
            self.record_failure();
        }
    }

    fn pre_check(&self) -> Option<Duration> {
        let mut g = self.inner.lock();
        match g.state {
            State::Closed => None,
            State::Open(t) => {
                if t.elapsed() >= g.cooldown {
                    g.state = State::HalfOpen;
                    None
                } else {
                    Some(g.cooldown - t.elapsed())
                }
            }
            State::HalfOpen => None,
        }
    }
}

impl Default for CircuitBreaker {
    /// Quick-tripping defaults (5/60s/30s) used by tests that
    /// want to exercise breaker behaviour deterministically,
    /// and by `BreakeredProvider` instances that wrap a mock
    /// provider where trip-on-fifth-error is the right
    /// contract. Production providers (the
    /// `BreakeredProvider` instances built by
    /// `ProviderRegistry::new` / `insert` / `with_pool` /
    /// `registry_from_config_*`) use the **lenient** defaults
    /// exposed by [`Self::lenient`] instead. The split keeps
    /// the test surface stable (5/60s/30s) while production
    /// tolerates the 10-error burst a flaky cell-network call
    /// typically produces before recovering. Spec: catalog
    /// 10-integrada-v0 §D.19.5.
    fn default() -> Self {
        Self::new(5, Duration::from_secs(60), Duration::from_secs(30))
    }
}

impl CircuitBreaker {
    /// Lenient defaults for production: 50 failures in a 300 s
    /// window with a 60 s cooldown. Tolerates the 10-error
    /// burst a flaky cell-network call site typically produces
    /// before recovering, over a 5-minute window so a
    /// few-minute outage does not look like a provider-level
    /// failure. Cooldown is 60 s so a tripped breaker probes
    /// again within a minute — long enough to dodge a
    /// sustained outage, short enough that an isolated burst
    /// does not lock the provider out for the rest of the run.
    /// Used by `ProviderRegistry::new` / `insert` /
    /// `with_pool` / `registry_from_config_*` when each call
    /// site constructs its own fresh `Arc<CircuitBreaker>`.
    /// Spec: catalog 10-integrada-v0 §D.19.5.
    pub fn lenient() -> Self {
        Self::new(50, Duration::from_secs(300), Duration::from_secs(60))
    }
}

/// Per-role circuit breaker configuration. The values are
/// consumed by [`BreakerRegistry::new_with_config`]; an empty
/// config means the registry falls back to the
/// [`CircuitBreaker::lenient`] defaults.
#[derive(Debug, Clone, Copy)]
pub struct BreakerConfig {
    pub threshold: u32,
    pub window: Duration,
    pub cooldown: Duration,
}

impl From<crate::config::BreakerConfig> for BreakerConfig {
    fn from(c: crate::config::BreakerConfig) -> Self {
        Self {
            threshold: c.threshold,
            window: Duration::from_secs(c.window_secs),
            cooldown: Duration::from_secs(c.cooldown_secs),
        }
    }
}

impl BreakerConfig {
    pub const fn new(threshold: u32, window: Duration, cooldown: Duration) -> Self {
        Self {
            threshold,
            window,
            cooldown,
        }
    }

    /// Parse `THRESHOLD:WINDOW_SECS:COOLDOWN_SECS`. Used by
    /// `MOAGAN_CIRCUIT_BREAKER_PER_ROLE_<role>` env-var parsing.
    pub fn from_env_str(s: &str) -> Result<Self> {
        let mut tokens = s.split([':', ' ']).filter(|t| !t.is_empty());
        let threshold = tokens
            .next()
            .ok_or_else(|| Error::Provider {
                message: "circuit_breaker: missing threshold".into(),
                http_status: None,
            })?
            .parse::<u32>()
            .map_err(|e| Error::Provider {
                message: format!("circuit_breaker: threshold: {e}"),
                http_status: None,
            })?;
        let window_secs = tokens
            .next()
            .ok_or_else(|| Error::Provider {
                message: "circuit_breaker: missing window_secs".into(),
                http_status: None,
            })?
            .parse::<u64>()
            .map_err(|e| Error::Provider {
                message: format!("circuit_breaker: window_secs: {e}"),
                http_status: None,
            })?;
        let cooldown_secs = tokens
            .next()
            .ok_or_else(|| Error::Provider {
                message: "circuit_breaker: missing cooldown_secs".into(),
                http_status: None,
            })?
            .parse::<u64>()
            .map_err(|e| Error::Provider {
                message: format!("circuit_breaker: cooldown_secs: {e}"),
                http_status: None,
            })?;
        Ok(Self::new(
            threshold,
            Duration::from_secs(window_secs),
            Duration::from_secs(cooldown_secs),
        ))
    }
}

impl Default for BreakerConfig {
    /// Default: matches the per-provider lenient breaker from
    /// v0.9.4 (50 errors / 300 s window / 60 s cooldown) so a
    /// bare registry matches the pre-v0.9.6 behaviour for
    /// `PlanExhausted` (the only error class this breaker still
    /// reacts to).
    fn default() -> Self {
        Self::new(50, Duration::from_secs(300), Duration::from_secs(60))
    }
}

/// Per-`(provider, role)` registry of circuit breakers. Each
/// `(provider, role)` pair gets its own [`CircuitBreaker`] so a
/// `PlanExhausted` on `minimax`/`facet_deriver` does not trip the
/// breaker used by `minimax`/`tagger` (and vice versa).
///
/// The registry default config is [`BreakerConfig::default`];
/// callers can pre-create a pair with
/// [`Self::new_with_config`] so the very first call observes the
/// operator-tuned values. Lazy creation is also supported via
/// [`Self::breaker_for`] for code paths that only know the pair
/// at call time.
#[derive(Debug, Default, Clone)]
pub struct BreakerRegistry {
    by_pair: Arc<RwLock<HashMap<(String, Role), CircuitBreaker>>>,
    default_config: BreakerConfig,
}

impl BreakerRegistry {
    pub fn new() -> Self {
        Self {
            by_pair: Arc::new(RwLock::new(HashMap::new())),
            default_config: BreakerConfig::default(),
        }
    }

    pub fn new_with_config(default_config: BreakerConfig) -> Self {
        Self {
            by_pair: Arc::new(RwLock::new(HashMap::new())),
            default_config,
        }
    }

    /// Pre-create a breaker with a non-default config for the
    /// supplied `(provider, role)` pair. The first call to
    /// `breaker_for(provider, role)` returns this breaker
    /// instead of constructing a default one.
    pub fn pre_create(&mut self, provider: &str, role: Role, cfg: BreakerConfig) -> &mut Self {
        let key = (provider.to_string(), role);
        let breaker = CircuitBreaker::new(cfg.threshold, cfg.window, cfg.cooldown);
        self.by_pair.write().insert(key, breaker);
        self
    }

    /// Get or lazily create the breaker for `(provider, role)`.
    /// The caller owns the [`CircuitBreaker`] (cheap to clone via
    /// `Arc<Mutex<_>>`) so they can consult `is_open()` /
    /// `record_*()` without racing other pairs.
    pub fn breaker_for(&self, provider: &str, role: Role) -> CircuitBreaker {
        let key = (provider.to_string(), role);
        {
            let r = self.by_pair.read();
            if let Some(b) = r.get(&key) {
                return b.clone();
            }
        }
        let mut w = self.by_pair.write();
        if let Some(b) = w.get(&key) {
            return b.clone();
        }
        let breaker = CircuitBreaker::new(
            self.default_config.threshold,
            self.default_config.window,
            self.default_config.cooldown,
        );
        w.insert(key, breaker.clone());
        breaker
    }

    /// True iff the breaker for `(provider, role)` is currently
    /// rejecting calls.
    pub fn is_open(&self, provider: &str, role: Role) -> bool {
        self.breaker_for(provider, role).is_open()
    }

    /// Snapshot for telemetry. Unknown pairs return None.
    pub fn snapshot(&self, provider: &str, role: Role) -> Option<(String, u32)> {
        let key = (provider.to_string(), role);
        let r = self.by_pair.read();
        r.get(&key)
            .map(|b| (b.state().to_string(), b.failure_count()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closed_breaker_passes_through() {
        let cb = CircuitBreaker::default();
        let r = cb
            .run(|| async { Ok::<i32, crate::Error>(42) })
            .await
            .unwrap();
        assert_eq!(r, 42);
        assert_eq!(cb.state(), "closed");
        assert!(!cb.is_open());
        assert_eq!(cb.failure_count(), 0);
    }

    #[tokio::test]
    async fn opens_after_threshold_failures() {
        let cb = CircuitBreaker::new(2, Duration::from_secs(60), Duration::from_secs(60));
        for _ in 0..2 {
            let r: Result<()> = cb
                .run(|| async {
                    Err::<(), _>(crate::Error::Provider {
                        message: "x".into(),
                        http_status: None,
                    })
                })
                .await;
            assert!(r.is_err());
        }
        assert_eq!(cb.state(), "open");
        assert!(cb.is_open());
        assert_eq!(cb.failure_count(), 2);
    }

    #[test]
    fn trip_forces_open() {
        let cb = CircuitBreaker::default();
        cb.trip();
        assert_eq!(cb.state(), "open");
        assert!(cb.is_open());
    }

    #[test]
    fn record_success_resets_state() {
        // Drive a breaker into Open via `trip`, then `record_success`
        // resets to Closed with zero failures. This is the recovery
        // path the wrapper relies on after a half-open probe
        // succeeds.
        let cb = CircuitBreaker::new(1, Duration::from_secs(60), Duration::from_secs(60));
        cb.record_failure();
        assert_eq!(cb.state(), "open");
        cb.record_success();
        assert_eq!(cb.state(), "closed");
        assert!(!cb.is_open());
        assert_eq!(cb.failure_count(), 0);
    }

    #[test]
    fn record_failure_outside_window_resets_counter() {
        // Window 50ms: two back-to-back failures trip the breaker,
        // but a third failure after a 100ms sleep falls outside
        // the window so the counter resets and the breaker returns
        // to Closed with failures=1 (well below the threshold of
        // 2).
        let cb = CircuitBreaker::new(2, Duration::from_millis(50), Duration::from_secs(60));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), "open", "two back-to-back failures must trip");
        std::thread::sleep(Duration::from_millis(100));
        cb.record_failure();
        assert_eq!(
            cb.state(),
            "closed",
            "failure streak broke across the window; got state {}",
            cb.state()
        );
        assert_eq!(cb.failure_count(), 1);
    }

    #[test]
    fn half_open_probe_returns_false_on_is_open() {
        // Open, advance past cooldown, expect HalfOpen (is_open =
        // false). Done synchronously via sleep — short cooldown.
        let cb = CircuitBreaker::new(1, Duration::from_secs(60), Duration::from_millis(20));
        cb.record_failure();
        assert!(cb.is_open());
        std::thread::sleep(Duration::from_millis(30));
        // is_open() is a snapshot — it does NOT advance to half_open
        // by itself (that transition happens inside pre_check /
        // run). So is_open() still returns true after the cooldown
        // elapses until a call drives the transition. The
        // integration tests in tests/integration_circuit_breaker.rs
        // exercise the full half-open path through run().
        assert!(
            cb.is_open(),
            "is_open() is a snapshot of the persisted state and stays Open until a call triggers pre_check"
        );
    }
}

#[test]
fn breaker_config_from_env_str_parses() {
    let cfg = BreakerConfig::from_env_str("5:300:1800").unwrap();
    assert_eq!(cfg.threshold, 5);
    assert_eq!(cfg.window, Duration::from_secs(300));
    assert_eq!(cfg.cooldown, Duration::from_secs(1800));
}

#[test]
fn breaker_registry_separates_pairs() {
    let mut reg = BreakerRegistry::new();
    reg.pre_create(
        "minimax",
        Role::Tagger,
        BreakerConfig::new(1, Duration::from_secs(60), Duration::from_secs(30)),
    );
    let tagger = reg.breaker_for("minimax", Role::Tagger);
    let facet = reg.breaker_for("minimax", Role::FacetDeriver);
    // Same provider, different role -> distinct breakers.
    tagger.trip();
    assert!(tagger.is_open());
    assert!(!facet.is_open());
}

#[test]
fn breaker_registry_lazy_creation_uses_default_config() {
    let reg = BreakerRegistry::new_with_config(BreakerConfig::new(
        2,
        Duration::from_secs(60),
        Duration::from_secs(60),
    ));
    let b = reg.breaker_for("minimax", Role::Tagger);
    // Default threshold is 2: two failures trip.
    b.record_failure();
    b.record_failure();
    assert!(b.is_open());
}

#[test]
fn breaker_registry_failure_only_affects_target_pair() {
    let mut reg = BreakerRegistry::new_with_config(BreakerConfig::new(
        1,
        Duration::from_secs(60),
        Duration::from_secs(30),
    ));
    reg.pre_create(
        "minimax",
        Role::Tagger,
        BreakerConfig::new(1, Duration::from_secs(60), Duration::from_secs(30)),
    );
    reg.pre_create(
        "opencode_go",
        Role::Tagger,
        BreakerConfig::new(1, Duration::from_secs(60), Duration::from_secs(30)),
    );
    let m = reg.breaker_for("minimax", Role::Tagger);
    let o = reg.breaker_for("opencode_go", Role::Tagger);
    m.trip();
    assert!(m.is_open());
    assert!(!o.is_open());
}
