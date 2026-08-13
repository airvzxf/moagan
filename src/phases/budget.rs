//! Budget observer with soft/hard pressure tiers.
//!
//! Reads planned vs used tokens per run via
//! [`crate::storage::sqlite::Db::budget_read`] and exposes a
//! three-state pressure verdict:
//!
//! - [`PressureLevel::Ok`] — usage below the soft threshold. All
//!   optional work runs as normal.
//! - [`PressureLevel::Soft`] — usage between soft and hard. A future
//!   observer policy could surface a warning or a telemetry event
//!   here; the current implementation is a no-op for Soft.
//! - [`PressureLevel::Hard`] — usage at or above the hard threshold.
//!   Combined with [`BudgetPolicy::Reduce`], the
//!   [`BudgetObserver::should_skip_optional`] predicate returns
//!   `true` so the calling phase can skip its optional work
//!   (rank-phase stability check, synthesize merge, judge-phase
//!   adversary pass) and conserve remaining tokens for the core
//!   pipeline.
//!
//! ## Soft / hard thresholds
//!
//! Defaults are 50% (soft) and 90% (hard). Both are constructor
//! fields so tests can exercise each pressure tier without
//! manipulating the run's actual token accounting — the observer
//! is just a thin predicate over `(planned, used)`.
//!
//! ## Policy
//!
//! [`BudgetPolicy::Warn`] is reserved for a future telemetry hook;
//! the current implementation is a no-op for Warn.
//! [`BudgetPolicy::Reduce`] (the default) flips
//! `should_skip_optional` to `true` under Hard pressure.
//!
//! ## No-DB safety
//!
//! The observer always carries a `Db`. A no-DB run (legacy
//! `Telemetry::noop` with no SQLite index) is constructed with
//! the same `Db` value, but every call to `pressure()` /
//! `should_skip_optional()` reads through it; a missing row
//! resolves to `(0, 0)` which the observer maps to
//! `PressureLevel::Ok` so the optional work always runs in
//! tests that do not stage the budget row.

use crate::error::Result;
use crate::ids::RunId;
use crate::storage::sqlite::Db;

/// Three-state pressure verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureLevel {
    /// Below the soft threshold. Run everything.
    Ok,
    /// Between soft and hard. Reserved for a future Warn policy.
    Soft,
    /// At or above the hard threshold. Optional work is skipped
    /// when the policy is `Reduce`.
    Hard,
}

/// What the observer should do when the pressure reaches a
/// particular tier. `Warn` is a deliberate future hook; the only
/// policy the calling phases consult today is `Reduce`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetPolicy {
    /// Surface a warning (telemetry-only). No phase skips.
    Warn,
    /// Skip optional work when pressure reaches `Hard`.
    Reduce,
}

/// Read-only observer over the per-run budget table.
pub struct BudgetObserver {
    /// SQLite handle. Cheap to clone via `Arc` inside `Db`.
    pub db: Db,
    /// Run this observer tracks.
    pub run_id: RunId,
    /// Soft pressure threshold in percent of `planned_tokens`.
    /// Defaults to 50 (i.e. 50%).
    pub soft_pct: u8,
    /// Hard pressure threshold in percent of `planned_tokens`.
    /// Defaults to 90.
    pub hard_pct: u8,
    /// What the observer should do at each tier.
    pub policy: BudgetPolicy,
}

impl BudgetObserver {
    /// Build an observer with the soft/hard defaults (50 / 90) and
    /// the `Reduce` policy.
    pub fn new(db: Db, run_id: RunId) -> Self {
        Self {
            db,
            run_id,
            soft_pct: 50,
            hard_pct: 90,
            policy: BudgetPolicy::Reduce,
        }
    }

    /// Read the pressure tier. A `planned_tokens == 0` budget
    /// (the default — "no plan configured") resolves to
    /// `Ok` regardless of usage so a run that never sets a
    /// budget is never artificially throttled.
    pub fn pressure(&self) -> Result<PressureLevel> {
        let (planned, used) = self.db.budget_read(self.run_id)?;
        if planned == 0 {
            return Ok(PressureLevel::Ok);
        }
        let pct = (used.saturating_mul(100)) / planned;
        let pct_u8 = pct.min(u64::from(u8::MAX)) as u8;
        if pct_u8 >= self.hard_pct {
            Ok(PressureLevel::Hard)
        } else if pct_u8 >= self.soft_pct {
            Ok(PressureLevel::Soft)
        } else {
            Ok(PressureLevel::Ok)
        }
    }

    /// `true` when the calling phase should skip its optional
    /// work. The predicate is `true` only when the pressure is
    /// `Hard` AND the policy is `Reduce` — any other
    /// combination keeps the optional work on.
    pub fn should_skip_optional(&self) -> Result<bool> {
        Ok(matches!(self.pressure()?, PressureLevel::Hard) && self.policy == BudgetPolicy::Reduce)
    }

    /// Append `tokens` to the run's `used_tokens` counter, tagged
    /// with the caller-supplied `phase`. Returns `Ok(())` on a
    /// pre-v011 database (the helper short-circuits to a no-op)
    /// so a legacy operator upgrading the binary mid-run does
    /// not see a synthetic write.
    pub fn record(&self, phase: &str, tokens: u64) -> Result<()> {
        self.db.budget_record(self.run_id, phase, tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::Db;

    fn fresh_db() -> Db {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("budget-meta.sqlite");
        // Leak the tempdir so the DB survives the test body —
        // mirrors the pattern in `src/storage/sqlite.rs::tests`.
        std::mem::forget(tmp);
        Db::open(&path).unwrap()
    }

    /// Helper: stamp a `planned_tokens` value into the row so the
    /// observer sees a non-zero budget. Mirrors the call the
    /// `moagan run --token-budget` flag will eventually make.
    fn seed_budget(db: &Db, run_id: RunId, planned: u64, used: u64) {
        // Register the run row first so the FK on
        // `budget_state.run_id` accepts the seed write. The
        // production `moagan run` driver does this long before
        // the budget observer is constructed; the test helper
        // mirrors that ordering.
        db.register_run(run_id, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        db.set_budget(run_id, planned).unwrap();
        if used > 0 {
            // Stage the `used_tokens` directly via a manual
            // budget_record so the test does not have to know
            // the exact phase name.
            db.budget_record(run_id, "seed", used).unwrap();
        }
    }

    #[test]
    fn budget_observer_pressure_ok_below_soft() {
        let db = fresh_db();
        let run_id = RunId::new();
        seed_budget(&db, run_id, 1000, 100);
        let obs = BudgetObserver::new(db, run_id);
        assert_eq!(obs.pressure().unwrap(), PressureLevel::Ok);
        assert!(!obs.should_skip_optional().unwrap());
    }

    #[test]
    fn budget_observer_pressure_soft_between_soft_and_hard() {
        let db = fresh_db();
        let run_id = RunId::new();
        seed_budget(&db, run_id, 1000, 700);
        let obs = BudgetObserver::new(db, run_id);
        assert_eq!(obs.pressure().unwrap(), PressureLevel::Soft);
        // Soft pressure is not enough to skip optional work.
        assert!(!obs.should_skip_optional().unwrap());
    }

    #[test]
    fn budget_observer_pressure_hard_above_hard() {
        let db = fresh_db();
        let run_id = RunId::new();
        seed_budget(&db, run_id, 1000, 950);
        let obs = BudgetObserver::new(db, run_id);
        assert_eq!(obs.pressure().unwrap(), PressureLevel::Hard);
        // Hard pressure alone is not enough — the policy must
        // also be `Reduce` (the default).
        assert!(obs.should_skip_optional().unwrap());
    }

    #[test]
    fn budget_observer_should_skip_optional_when_hard_and_reduce_policy() {
        let db = fresh_db();
        let run_id = RunId::new();
        seed_budget(&db, run_id, 1000, 950);
        let obs = BudgetObserver {
            policy: BudgetPolicy::Reduce,
            ..BudgetObserver::new(db, run_id)
        };
        assert!(obs.should_skip_optional().unwrap());
    }

    #[test]
    fn budget_observer_warn_policy_does_not_skip_under_hard() {
        let db = fresh_db();
        let run_id = RunId::new();
        seed_budget(&db, run_id, 1000, 999);
        let obs = BudgetObserver {
            policy: BudgetPolicy::Warn,
            ..BudgetObserver::new(db, run_id)
        };
        // Hard pressure is real, but the Warn policy keeps the
        // optional work on so a future telemetry hook is the
        // only signal.
        assert!(!obs.should_skip_optional().unwrap());
    }

    #[test]
    fn budget_observer_unlimited_plan_is_always_ok() {
        let db = fresh_db();
        let run_id = RunId::new();
        // `planned = 0` is the "unlimited" sentinel — the
        // observer never throttles a run that did not stage a
        // budget, even if `used` is non-zero.
        seed_budget(&db, run_id, 0, 1_000_000);
        let obs = BudgetObserver::new(db, run_id);
        assert_eq!(obs.pressure().unwrap(), PressureLevel::Ok);
        assert!(!obs.should_skip_optional().unwrap());
    }

    #[test]
    fn budget_observer_record_appends_used_tokens() {
        let db = fresh_db();
        let run_id = RunId::new();
        let obs = BudgetObserver::new(db.clone(), run_id);
        db.register_run(run_id, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        db.set_budget(run_id, 1000).unwrap();
        obs.record("rank", 100).unwrap();
        obs.record("synthesize", 200).unwrap();
        let (planned, used) = db.budget_read(run_id).unwrap();
        assert_eq!(planned, 1000);
        assert_eq!(used, 300);
    }
}
