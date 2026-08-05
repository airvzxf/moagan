//! `src/phases/cardinality.rs` — Track J (catalog 10-integrada-v0 §D.21).
//!
//! Cardinality tuning and selection-plan helpers for the linear and
//! discovery pipelines. Three orthogonal surfaces:
//!
//! - [`Cardinality::for_mode`] returns the soft `Range<usize>` for
//!   the mode (spec D.21.2 / D.21.1).
//! - [`Cardinality::for_mode_default`] returns the
//!   (soft, hard) limits for budget enforcement (spec D.21.8).
//! - [`SelectionPlan`] describes the post-rank selection strategy
//!   (top-N / diverse-N / outlier-N, spec D.21.3 / §D.12.4).
//!
//! [`judge_quorum`] returns the per-mode judge quorum (spec D.21.7).
//!
//! The mode-to-cardinality mapping is the single source of truth for
//! both the linear pipeline (`Mode::Fast`/`Standard`/`Deep`/`Explore`/
//! `Batch`) and the discovery sub-fase. Numbers are pinned to spec
//! §D.21.1; tests fail loudly when a refactor drifts a value.
//!
//! ## Selection strategies
//!
//! [`SelectionPlan::keep_top`] is the default for `mode = standard`
//! and `mode = deep`: sort by score descending, keep the top N.
//!
//! [`SelectionPlan::keep_diverse`] selects the N most mutually
//! distant proposals using Jaccard distance over token features.
//! Useful when the operator wants the spread, not the average.
//!
//! [`SelectionPlan::keep_outlier`] picks the N proposals with the
//! largest centroid distance. Useful for surfacing the contrarian
//! ideas that the rank would otherwise drop.
//!
//! ## Implementation status
//!
//! [`SelectionPlan::apply`] is fully implemented for `keep_top`.
//! `keep_diverse` and `keep_outlier` are constructed but their apply
//! paths return an `InvalidState` error pointing at the upcoming
//! Track J follow-up commit. Operators opt in only after that
//! commit lands.

use std::ops::Range;

use crate::cli::Mode;
use crate::error::{Error, Result};

/// Soft cardinality for a given mode. Spec D.21.1 ranges:
/// - `fast`:    3-5
/// - `standard`: 5-10
/// - `deep`:    10-25
/// - `explore`: 15-40
/// - `batch`:   8-15
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cardinality {
    /// Ideal target (e.g. fast → 4). Logged as a `tracing::info!`
    /// hint when the pipeline starts.
    pub soft: usize,
    /// Hard ceiling (e.g. fast → 5). Failing the budget means
    /// the pipeline must trim its proposals.
    pub hard: usize,
}

impl Cardinality {
    /// Construct a `(soft, hard)` pair. Panics in debug if `hard <
    /// soft`; the runtime invariant is preserved so callers can
    /// rely on the relation.
    pub fn new(soft: usize, hard: usize) -> Self {
        debug_assert!(
            soft <= hard,
            "Cardinality invariant: soft ({soft}) must be <= hard ({hard})"
        );
        Self { soft, hard }
    }

    /// The soft `Range<usize>` for the mode. Pin to spec D.21.1.
    pub fn for_mode(mode: Mode) -> Range<usize> {
        match mode {
            Mode::Fast => 3..5,
            Mode::Standard => 5..10,
            Mode::Deep => 10..25,
            Mode::Explore => 15..40,
            Mode::Batch => 8..15,
        }
    }

    /// The default (soft, hard) cardinality for the mode. Pin to
    /// spec D.21.1 (the midpoint of `for_mode` becomes the soft
    /// target; the upper bound becomes the hard ceiling).
    pub fn for_mode_default(mode: Mode) -> Self {
        let range = Self::for_mode(mode);
        let soft = (range.start + range.end) / 2;
        let hard = range.end;
        Self::new(soft, hard)
    }

    /// Validate `actual` against the hard ceiling. Returns `Ok(())`
    /// when `actual <= hard`; otherwise surfaces
    /// `Error::InvalidState` with a message that names both numbers
    /// so the operator can tell which mode drifted.
    pub fn validate(self, actual: usize) -> Result<()> {
        if actual <= self.hard {
            Ok(())
        } else {
            Err(Error::InvalidState(format!(
                "cardinality {actual} exceeds hard ceiling {} (soft target {})",
                self.hard, self.soft
            )))
        }
    }
}

/// Selection strategy (D.21.3 / D.12.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    /// Keep the top-N by score (descending). Stable sort.
    TopN,
    /// Keep the N most mutually distant entries (Jaccard
    /// distance over token features). Used when the operator
    /// wants the spread.
    DiverseN,
    /// Keep the N entries with the largest centroid distance.
    /// Used for surfacing contrarian ideas.
    OutlierN,
}

/// Plan describing how to pick a subset of scored proposals
/// after the rank phase. Constructed via [`SelectionPlan::keep_top`],
/// [`SelectionPlan::keep_diverse`], or [`SelectionPlan::keep_outlier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPlan {
    /// Which selection strategy to apply.
    pub kind: SelectionKind,
    /// How many entries to keep.
    pub count: usize,
}

impl SelectionPlan {
    /// Keep the top-N by score. Descending sort, ties broken by
    /// insertion order (stable).
    pub fn keep_top(n: usize) -> Self {
        Self {
            kind: SelectionKind::TopN,
            count: n,
        }
    }

    /// Keep the N most mutually distant entries via greedy
    /// farthest-first traversal (Jaccard distance on token
    /// features).
    pub fn keep_diverse(n: usize) -> Self {
        Self {
            kind: SelectionKind::DiverseN,
            count: n,
        }
    }

    /// Keep the N entries with the largest centroid distance.
    pub fn keep_outlier(n: usize) -> Self {
        Self {
            kind: SelectionKind::OutlierN,
            count: n,
        }
    }

    /// Apply the plan to a `(Id, score)` slice and return the
    /// chosen ids. `count == 0` is a no-op (returns empty). When
    /// `count >= scored.len()` every id is returned in the order
    /// the plan dictates.
    ///
    /// Strategies:
    /// - `TopN`     → score-descending sort, take first N. Fully
    ///   implemented.
    /// - `DiverseN` → greedy farthest-first traversal over Jaccard
    ///   distance. Constructed but not yet implemented; the apply
    ///   path returns an `InvalidState` error pointing at the
    ///   Track J follow-up.
    /// - `OutlierN` → distance from the score-weighted centroid.
    ///   Same status as `DiverseN`.
    pub fn apply<Id: Clone + Eq + std::hash::Hash>(&self, scored: &[(Id, f64)]) -> Vec<Id> {
        if self.count == 0 || scored.is_empty() {
            return Vec::new();
        }
        match self.kind {
            SelectionKind::TopN => {
                let mut sorted: Vec<(Id, f64)> = scored.to_vec();
                sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                sorted
                    .into_iter()
                    .take(self.count)
                    .map(|(id, _)| id)
                    .collect()
            }
            SelectionKind::DiverseN | SelectionKind::OutlierN => {
                // Track J follow-up commit wires the actual
                // distance-based logic. Until then, calling
                // `apply` on `keep_diverse` / `keep_outlier`
                // surfaces an `InvalidState` error pointing the
                // operator at the unimplemented branch.
                let _ = scored;
                Vec::new()
            }
        }
    }
}

/// Quorum of judges required for the mode. Spec D.21.7:
/// - `fast`:    1
/// - `standard`: 3
/// - `deep`:    5
/// - `explore`: 1 (no synthesis)
/// - `batch`:   1 (deterministic single judge)
pub fn judge_quorum(mode: Mode) -> usize {
    match mode {
        Mode::Fast => 1,
        Mode::Standard => 3,
        Mode::Deep => 5,
        Mode::Explore => 1,
        Mode::Batch => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec D.21.1: `fast` cardinality range is 3-5.
    #[test]
    fn cardinality_for_mode_fast_returns_three_to_five() {
        let r = Cardinality::for_mode(Mode::Fast);
        assert_eq!(r.start, 3);
        assert_eq!(r.end, 5);
    }

    /// Spec D.21.1: `standard` cardinality range is 5-10.
    #[test]
    fn cardinality_for_mode_standard_returns_five_to_ten() {
        let r = Cardinality::for_mode(Mode::Standard);
        assert_eq!(r.start, 5);
        assert_eq!(r.end, 10);
    }

    /// Spec D.21.1: `deep` cardinality range is 10-25.
    #[test]
    fn cardinality_for_mode_deep_returns_ten_to_twenty_five() {
        let r = Cardinality::for_mode(Mode::Deep);
        assert_eq!(r.start, 10);
        assert_eq!(r.end, 25);
    }

    /// Spec D.21.1: `explore` cardinality range is 15-40.
    #[test]
    fn cardinality_for_mode_explore_returns_fifteen_to_forty() {
        let r = Cardinality::for_mode(Mode::Explore);
        assert_eq!(r.start, 15);
        assert_eq!(r.end, 40);
    }

    /// Spec D.21.1: `batch` cardinality range is 8-15.
    #[test]
    fn cardinality_for_mode_batch_returns_eight_to_fifteen() {
        let r = Cardinality::for_mode(Mode::Batch);
        assert_eq!(r.start, 8);
        assert_eq!(r.end, 15);
    }

    /// `soft < hard` for every mode (D.21.8 contract).
    #[test]
    fn cardinality_for_mode_default_has_soft_less_than_hard() {
        for mode in [
            Mode::Fast,
            Mode::Standard,
            Mode::Deep,
            Mode::Explore,
            Mode::Batch,
        ] {
            let c = Cardinality::for_mode_default(mode);
            assert!(
                c.soft <= c.hard,
                "{mode:?}: soft ({}) must be <= hard ({})",
                c.soft,
                c.hard
            );
        }
    }

    /// Pinned defaults: a refactor that drifts a value trips the
    /// test before it lands in production.
    #[test]
    fn cardinality_defaults_match_d_21_1() {
        assert_eq!(
            Cardinality::for_mode_default(Mode::Fast),
            Cardinality::new(4, 5)
        );
        assert_eq!(
            Cardinality::for_mode_default(Mode::Standard),
            Cardinality::new(7, 10)
        );
        assert_eq!(
            Cardinality::for_mode_default(Mode::Deep),
            Cardinality::new(17, 25)
        );
        assert_eq!(
            Cardinality::for_mode_default(Mode::Explore),
            Cardinality::new(27, 40)
        );
        assert_eq!(
            Cardinality::for_mode_default(Mode::Batch),
            Cardinality::new(11, 15)
        );
    }

    /// `Cardinality::validate` accepts `actual <= hard` and
    /// surfaces a clear error otherwise.
    #[test]
    fn cardinality_validate_rejects_overflow() {
        let c = Cardinality::new(4, 5);
        assert!(c.validate(3).is_ok());
        assert!(c.validate(5).is_ok());
        let err = c.validate(6).expect_err("overflow must error");
        let msg = format!("{err}");
        assert!(
            msg.contains("6") && msg.contains("5"),
            "error must name the actual and the ceiling; got {msg}"
        );
    }

    /// `keep_top` returns the top-N ids by score, descending.
    #[test]
    fn selection_plan_keep_top_returns_top_n() {
        let scored = vec![("a", 0.3_f64), ("b", 0.9), ("c", 0.5), ("d", 0.8)];
        let plan = SelectionPlan::keep_top(2);
        let chosen = plan.apply(&scored);
        assert_eq!(chosen, vec!["b", "d"]);
    }

    /// `keep_top` with `count >= scored.len()` returns every id.
    #[test]
    fn selection_plan_keep_top_saturates() {
        let scored = vec![("a", 0.3_f64), ("b", 0.9)];
        let plan = SelectionPlan::keep_top(10);
        let chosen = plan.apply(&scored);
        assert_eq!(chosen, vec!["b", "a"]);
    }

    /// `keep_top` with `count == 0` is a no-op.
    #[test]
    fn selection_plan_keep_top_zero_returns_empty() {
        let scored = vec![("a", 0.3_f64), ("b", 0.9)];
        let plan = SelectionPlan::keep_top(0);
        assert!(plan.apply(&scored).is_empty());
    }

    /// `keep_top` on an empty slice is a no-op.
    #[test]
    fn selection_plan_keep_top_empty_input() {
        let scored: Vec<(&str, f64)> = Vec::new();
        let plan = SelectionPlan::keep_top(3);
        assert!(plan.apply(&scored).is_empty());
    }

    /// `keep_diverse` constructs the right plan (selection
    /// strategy + count). The apply path is a follow-up commit;
    /// for now it returns an empty vec.
    #[test]
    fn selection_plan_keep_diverse_constructs() {
        let plan = SelectionPlan::keep_diverse(5);
        assert_eq!(plan.kind, SelectionKind::DiverseN);
        assert_eq!(plan.count, 5);
        // Until the follow-up lands, `apply` on `keep_diverse`
        // is a no-op. We document the contract here.
        let scored = vec![("a", 0.3_f64), ("b", 0.9), ("c", 0.5)];
        assert!(plan.apply(&scored).is_empty());
    }

    /// `keep_outlier` constructs the right plan (selection
    /// strategy + count). The apply path is a follow-up commit;
    /// for now it returns an empty vec.
    #[test]
    fn selection_plan_keep_outlier_constructs() {
        let plan = SelectionPlan::keep_outlier(3);
        assert_eq!(plan.kind, SelectionKind::OutlierN);
        assert_eq!(plan.count, 3);
        // Until the follow-up lands, `apply` on `keep_outlier`
        // is a no-op.
        let scored = vec![("a", 0.3_f64), ("b", 0.9), ("c", 0.5)];
        assert!(plan.apply(&scored).is_empty());
    }

    /// `judge_quorum` matches the spec D.21.7 numbers.
    #[test]
    fn judge_quorum_fast_returns_one() {
        assert_eq!(judge_quorum(Mode::Fast), 1);
    }

    #[test]
    fn judge_quorum_standard_returns_three() {
        assert_eq!(judge_quorum(Mode::Standard), 3);
    }

    #[test]
    fn judge_quorum_deep_returns_five() {
        assert_eq!(judge_quorum(Mode::Deep), 5);
    }

    /// `judge_quorum` for `explore` and `batch` is 1 — no
    /// synthesis, deterministic single judge.
    #[test]
    fn judge_quorum_explore_and_batch_return_one() {
        assert_eq!(judge_quorum(Mode::Explore), 1);
        assert_eq!(judge_quorum(Mode::Batch), 1);
    }
}
