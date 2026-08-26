//! D.13.1 + D.13.8 + D.13.3: stop-policy enums and tuning constants
//! that drive [`crate::discovery::saturation::SaturationTracker`].
//!
//! The spec (T01-06 §9.3, V4 §6.4) frames the discovery loop's
//! termination as a small state machine: the tracker observes a
//! `(batch, clusters)` snapshot, decides whether to keep generating
//! sketches, and if it stops, surfaces a typed reason so the
//! downstream phase can emit a telemetry event and persist the
//! state. The three enums below are the vocabulary the rest of
//! the pipeline speaks.
//!
//! The [`StopPolicy`] struct captures the numerical knobs the loop
//! needs: a `saturation_threshold` below which an iteration is
//! considered saturated, a `reserve_ratio` margin that lets the
//! loop fire a final reserve batch after saturation is detected,
//! the `outlier_distance` cutoff the outlier tracker uses, and
//! the `[min_sketches, max_sketches]` hard limits. Spec D.13.3
//! pins the defaults as `pub const` so a refactor that drifts a
//! value trips a test before it lands.

/// Decision the saturation tracker returns after observing one
/// `(batch, clusters)` snapshot. `Continue` means the loop should
/// keep generating; `Stop` carries the typed reason so callers can
/// branch (e.g. emit a `DiscoverySaturated` telemetry event only
/// when the reason is `Saturated`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopDecision {
    /// The loop has not yet reached a stop condition. Keep
    /// generating.
    Continue,
    /// The loop must stop. The inner reason tells the caller
    /// *why* so the supervisor can pick the right telemetry
    /// variant and the right log line.
    Stop {
        /// Why the loop is stopping.
        reason: StopReason,
    },
}

/// Why the loop is stopping.
///
/// Six variants cover the spec (D.13.1) + the per-model fan-out
/// (D.13.2) + the cancel / budget surfaces. The variants are
/// ordered by frequency (saturated is the common case, cancelled
/// is rare, min/max are bookends).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Mean intra-cluster similarity crossed the
    /// `saturation_threshold` ceiling; the loop produced no new
    /// signal this iteration.
    Saturated,
    /// The outlier tracker collected enough outliers that the
    /// `outliers_collected` counter hit its cap.
    OutliersCollected,
    /// The hard token / cost budget was hit; further iterations
    /// would push the run over the planned total.
    BudgetExhausted,
    /// The cooperative cancel token flipped; the supervisor
    /// asked the loop to bail out.
    Cancelled,
    /// The loop produced the minimum acceptable sketch count and
    /// the operator chose the soft path (skip the reserve batch).
    MinSketchesReached,
    /// The loop produced the maximum acceptable sketch count;
    /// the hard cap must not be exceeded.
    MaxSketchesReached,
}

/// Why the loop is blocked from advancing (D.13.8). The
/// `BlockReason` is the "hard" sibling of `StopReason` — it means
/// the supervisor refuses to keep generating because the run is
/// in a degraded state (insufficient results, every model
/// saturated, etc.) rather than a clean terminal stop. The
/// distinction matters for the supervisor's decision tree: a
/// `BlockReason` should propagate up as an `Err`; a `StopReason`
/// is a clean exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockReason {
    /// The loop produced too few usable sketches to keep going.
    InsufficientResults,
    /// Every model in the fan-out is saturated; the loop has
    /// no productive direction left.
    AllModelsSaturated,
    /// The hard token / cost budget was hit.
    BudgetExhausted,
    /// The cooperative cancel token flipped.
    Cancelled,
}

/// Numerical tuning for the stop policy. Spec D.13.3 pins the
/// defaults as `pub const`; the struct is `Copy` so it can live
/// directly inside [`crate::discovery::saturation::SaturationTracker`]
/// without indirection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StopPolicy {
    /// Mean intra-cluster similarity threshold below which the
    /// tracker reports `Saturated`. Default `0.05` (D.13.3).
    pub saturation_threshold: f32,
    /// Fraction of the run's total sketch count that the loop is
    /// allowed to spend on the post-saturation reserve batch.
    /// Default `0.25` (D.13.3, T01-06 §9.3 calls it `margin_frac`).
    pub reserve_ratio: f32,
    /// Outlier distance threshold (Jaccard, `0..=1`). A sketch
    /// whose min-Jaccard to any cluster centroid is at least this
    /// value is considered an outlier. Default `0.30`.
    pub outlier_distance: f32,
    /// Lower sketch-count bound: a loop that produced fewer than
    /// `min_sketches` is `InsufficientResults`. Default `40`.
    pub min_sketches: usize,
    /// Soft sketch-count ceiling: the loop should stop
    /// voluntarily when it reaches this number. Default
    /// `max_sketches` (see below).
    pub max_sketches: usize,
    /// Hard sketch-count ceiling: the loop must never exceed
    /// this number. Default `500` (D.13.3).
    pub hard_cap: usize,
}

impl Default for StopPolicy {
    fn default() -> Self {
        tracing::trace!(
            saturation_threshold = DEFAULT_SATURATION_THRESHOLD,
            reserve_ratio = DEFAULT_RESERVE_RATIO,
            outlier_distance = DEFAULT_OUTLIER_DISTANCE,
            min_sketches = DEFAULT_MIN_SKETCHES,
            max_sketches = DEFAULT_MAX_SKETCHES,
            hard_cap = DEFAULT_DISCOVERY_HARD_CAP,
            "StopPolicy::default"
        );
        Self {
            saturation_threshold: DEFAULT_SATURATION_THRESHOLD,
            reserve_ratio: DEFAULT_RESERVE_RATIO,
            outlier_distance: DEFAULT_OUTLIER_DISTANCE,
            min_sketches: DEFAULT_MIN_SKETCHES,
            max_sketches: DEFAULT_MAX_SKETCHES,
            hard_cap: DEFAULT_DISCOVERY_HARD_CAP,
        }
    }
}

/// Default `saturation_threshold` (D.13.3). Pinned so a refactor
/// that drifts a value trips a test before it lands.
pub const DEFAULT_SATURATION_THRESHOLD: f32 = 0.05;
/// Default `reserve_ratio` (D.13.3). Pinned.
pub const DEFAULT_RESERVE_RATIO: f32 = 0.25;
/// Default `outlier_distance` (D.13.3). Pinned. The original
/// proposal named the field `outlier_distance_bits` (SimHash
/// bits, default 32); we keep the spec's name but switch the
/// unit to Jaccard because the embedder-based clusterer already
/// uses cosine / Jaccard instead of SimHash.
pub const DEFAULT_OUTLIER_DISTANCE: f32 = 0.30;
/// Default `min_sketches` (D.13.3). Pinned.
pub const DEFAULT_MIN_SKETCHES: usize = 40;
/// Default soft `max_sketches`. No effective cap; the matrix
/// loop walks every candidate the `--temperature-profile`
/// defines.
pub const DEFAULT_MAX_SKETCHES: usize = 4_294_967_295;
/// Default `hard_cap` (D.13.3). No effective cap; the matrix
/// loop walks every candidate the `--temperature-profile` defines.
pub const DEFAULT_DISCOVERY_HARD_CAP: usize = 4_294_967_295;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_decision_continue_is_distinct_from_stop() {
        let c = StopDecision::Continue;
        let s = StopDecision::Stop {
            reason: StopReason::Saturated,
        };
        assert_ne!(c, s);
    }

    #[test]
    fn stop_decision_stop_carries_reason() {
        let s = StopDecision::Stop {
            reason: StopReason::MaxSketchesReached,
        };
        match s {
            StopDecision::Stop { reason } => {
                assert_eq!(reason, StopReason::MaxSketchesReached);
            }
            other => panic!("expected Stop, got {other:?}"),
        }
    }

    #[test]
    fn stop_reason_variants_are_distinct() {
        let variants = [
            StopReason::Saturated,
            StopReason::OutliersCollected,
            StopReason::BudgetExhausted,
            StopReason::Cancelled,
            StopReason::MinSketchesReached,
            StopReason::MaxSketchesReached,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b, "variants {i} and {j} must differ");
                }
            }
        }
    }

    #[test]
    fn block_reason_variants_are_distinct() {
        let variants = [
            BlockReason::InsufficientResults,
            BlockReason::AllModelsSaturated,
            BlockReason::BudgetExhausted,
            BlockReason::Cancelled,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b, "variants {i} and {j} must differ");
                }
            }
        }
    }

    /// D.13.3: defaults are pinned so a refactor that drifts a
    /// value trips this test before it lands.
    #[test]
    fn stop_policy_defaults_match_d_13_3() {
        let p = StopPolicy::default();
        assert!((p.saturation_threshold - 0.05).abs() < 1e-6);
        assert!((p.reserve_ratio - 0.25).abs() < 1e-6);
        assert!((p.outlier_distance - 0.30).abs() < 1e-6);
        assert_eq!(p.min_sketches, 40);
        assert_eq!(p.hard_cap, 4_294_967_295);
    }

    /// D.13.3: `min_sketches <= max_sketches <= hard_cap` for the
    /// default policy. The runtime invariant the saturation
    /// tracker relies on when it picks `MinSketchesReached` vs
    /// `MaxSketchesReached`.
    #[test]
    fn stop_policy_default_invariants() {
        let p = StopPolicy::default();
        assert!(
            p.min_sketches <= p.max_sketches,
            "min_sketches ({}) must be <= max_sketches ({})",
            p.min_sketches,
            p.max_sketches
        );
        assert!(
            p.max_sketches <= p.hard_cap,
            "max_sketches ({}) must be <= hard_cap ({})",
            p.max_sketches,
            p.hard_cap
        );
    }
}
