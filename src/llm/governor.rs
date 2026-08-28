#![allow(missing_docs)]

//! Adaptive concurrency / backoff governor, per-`(provider, role)`.
//!
//! The throttle responds to transient HTTP 429s by reducing the
//! per-role in-flight concurrency (AIMD multiplicative decrease) and
//! increasing the pre-call backoff (exponential with jitter). When the
//! 429 stream stops, the governor slowly restores concurrency
//! (additive increase) and decays the backoff.
//!
//! The companion [`super::circuit_breaker::BreakerRegistry`] covers
//! the persistent-failure lane (`PlanExhausted`). Two tiers,
//! complementary: the throttle never opens the breaker; the breaker
//! only opens on persistent signals the throttle has been
//! absorbing.
//!
//! Spec: catalog 10-integrada-v0 §D.19.6 (token-bucket rate-limiter
//! baseline) extended by per-role AIMD for transient 429s.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fastrand;
use parking_lot::{Mutex, RwLock};

use crate::error::{Error, Result};
use crate::llm::role::Role;

impl From<crate::config::ThrottleConfig> for ThrottleConfig {
    fn from(c: crate::config::ThrottleConfig) -> Self {
        Self {
            initial_concurrency: c.initial_concurrency,
            max_concurrency: c.max_concurrency,
            initial_backoff_ms: c.initial_backoff_ms,
            max_backoff_ms: c.max_backoff_ms,
            additive_after_ms: c.additive_after_ms,
            jitter_ms: c.jitter_ms,
        }
    }
}

/// Configuration for a single [`ThrottleGovernor`]. The struct is
/// per-role (the CLI boundary constructs one per `Role` listed in
/// the operator's `[throttle_per_role]` table; the rest fall back
/// to a default-constructed governor with role-specific defaults).
#[derive(Debug, Clone)]
pub struct ThrottleConfig {
    /// Initial in-flight concurrency cap before any 429. Default 4.
    pub initial_concurrency: u32,
    /// Maximum in-flight concurrency cap after recovery. Default 16.
    pub max_concurrency: u32,
    /// First-429 backoff: when the throttle receives its initial
    /// transient 429 with `current_backoff_ms == 0`, the
    /// backoff jumps to this value. Subsequent 429s double it
    /// (capped at `max_backoff_ms`). Defaults to 500 ms — long
    /// enough to take pressure off upstream, short enough that
    /// the first sleep is unobtrusive.
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub additive_after_ms: u64,
    pub jitter_ms: u64,
}

impl ThrottleConfig {
    /// Conservative default suitable for the role profiles observed
    /// in production (`tagger` is the loudest, then `sketch`, then
    /// `facet_deriver` and `extractor`). Override via
    /// `[throttle_per_role]` in `~/.config/moagan/config.toml` or
    /// `MOAGAN_THROTTLE_PER_ROLE_<role>=...`.
    pub fn default_for_role(role: Role) -> Self {
        tracing::trace!(role = ?role, "ThrottleConfig::default_for_role");
        match role {
            Role::Tagger => Self::new(4, 16, 500, 30_000, 5_000, 500),
            Role::Sketch => Self::new(2, 8, 500, 30_000, 5_000, 500),
            Role::FacetDeriver | Role::Extractor => Self::new(2, 4, 500, 30_000, 5_000, 500),
            _ => Self::new(1, 2, 500, 30_000, 5_000, 500),
        }
    }

    pub fn new(
        initial_concurrency: u32,
        max_concurrency: u32,
        initial_backoff_ms: u64,
        max_backoff_ms: u64,
        additive_after_ms: u64,
        jitter_ms: u64,
    ) -> Self {
        let initial_concurrency = initial_concurrency.max(1);
        let max_concurrency = max_concurrency.max(initial_concurrency);
        // `initial_backoff_ms` is the floor after the first 429
        // (the value the backoff defaults to before the doubling
        // sequence kicks in); `max_backoff_ms` is the cap. Their
        // relationship is normally `initial ≤ max` but we do not
        // panic on inversion — the worst-case outcome is that the
        // very first 429 saturates at `max_backoff_ms`, which is
        // exactly what we want when the operator misconfigures.
        let initial_backoff_ms = initial_backoff_ms.min(max_backoff_ms);
        tracing::trace!(
            initial_concurrency,
            max_concurrency,
            initial_backoff_ms,
            max_backoff_ms,
            "ThrottleConfig::new"
        );
        Self {
            initial_concurrency,
            max_concurrency,
            initial_backoff_ms,
            max_backoff_ms,
            additive_after_ms,
            jitter_ms,
        }
    }

    /// Parse
    /// `INITIAL:MAX:INITIAL_BACKOFF:MAX_BACKOFF:ADDITIVE_AFTER:JITTER`.
    /// Whitespace-separated tokens also accepted. Used by
    /// `MOAGAN_THROTTLE_PER_ROLE_<role>` env-var parsing.
    pub fn from_env_str(s: &str) -> Result<Self> {
        let mut tokens = s.split([':', ' ']).filter(|t| !t.is_empty());
        let initial = tokens
            .next()
            .ok_or_else(|| Error::Provider {
                message: "throttle: missing initial_concurrency".into(),
                http_status: None,
            })?
            .parse::<u32>()
            .map_err(|e| Error::Provider {
                message: format!("throttle: initial_concurrency: {e}"),
                http_status: None,
            })?;
        let max = tokens
            .next()
            .ok_or_else(|| Error::Provider {
                message: "throttle: missing max_concurrency".into(),
                http_status: None,
            })?
            .parse::<u32>()
            .map_err(|e| Error::Provider {
                message: format!("throttle: max_concurrency: {e}"),
                http_status: None,
            })?;
        let initial_backoff = tokens
            .next()
            .ok_or_else(|| Error::Provider {
                message: "throttle: missing initial_backoff_ms".into(),
                http_status: None,
            })?
            .parse::<u64>()
            .map_err(|e| Error::Provider {
                message: format!("throttle: initial_backoff_ms: {e}"),
                http_status: None,
            })?;
        let max_backoff = tokens
            .next()
            .ok_or_else(|| Error::Provider {
                message: "throttle: missing max_backoff_ms".into(),
                http_status: None,
            })?
            .parse::<u64>()
            .map_err(|e| Error::Provider {
                message: format!("throttle: max_backoff_ms: {e}"),
                http_status: None,
            })?;
        let additive_after = tokens
            .next()
            .ok_or_else(|| Error::Provider {
                message: "throttle: missing additive_after_ms".into(),
                http_status: None,
            })?
            .parse::<u64>()
            .map_err(|e| Error::Provider {
                message: format!("throttle: additive_after_ms: {e}"),
                http_status: None,
            })?;
        let jitter = tokens
            .next()
            .ok_or_else(|| Error::Provider {
                message: "throttle: missing jitter_ms".into(),
                http_status: None,
            })?
            .parse::<u64>()
            .map_err(|e| Error::Provider {
                message: format!("throttle: jitter_ms: {e}"),
                http_status: None,
            })?;
        Ok(Self::new(
            initial,
            max,
            initial_backoff,
            max_backoff,
            additive_after,
            jitter,
        ))
    }
}

#[derive(Debug, Clone)]
pub struct GovernorSnapshot {
    pub current_concurrency: u32,
    pub current_backoff_ms: u64,
    pub consecutive_429s: u32,
    pub consecutive_ok: u32,
}

#[derive(Debug)]
struct State {
    current_concurrency: u32,
    current_backoff_ms: u64,
    consecutive_429s: u32,
    consecutive_ok: u32,
    last_429_at: Option<Instant>,
}

/// The per-`(provider, role)` adaptive governor. Wraps an internal
/// mutex-protected state (current concurrency cap, backoff
/// duration, success / 429 streaks) and exposes the AIMD
/// state-machine through [`Self::pre_call`], [`Self::on_success`],
/// and [`Self::on_transient_429`].
pub struct ThrottleGovernor {
    config: ThrottleConfig,
    state: Mutex<State>,
}

impl ThrottleGovernor {
    /// Build a fresh governor with the supplied config. The starting
    /// state is `initial_concurrency` and `0` ms backoff so the
    /// first call to `pre_call` returns immediately.
    pub fn new(config: ThrottleConfig) -> Self {
        tracing::debug!(
            initial_concurrency = config.initial_concurrency,
            max_concurrency = config.max_concurrency,
            "ThrottleGovernor: constructed"
        );
        let start = State {
            current_concurrency: config.initial_concurrency,
            current_backoff_ms: 0,
            consecutive_429s: 0,
            consecutive_ok: 0,
            last_429_at: None,
        };
        Self {
            config,
            state: Mutex::new(start),
        }
    }

    /// Block until the per-role in-flight semaphore would be free
    /// under the current `current_concurrency`. The implementation
    /// returns the duration slept (caller may log it).
    pub async fn pre_call(&self) -> Duration {
        let backoff_ms = {
            let g = self.state.lock();
            g.current_backoff_ms
        };
        if backoff_ms == 0 {
            return Duration::ZERO;
        }
        let jitter = if self.config.jitter_ms > 0 {
            fastrand::u64(0..=self.config.jitter_ms)
        } else {
            0
        };
        let total_ms = backoff_ms.saturating_add(jitter);
        let dur = Duration::from_millis(total_ms);
        tracing::trace!(
            backoff_ms,
            jitter,
            total_ms,
            "ThrottleGovernor::pre_call: sleeping"
        );
        tokio::time::sleep(dur).await;
        dur
    }

    /// Notify the governor that the upstream returned a transient
    /// 429 (the per-role throttle absorbs it; the breaker is NOT
    /// touched). When `Retry-After` is supplied and larger than the
    /// computed backoff, the governor lifts its backoff to that
    /// floor so the upstream's own hint is respected.
    pub fn on_transient_429(&self, retry_after: Option<Duration>) {
        let now = Instant::now();
        let mut g = self.state.lock();
        g.consecutive_429s = g.consecutive_429s.saturating_add(1);
        g.consecutive_ok = 0;
        g.last_429_at = Some(now);
        let before_concurrency = g.current_concurrency;
        let before_backoff = g.current_backoff_ms;
        // Multiplicative decrease (floor 1). Skip when initial == 1,
        // which is the floor case.
        g.current_concurrency = (g.current_concurrency / 2).max(1);
        // Exponential backoff with jitter applied at sleep time.
        // The first 429 lifts the backoff from 0 to
        // `initial_backoff_ms`; subsequent 429s double up to
        // `max_backoff_ms`. `Retry-After` from the upstream
        // response, when present and larger than the computed
        // value, takes precedence so the throttle honours the
        // server's own backoff hint.
        let doubled = if g.current_backoff_ms == 0 {
            self.config.initial_backoff_ms
        } else {
            g.current_backoff_ms.saturating_mul(2)
        };
        let next = doubled.min(self.config.max_backoff_ms);
        if let Some(r) = retry_after {
            let r_ms = r.as_millis() as u64;
            if r_ms > next {
                g.current_backoff_ms = r_ms.min(self.config.max_backoff_ms);
            } else {
                g.current_backoff_ms = next;
            }
        } else {
            g.current_backoff_ms = next;
        }
        tracing::info!(
            before_concurrency,
            after_concurrency = g.current_concurrency,
            before_backoff_ms = before_backoff,
            after_backoff_ms = g.current_backoff_ms,
            retry_after = retry_after.is_some(),
            "ThrottleGovernor::on_transient_429: applied"
        );
    }

    /// Notify the governor that a call succeeded. After
    /// `additive_after_ms` without a 429, the concurrency grows by
    /// 1 (additive increase, capped at `max_concurrency`). The
    /// backoff decays multiplicatively by 3/4 each successful call,
    /// so sustained success eventually drives it back to 0.
    pub fn on_success(&self) {
        let now = Instant::now();
        let mut g = self.state.lock();
        g.consecutive_429s = 0;
        g.consecutive_ok = g.consecutive_ok.saturating_add(1);
        // Decay backoff every success. The 3/4 ratio means a run of
        // ~10 successes brings a 30 s backoff down to < 1 s — a
        // reasonable recovery gradient that does not race upstream
        // (since the next 429 re-applies the same doubling).
        g.current_backoff_ms = g.current_backoff_ms.saturating_mul(3) / 4;
        // Concurrency recovery: only when the last 429 is at least
        // `additive_after_ms` in the past, to avoid racing the
        // failure stream. The check fires on every success so a
        // burst of 5 successes adds +5 concurrency back, capped at
        // max_concurrency. This is the AIMD additive-increase side
        // of the controller.
        if g.current_backoff_ms == 0 && g.current_concurrency < self.config.max_concurrency {
            let raise = match g.last_429_at {
                Some(t)
                    if now.duration_since(t)
                        >= Duration::from_millis(self.config.additive_after_ms) =>
                {
                    true
                }
                None => true,
                _ => false,
            };
            if raise {
                let before = g.current_concurrency;
                g.current_concurrency += 1;
                tracing::debug!(
                    before,
                    after = g.current_concurrency,
                    "ThrottleGovernor::on_success: concurrency raised"
                );
            }
        }
    }

    pub fn snapshot(&self) -> GovernorSnapshot {
        let g = self.state.lock();
        GovernorSnapshot {
            current_concurrency: g.current_concurrency,
            current_backoff_ms: g.current_backoff_ms,
            consecutive_429s: g.consecutive_429s,
            consecutive_ok: g.consecutive_ok,
        }
    }

    /// Per-call concurrency ceiling reported back to the semaphore
    /// layer. The default governor returns `initial_concurrency`
    /// (no throttling yet) and adapts as 429s arrive.
    pub fn effective_concurrency(&self) -> u32 {
        self.state.lock().current_concurrency
    }
}

/// Per-`(provider, role)` registry of [`ThrottleGovernor`]
/// instances. Built lazily — the first call to
/// [`Self::governor_for`] constructs a default-config governor
/// keyed by the supplied pair; subsequent calls return the same
/// `Arc<ThrottleGovernor>`. Cheap to clone (the `by_pair` field
/// is wrapped in an `Arc<RwLock<...>>`).
pub struct GovernorRegistry {
    by_pair: Arc<RwLock<PairMap>>,
    default_initial_concurrency: u32,
    default_max_concurrency: u32,
}

/// `HashMap<(String, Role), Arc<ThrottleGovernor>>` — the cell
/// type of the per-`(provider, role)` registry. Extracted as a
/// `type` alias to keep the `pub struct` signature within
/// `clippy::type_complexity`'s threshold.
type PairMap = HashMap<(String, Role), Arc<ThrottleGovernor>>;

impl std::fmt::Debug for GovernorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GovernorRegistry")
            .field("by_pair (live count)", &self.by_pair.read().len())
            .field(
                "default_initial_concurrency",
                &self.default_initial_concurrency,
            )
            .field("default_max_concurrency", &self.default_max_concurrency)
            .finish()
    }
}

impl Default for GovernorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for GovernorRegistry {
    fn clone(&self) -> Self {
        Self {
            by_pair: self.by_pair.clone(),
            default_initial_concurrency: self.default_initial_concurrency,
            default_max_concurrency: self.default_max_concurrency,
        }
    }
}

impl GovernorRegistry {
    pub fn new() -> Self {
        tracing::debug!(
            default_initial_concurrency = 2,
            default_max_concurrency = 8,
            "GovernorRegistry: constructed"
        );
        Self {
            by_pair: Arc::new(RwLock::new(HashMap::new())),
            default_initial_concurrency: 2,
            default_max_concurrency: 8,
        }
    }

    /// Lookup (or lazily create) the governor for `(provider, role)`.
    /// The default config is `ThrottleConfig::default_for_role(role)`,
    /// overridable via [`Self::with_config_for`]. The same
    /// `(provider, role)` always returns the same `Arc<ThrottleGovernor>`
    /// so concurrent callers observe consistent state.
    pub fn governor_for(&self, provider: &str, role: Role) -> Arc<ThrottleGovernor> {
        let key = (provider.to_string(), role);
        {
            let r = self.by_pair.read();
            if let Some(g) = r.get(&key) {
                return g.clone();
            }
        }
        let mut w = self.by_pair.write();
        if let Some(g) = w.get(&key) {
            return g.clone();
        }
        let cfg = ThrottleConfig::default_for_role(role);
        let gov = Arc::new(ThrottleGovernor::new(cfg));
        tracing::debug!(
            provider,
            role = ?role,
            "GovernorRegistry: governor_for lazily created"
        );
        w.insert(key, gov.clone());
        gov
    }

    /// Pre-build a governor with a non-default config so the very
    /// first call observes the operator-tuned values without waiting
    /// for the lazy-create path.
    pub fn with_config_for(
        &mut self,
        provider: &str,
        role: Role,
        cfg: ThrottleConfig,
    ) -> &mut Self {
        tracing::debug!(
            provider,
            role = ?role,
            initial_concurrency = cfg.initial_concurrency,
            "GovernorRegistry: with_config_for"
        );
        let key = (provider.to_string(), role);
        self.by_pair
            .write()
            .insert(key, Arc::new(ThrottleGovernor::new(cfg)));
        self
    }

    /// Snapshot for telemetry across all known (provider, role)
    /// pairs. Unknown pairs are skipped (lazy-created governors do
    /// not appear until the first call).
    pub fn snapshots(&self) -> Vec<((String, Role), GovernorSnapshot)> {
        let r = self.by_pair.read();
        let out: Vec<((String, Role), GovernorSnapshot)> =
            r.iter().map(|(k, g)| (k.clone(), g.snapshot())).collect();
        tracing::trace!(count = out.len(), "GovernorRegistry::snapshots");
        out
    }

    /// Iterate every (provider, role) pair the registry has built so
    /// the call-site can read telemetry without waiting for the
    /// next call to lazy-create it.
    pub fn pairs(&self) -> Vec<(String, Role)> {
        let r = self.by_pair.read();
        r.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ThrottleConfig {
        ThrottleConfig::new(4, 8, 500, 1000, 100, 0)
    }

    #[test]
    fn throttle_starts_at_initial_concurrency() {
        let gov = ThrottleGovernor::new(cfg());
        let s = gov.snapshot();
        assert_eq!(s.current_concurrency, 4);
        assert_eq!(s.current_backoff_ms, 0);
        assert_eq!(s.consecutive_429s, 0);
    }

    #[test]
    fn transient_429_halves_concurrency_and_doubles_backoff() {
        let gov = ThrottleGovernor::new(cfg());
        gov.on_transient_429(None);
        let s = gov.snapshot();
        assert_eq!(s.current_concurrency, 2);
        assert_eq!(s.current_backoff_ms, 500); // initial_backoff_ms=500
        assert_eq!(s.consecutive_429s, 1);

        // Second 429 doubles the backoff above the initial floor.
        gov.on_transient_429(None);
        let s = gov.snapshot();
        assert_eq!(s.current_concurrency, 1); // floor 1
        assert_eq!(s.current_backoff_ms, 1000); // 500*2
    }

    #[test]
    fn transient_429_clamped_at_initial_1() {
        // initial=1, max=4. Even after repeated 429s the current
        // concurrency cannot go below 1 — the AIMD floor.
        let gov = ThrottleGovernor::new(ThrottleConfig::new(1, 4, 500, 1000, 100, 0));
        for _ in 0..5 {
            gov.on_transient_429(None);
        }
        assert_eq!(gov.snapshot().current_concurrency, 1);
    }

    #[test]
    fn transient_429_with_retry_after_dominates_backoff() {
        let gov = ThrottleGovernor::new(cfg());
        gov.on_transient_429(Some(Duration::from_millis(800)));
        assert_eq!(gov.snapshot().current_backoff_ms, 800);
    }

    #[test]
    fn success_resets_consecutive_429s() {
        let gov = ThrottleGovernor::new(cfg());
        gov.on_transient_429(None);
        gov.on_transient_429(None);
        assert_eq!(gov.snapshot().consecutive_429s, 2);
        gov.on_success();
        assert_eq!(gov.snapshot().consecutive_429s, 0);
    }

    #[test]
    fn success_decays_backoff() {
        let gov = ThrottleGovernor::new(cfg());
        gov.on_transient_429(None);
        gov.on_transient_429(None);
        assert!(gov.snapshot().current_backoff_ms > 0);
        for _ in 0..30 {
            gov.on_success();
        }
        assert_eq!(gov.snapshot().current_backoff_ms, 0);
    }

    #[test]
    fn config_from_env_str_parses_colons() {
        let cfg = ThrottleConfig::from_env_str("4:16:500:30000:5000:500").unwrap();
        assert_eq!(cfg.initial_concurrency, 4);
        assert_eq!(cfg.max_concurrency, 16);
        assert_eq!(cfg.initial_backoff_ms, 500);
        assert_eq!(cfg.max_backoff_ms, 30_000);
        assert_eq!(cfg.additive_after_ms, 5_000);
        assert_eq!(cfg.jitter_ms, 500);
    }

    #[test]
    fn config_invalid_env_string_errors() {
        assert!(ThrottleConfig::from_env_str("garbage").is_err());
        assert!(ThrottleConfig::from_env_str("4:16:bad:30000:5000:500").is_err());
    }

    #[test]
    fn registry_returns_same_arc_for_same_pair() {
        let reg = GovernorRegistry::new();
        let a = reg.governor_for("minimax", Role::Tagger);
        let b = reg.governor_for("minimax", Role::Tagger);
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn registry_separates_pairs() {
        let reg = GovernorRegistry::new();
        let a = reg.governor_for("minimax", Role::Tagger);
        let b = reg.governor_for("minimax", Role::FacetDeriver);
        let c = reg.governor_for("opencode", Role::Tagger);
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
    }

    #[test]
    fn registry_with_config_for_pre_creates() {
        let mut reg = GovernorRegistry::new();
        reg.with_config_for(
            "minimax",
            Role::Tagger,
            ThrottleConfig::new(1, 1, 0, 0, 0, 0),
        );
        let gov = reg.governor_for("minimax", Role::Tagger);
        assert_eq!(gov.snapshot().current_concurrency, 1);
        // The pair shows up in snapshots without needing a call first.
        assert_eq!(reg.pairs().len(), 1);
    }
}
