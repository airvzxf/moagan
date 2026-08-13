//! D.21.5: `BudgetCascade` — when a `BudgetObserver` reports Soft
//! or Hard pressure, the pipeline can call [`cascade_reduce`] to
//! shrink the next-phase [`Cardinality`] without restarting the
//! run. The cascade is deterministic: every pressure level maps
//! to one reduction rule, and the result preserves the
//! `soft <= hard` invariant (Cardinality is constructed via
//! `Cardinality::new`, which `debug_assert!`s the order).
//!
//! Spec contract:
//!
//! - **Ok** → return `current` unchanged. No reduction when the
//!   budget is healthy; the pipeline runs the spec-baseline
//!   cardinality for the active mode.
//! - **Soft** → trim the upper bound. The new `hard` is the
//!   midpoint between `current.soft` and `current.hard`, so
//!   the soft target is preserved and the safety margin
//!   shrinks.
//! - **Hard** → halve both bounds. The new `soft` and `hard`
//!   are `current.soft / 2` and `current.hard / 2`. Pairs with
//!   `BudgetObserver::should_skip_optional` so the calling phase
//!   skips its optional work *and* shrinks the next round's
//!   proposal pool.
//!
//! The function is pure: it does not mutate the observer and
//! does not write to the DB. The pipeline records the cascade
//! via `BudgetObserver::record` separately so the audit log
//! (D.5.1) keeps a single source of truth.

use crate::phases::budget::{BudgetObserver, PressureLevel};
use crate::phases::cardinality::Cardinality;

/// Reduce `current` according to the observer's pressure level.
/// Pure function; never mutates the observer or the DB.
pub fn cascade_reduce(observer: &BudgetObserver, current: Cardinality) -> Cardinality {
    match observer.pressure() {
        Ok(PressureLevel::Hard) => Cardinality::new(current.soft / 2, current.hard / 2),
        Ok(PressureLevel::Soft) => {
            let trimmed_hard = current.soft + (current.hard - current.soft) / 2;
            Cardinality::new(current.soft, trimmed_hard)
        }
        Ok(PressureLevel::Ok) | Err(_) => current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::RunId;
    use crate::storage::sqlite::Db;

    /// Stage a budget row for a run. Mirrors the helper in
    /// `src/phases/budget.rs::tests` so the cascade test does
    /// not have to duplicate the seeding logic.
    fn fresh_db_with_budget(planned: u64, used: u64) -> (Db, RunId) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("cascade.sqlite");
        // Leak the tempdir so the DB survives the test body
        // — same trick the budget tests use.
        std::mem::forget(tmp);
        let db = Db::open(&path).expect("open db");
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "running", "0.4.0", None, None, None)
            .expect("register run");
        db.set_budget(run_id, planned).expect("set budget");
        if used > 0 {
            db.budget_record(run_id, "seed", used).expect("seed usage");
        }
        (db, run_id)
    }

    /// Ok pressure: the cascade is a no-op. The caller gets
    /// back exactly the cardinality they passed in. Pins the
    /// "healthy budget = no reduction" contract.
    #[test]
    fn cascade_reduce_returns_current_when_ok() {
        let (db, run_id) = fresh_db_with_budget(1000, 100);
        let observer = BudgetObserver::new(db, run_id);
        let current = Cardinality::new(4, 5);
        let result = cascade_reduce(&observer, current);
        assert_eq!(result, current);
    }

    /// Hard pressure: both bounds halve. Pin the spec
    /// exactly: `soft / 2` and `hard / 2`, integer division.
    /// `(4, 5)` halves to `(2, 2)`, `(10, 25)` halves to
    /// `(5, 12)`.
    #[test]
    fn cascade_reduce_halves_bounds_under_hard() {
        let (db, run_id) = fresh_db_with_budget(1000, 950);
        let observer = BudgetObserver::new(db, run_id);
        let current = Cardinality::new(10, 25);
        let result = cascade_reduce(&observer, current);
        assert_eq!(result, Cardinality::new(5, 12));

        // Small cardinality: (4, 5) halves to (2, 2). The
        // debug_assert in Cardinality::new requires
        // soft <= hard, which the halving preserves.
        let (db, run_id) = fresh_db_with_budget(1000, 999);
        let observer = BudgetObserver::new(db, run_id);
        let result = cascade_reduce(&observer, Cardinality::new(4, 5));
        assert_eq!(result, Cardinality::new(2, 2));
    }

    /// Soft pressure: trim the upper bound to the midpoint
    /// between `soft` and `hard`. The soft target stays
    /// put. `(4, 5)` → new hard = `4 + (5-4)/2 = 4`. `(10,
    /// 25)` → new hard = `10 + 15/2 = 17`. Pins the spec.
    #[test]
    fn cascade_reduce_trims_upper_bound_under_soft() {
        let (db, run_id) = fresh_db_with_budget(1000, 700);
        let observer = BudgetObserver::new(db, run_id);
        let current = Cardinality::new(10, 25);
        let result = cascade_reduce(&observer, current);
        assert_eq!(result, Cardinality::new(10, 17));

        // Degenerate (soft, hard) = (4, 5) → new hard = 4.
        // Cardinality::new(4, 4) preserves the soft <= hard
        // invariant.
        let (db, run_id) = fresh_db_with_budget(1000, 600);
        let observer = BudgetObserver::new(db, run_id);
        let result = cascade_reduce(&observer, Cardinality::new(4, 5));
        assert_eq!(result, Cardinality::new(4, 4));
    }
}
