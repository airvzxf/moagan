//! D.13.2 + D.13.7: extended saturation tracker.
//!
//! The tracker now exposes a typed `update(batch, clusters) -> StopDecision`
//! entry point that drives the discovery loop's termination
//! state machine. It also tracks `outliers_collected` and the
//! per-model saturation map, so the supervisor can branch on a
//! rich signal instead of a single `completed` counter.
//!
//! The semantics:
//!
//! 1. `completed` is the source-of-truth counter. `coverage()` is
//!    `completed / target` and `is_saturated()` is `coverage() >= 1.0`.
//! 2. `update(batch, clusters)` is the per-iteration hook. It
//!    observes the new batch, advances `outliers_collected` with
//!    the outlier count, refreshes the per-model saturation map,
//!    and returns either `Continue` (loop should keep going) or
//!    `Stop { reason }` (loop must terminate). The decision is
//!    fully deterministic so a regression test can drive it with
//!    hand-rolled fixtures and the same input always yields the
//!    same answer.
//! 3. The `StopPolicy` knob struct lives in `stop_policy.rs`; the
//!    tracker holds one and consults it on every `update`.
//!
//! Backward compatibility: the original `coverage()`,
//! `is_saturated()`, `new()`, and `from_state()` entry points
//! are preserved unchanged so the existing `discovery::coordinator`
//! tests still pass.

use std::collections::BTreeMap;

use crate::discovery::outlier::detect_outliers_with_threshold;
use crate::discovery::state::SketchLoopState;
use crate::discovery::stop_policy::{StopDecision, StopPolicy, StopReason};
use crate::domain::{Cluster, Sketch};

/// Tracks sketch completion, per-model saturation, and the
/// outlier count. Callers (`DiscoveryCoordinator::run`,
/// `DiscoverMatrixPhase::execute`) call
/// [`SaturationTracker::update`] after each batch and inspect
/// the returned [`StopDecision`] to drive the loop.
pub struct SaturationTracker {
    /// Number of sketches that have been completed.
    pub completed: usize,
    /// Planned total number of sketches for this run.
    pub target: usize,
    /// Number of outliers collected so far. Bumped by
    /// [`SaturationTracker::update`] when the outlier detector
    /// returns new ids.
    pub outliers_collected: usize,
    /// Last observed `aporate` (mean intra-cluster similarity
    /// delta). The spec (D.13.2) names the field `last_aporate`
    /// to flag it as a moving average; the tracker updates it
    /// on every `update` call so the supervisor can plot it.
    pub last_aporate: f32,
    /// Number of clusters observed in the latest `update` call.
    pub cluster_count: usize,
    /// Mean intra-cluster similarity score (D.13.2) computed
    /// across the latest `clusters` snapshot.
    pub mean_intra_cluster_similarity: f32,
    /// Per-model saturation flags. Models that produced
    /// sufficiently-similar batches are marked `true` so the
    /// supervisor can drop them from the fan-out.
    pub per_model_saturated: BTreeMap<String, bool>,
    /// Tuning knobs. See [`StopPolicy`].
    pub policy: StopPolicy,
}

impl SaturationTracker {
    /// Build a tracker with `completed = 0` and the given target.
    /// Uses [`StopPolicy::default`] for the tuning knobs.
    pub fn new(target: usize) -> Self {
        tracing::debug!(target, "SaturationTracker::new");
        Self::with_policy(target, StopPolicy::default())
    }

    /// Build a tracker with a custom [`StopPolicy`].
    pub fn with_policy(target: usize, policy: StopPolicy) -> Self {
        tracing::debug!(
            target,
            saturation_threshold = policy.saturation_threshold,
            reserve_ratio = policy.reserve_ratio,
            outlier_distance = policy.outlier_distance,
            min_sketches = policy.min_sketches,
            max_sketches = policy.max_sketches,
            hard_cap = policy.hard_cap,
            "SaturationTracker::with_policy"
        );
        Self {
            completed: 0,
            target,
            outliers_collected: 0,
            last_aporate: 0.0,
            cluster_count: 0,
            mean_intra_cluster_similarity: 0.0,
            per_model_saturated: BTreeMap::new(),
            policy,
        }
    }

    /// Snapshot a tracker from the current `SketchLoopState`. The
    /// state's `completed_sketches` vector is the source of truth.
    pub fn from_state(state: &SketchLoopState, target: usize) -> Self {
        tracing::debug!(
            target,
            completed = state.completed_sketches.len(),
            failed = state.failed_attempts,
            "SaturationTracker::from_state"
        );
        Self {
            completed: state.completed_sketches.len(),
            target,
            ..Self::new(target)
        }
    }

    /// Fraction in `[0.0, 1.0]`. A `target` of `0` reports full
    /// coverage to avoid division-by-zero traps in the caller's
    /// decision logic.
    pub fn coverage(&self) -> f32 {
        let v = if self.target == 0 {
            1.0
        } else {
            self.completed as f32 / self.target as f32
        };
        tracing::trace!(
            completed = self.completed,
            target = self.target,
            coverage = v,
            "coverage"
        );
        v
    }

    /// `true` when coverage has reached (or exceeded) 100%.
    pub fn is_saturated(&self) -> bool {
        let s = self.coverage() >= 1.0;
        tracing::trace!(saturated = s, "is_saturated");
        s
    }

    /// Per-model saturation query (D.13.2). Returns `false` for
    /// models the tracker has not seen.
    pub fn model_saturated(&self, model: &str) -> bool {
        let v = self
            .per_model_saturated
            .get(model)
            .copied()
            .unwrap_or(false);
        tracing::trace!(model = %model, saturated = v, "model_saturated");
        v
    }

    /// Record a sketch completion. Convenience helper so the
    /// coordinator does not have to reach into the tracker's
    /// fields directly. Mirrors `SketchLoopState::record_completion`.
    pub fn record_completion(&mut self) {
        self.completed += 1;
        tracing::trace!(
            completed = self.completed,
            target = self.target,
            "record_completion"
        );
    }

    /// Record a batch of sketch completions.
    pub fn record_completions(&mut self, count: usize) {
        self.completed = self.completed.saturating_add(count);
        tracing::trace!(
            count,
            completed = self.completed,
            target = self.target,
            "record_completions"
        );
    }

    /// Observe a `(batch, clusters)` snapshot and return the
    /// next stop decision. The method is the single entry point
    /// the loop calls after every batch; the contract is:
    ///
    /// - `outliers_collected` is incremented by the number of
    ///   new outlier ids (deduplicated against the tracker's
    ///   current count via the sketch ids in the batch).
    /// - `cluster_count` and `mean_intra_cluster_similarity` are
    ///   refreshed from the supplied `clusters`.
    /// - `per_model_saturated` is updated: a model that appears
    ///   in the batch with a similar thesis to its previous
    ///   appearance is marked saturated.
    /// - The returned `StopDecision` is fully deterministic
    ///   given the tracker's state + the supplied snapshot.
    ///
    /// The order of the decision branches follows the spec
    /// (D.13.1 / D.13.2):
    ///
    /// 1. `MaxSketchesReached` — `completed >= hard_cap`.
    /// 2. `Saturated` — mean intra-cluster similarity has
    ///    crossed `saturation_threshold` AND the loop has spent
    ///    the `reserve_ratio` margin.
    /// 3. `MaxSketchesReached` — soft `max_sketches` cap.
    /// 4. `OutliersCollected` — outliers hit their
    ///    `min_sketches / 2` soft cap (one outlier per two
    ///    regular sketches is the rule of thumb; the cap is a
    ///    safety net to avoid unbounded outlier growth).
    /// 5. `MinSketchesReached` — loop reached `min_sketches`
    ///    and the input batch is empty (a clean early exit).
    ///    v0.13.2 (PR #688) lowered the operator-facing per-cell
    ///    floor from 10 to 1, so a matrix with `--sketches-per-cell
    ///    1` and `cells = 40` hits this branch at the natural
    ///    end-of-matrix boundary. The behaviour is intentional and
    ///    pinned by
    ///    `min_sketches_reached_pins_spc_1_small_matrix_contract`
    ///    in this module's tests.
    /// 6. `Continue` — none of the above.
    pub fn update(&mut self, batch: &[Sketch], clusters: &[Cluster]) -> StopDecision {
        self.cluster_count = clusters.len();
        self.mean_intra_cluster_similarity = mean_intra_cluster_similarity(clusters);
        self.last_aporate = self.mean_intra_cluster_similarity;

        tracing::trace!(
            completed = self.completed,
            target = self.target,
            cluster_count = self.cluster_count,
            batch_len = batch.len(),
            intra_sim = self.mean_intra_cluster_similarity,
            outliers = self.outliers_collected,
            hard_cap = self.policy.hard_cap,
            max_sketches = self.policy.max_sketches,
            min_sketches = self.policy.min_sketches,
            saturation_threshold = self.policy.saturation_threshold,
            "saturation: update entry"
        );

        // Outlier accounting. The detector treats unclustered
        // sketches as outliers (outlier.rs:90), so during the
        // matrix loop — where `clusters` is intentionally empty
        // until the post-matrix phase runs — EVERY sketch would be
        // counted as an outlier. That accumulator exists to safety
        // net a clusterer that produced too many outliers; it is
        // meaningless when there are no clusters yet. Skip the
        // accumulation when the cluster list is empty so the
        // `outliers_cap = min_sketches / 2` floor actually means
        // "outliers relative to clusters", not "iteration count".
        // The clusterer is the only caller that passes a non-empty
        // `clusters` slice, so the gate is the right shape
        // regardless of which driver ends up calling `update`.
        if !clusters.is_empty() {
            let outliers =
                detect_outliers_with_threshold(batch, clusters, self.policy.outlier_distance);
            self.outliers_collected = self.outliers_collected.saturating_add(outliers.len());
        }

        // Per-model saturation: every model that appears in the
        // batch with a thesis identical (case-insensitive) to a
        // prior batch member is marked saturated. The check is
        // deliberately strict — the loop is allowed to mark a
        // model saturated only when the model keeps producing
        // the same text, which is the spec's "no new signal"
        // condition.
        for sketch in batch {
            let key = sketch.angle.clone();
            if key.is_empty() {
                continue;
            }
            let saturated = batch
                .iter()
                .filter(|other| other.angle == key)
                .any(|other| {
                    other.thesis.eq_ignore_ascii_case(&sketch.thesis) && other.id != sketch.id
                });
            if saturated {
                self.per_model_saturated.insert(key, true);
            }
        }

        if self.completed >= self.policy.hard_cap {
            tracing::debug!(
                completed = self.completed,
                hard_cap = self.policy.hard_cap,
                "saturation: stop candidate hit (hard_cap)"
            );
            return StopDecision::Stop {
                reason: StopReason::MaxSketchesReached,
            };
        }
        if self.mean_intra_cluster_similarity >= self.policy.saturation_threshold
            && reserve_spent(self.completed, self.target, self.policy.reserve_ratio)
        {
            tracing::debug!(
                completed = self.completed,
                intra_sim = self.mean_intra_cluster_similarity,
                threshold = self.policy.saturation_threshold,
                reserve = self.policy.reserve_ratio,
                "saturation: stop candidate hit (Saturated)"
            );
            return StopDecision::Stop {
                reason: StopReason::Saturated,
            };
        }
        if self.completed >= self.policy.max_sketches {
            tracing::debug!(
                completed = self.completed,
                max_sketches = self.policy.max_sketches,
                "saturation: stop candidate hit (max_sketches)"
            );
            return StopDecision::Stop {
                reason: StopReason::MaxSketchesReached,
            };
        }
        if self.outliers_collected >= outliers_cap(self.policy.min_sketches) {
            tracing::debug!(
                outliers = self.outliers_collected,
                cap = outliers_cap(self.policy.min_sketches),
                "saturation: stop candidate hit (OutliersCollected)"
            );
            return StopDecision::Stop {
                reason: StopReason::OutliersCollected,
            };
        }
        if self.completed >= self.policy.min_sketches && batch.is_empty() {
            tracing::debug!(
                completed = self.completed,
                min_sketches = self.policy.min_sketches,
                "saturation: stop candidate hit (MinSketchesReached)"
            );
            return StopDecision::Stop {
                reason: StopReason::MinSketchesReached,
            };
        }
        tracing::trace!(
            completed = self.completed,
            "saturation: no stop condition matched; Continue"
        );
        StopDecision::Continue
    }
}

/// Mean intra-cluster similarity. Returns `0.0` when the input
/// is empty so the caller's `update` path stays simple. The
/// helper averages the cluster's `cohesion` field (D.13.2:
/// "mean intra-cluster similarity"); a future refactor can swap
/// in a real pairwise mean without touching the call sites.
fn mean_intra_cluster_similarity(clusters: &[Cluster]) -> f32 {
    if clusters.is_empty() {
        tracing::trace!("mean_intra_cluster_similarity: empty");
        return 0.0;
    }
    let sum: f32 = clusters.iter().map(|c| c.cohesion).sum();
    let v = sum / clusters.len() as f32;
    tracing::trace!(
        clusters = clusters.len(),
        sum,
        mean = v,
        "mean_intra_cluster_similarity"
    );
    v
}

/// True when the loop has spent the `reserve_ratio` margin on
/// top of the saturation point. The spec (T01-06 §9.3 + the
/// v0.5 PR-19 verification "con --cardinality 100 y saturación
/// al 50%, el run termina con ~60 sketches") defines the
/// saturation point as 50% of the target. The reserve is
/// `reserve_ratio` of that saturation point, not of the full
/// target: the loop fires the saturation-point sketches plus
/// the reserve, then stops.
///
/// With `target=100, ratio=0.25` the cap is
/// `ceil(50 * 1.25) = 63`. With `completed=63` the reserve is
/// spent and the `Saturated` decision fires. With
/// `completed=50` the reserve is intact and the loop keeps
/// going. A `target` of `0` reports `true` to avoid a
/// divide-by-zero trap.
fn reserve_spent(completed: usize, target: usize, reserve_ratio: f32) -> bool {
    if target == 0 {
        tracing::trace!("reserve_spent: target=0 → spent");
        return true;
    }
    let saturation_point = target / 2;
    let cap = (saturation_point as f32 * (1.0 + reserve_ratio)).ceil() as usize;
    let spent = completed >= cap;
    tracing::trace!(
        completed,
        target,
        reserve_ratio,
        saturation_point,
        cap,
        spent,
        "reserve_spent"
    );
    spent
}

/// Outlier cap. Default `min_sketches / 2` — i.e. one outlier
/// per two regular sketches is the rule of thumb. The cap is a
/// safety net so an outlier-flooded run still terminates.
fn outliers_cap(min_sketches: usize) -> usize {
    let v = min_sketches / 2;
    tracing::trace!(min_sketches, cap = v, "outliers_cap");
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturation_tracker_zero_target_is_saturated() {
        let t = SaturationTracker::new(0);
        assert_eq!(t.coverage(), 1.0);
        assert!(t.is_saturated());
    }

    #[test]
    fn saturation_tracker_computes_coverage() {
        let t = SaturationTracker {
            completed: 3,
            target: 4,
            ..SaturationTracker::new(4)
        };
        assert!((t.coverage() - 0.75).abs() < 1e-6);
        assert!(!t.is_saturated());
    }

    #[test]
    fn saturation_tracker_from_state_reads_completed_count() {
        let mut state = SketchLoopState::new("default".to_string());
        state.record_completion("sk_0001".to_string());
        state.record_completion("sk_0002".to_string());
        let t = SaturationTracker::from_state(&state, 2);
        assert_eq!(t.completed, 2);
        assert!(t.is_saturated());
    }

    fn sketch(id: &str, thesis: &str, angle: &str) -> Sketch {
        Sketch {
            id: id.into(),
            thesis: thesis.into(),
            angle: angle.into(),
            ..Sketch::default()
        }
    }

    fn cluster_with_cohesion(
        id: &str,
        label: &str,
        members: Vec<String>,
        cohesion: f32,
    ) -> Cluster {
        Cluster {
            id: id.into(),
            label: label.into(),
            members,
            cohesion,
            ..Cluster::default()
        }
    }

    /// Spec D.13.1: an empty batch + fresh tracker returns
    /// `Continue` (no stop condition met).
    #[test]
    fn update_with_empty_batch_returns_continue() {
        let mut t = SaturationTracker::new(80);
        let decision = t.update(&[], &[]);
        assert_eq!(decision, StopDecision::Continue);
        assert_eq!(t.cluster_count, 0);
        assert_eq!(t.mean_intra_cluster_similarity, 0.0);
    }

    /// Spec D.13.1: hitting the `hard_cap` returns
    /// `Stop(MaxSketchesReached)`. The hard cap wins over
    /// every softer stop reason.
    #[test]
    fn update_hits_hard_cap_returns_stop() {
        let mut t = SaturationTracker::with_policy(
            80,
            StopPolicy {
                hard_cap: 5,
                outlier_distance: 0.3,
                ..StopPolicy::default()
            },
        );
        t.completed = 5;
        let decision = t.update(&[], &[]);
        assert_eq!(
            decision,
            StopDecision::Stop {
                reason: StopReason::MaxSketchesReached
            }
        );
    }

    /// Spec D.13.1: hitting the soft `max_sketches` returns
    /// `Stop(MaxSketchesReached)` (without tripping the hard
    /// cap).
    #[test]
    fn update_hits_soft_max_returns_stop() {
        let mut t = SaturationTracker::with_policy(
            80,
            StopPolicy {
                max_sketches: 5,
                hard_cap: 100,
                outlier_distance: 0.3,
                ..StopPolicy::default()
            },
        );
        t.completed = 5;
        let decision = t.update(&[], &[]);
        assert_eq!(
            decision,
            StopDecision::Stop {
                reason: StopReason::MaxSketchesReached
            }
        );
    }

    /// Spec D.13.1: a `MinSketchesReached` early exit fires
    /// when the tracker has hit `min_sketches` and the
    /// incoming batch is empty (the loop has no more work to
    /// do).
    #[test]
    fn update_min_sketches_reached_with_empty_batch() {
        let mut t = SaturationTracker::with_policy(
            80,
            StopPolicy {
                min_sketches: 4,
                max_sketches: 80,
                hard_cap: 500,
                outlier_distance: 0.3,
                ..StopPolicy::default()
            },
        );
        t.completed = 4;
        let decision = t.update(&[], &[]);
        assert_eq!(
            decision,
            StopDecision::Stop {
                reason: StopReason::MinSketchesReached
            }
        );
    }

    /// Spec D.13.1: when `min_sketches` is met but the batch
    /// is non-empty, the loop continues — there's still work
    /// to do. (The outlier cap may still fire if the batch is
    /// unclustered; we put the sketch into a cluster with a
    /// matching angle so the test focuses on the
    /// `min_sketches` decision, not the outlier cap.)
    #[test]
    fn update_min_sketches_reached_with_non_empty_batch_continues() {
        let mut t = SaturationTracker::with_policy(
            80,
            StopPolicy {
                min_sketches: 2,
                ..StopPolicy::default()
            },
        );
        t.completed = 2;
        let clusters = vec![cluster_with_cohesion(
            "c1",
            "alpha minimalist",
            vec!["sk_001".into()],
            0.0,
        )];
        let decision = t.update(&[sketch("sk_001", "alpha", "minimalist")], &clusters);
        assert_eq!(decision, StopDecision::Continue);
    }

    /// Spec D.13.2: `update` advances the outlier counter for
    /// every outlier in the batch. The unclustered sketches are
    /// outliers (per the detector's "not in any cluster" rule).
    /// The clustered sketch is NOT an outlier because its label
    /// shares the thesis tokens.
    #[test]
    fn update_counts_outliers() {
        let mut t = SaturationTracker::new(80);
        let batch = vec![
            sketch("sk_001", "alpha beta gamma delta", "minimalist"),
            sketch("sk_002", "zeta eta theta iota", "production-grade"),
        ];
        let clusters = vec![cluster_with_cohesion(
            "cluster_01",
            "alpha beta gamma delta",
            vec!["sk_001".into()],
            0.0,
        )];
        let decision = t.update(&batch, &clusters);
        assert_eq!(decision, StopDecision::Continue);
        assert_eq!(t.outliers_collected, 1);
        assert_eq!(t.cluster_count, 1);
    }

    /// Spec D.13.1: when the mean intra-cluster similarity
    /// crosses the saturation threshold AND the loop has spent
    /// its reserve_ratio margin, the tracker returns
    /// `Stop(Saturated)`. Below the threshold the decision
    /// stays `Continue`.
    #[test]
    fn update_saturated_after_reserve_spent() {
        // target=10, reserve_ratio=0.25 → saturation_point=5,
        // cap=ceil(5*1.25)=7. completed=7 (reserve spent) AND
        // mean_similarity=0.5 (above the 0.05 threshold) →
        // Saturated.
        let mut t = SaturationTracker::with_policy(
            10,
            StopPolicy {
                saturation_threshold: 0.05,
                reserve_ratio: 0.25,
                ..StopPolicy::default()
            },
        );
        t.completed = 7;
        let clusters = vec![cluster_with_cohesion("c1", "x", vec!["sk_001".into()], 0.5)];
        let decision = t.update(&[], &clusters);
        assert_eq!(
            decision,
            StopDecision::Stop {
                reason: StopReason::Saturated
            }
        );
    }

    /// Spec D.13.1: when mean intra-cluster similarity is
    /// below the threshold, the tracker keeps going even if
    /// the reserve is spent — there's still productive work
    /// to do.
    #[test]
    fn update_below_saturation_threshold_continues() {
        let mut t = SaturationTracker::with_policy(
            10,
            StopPolicy {
                saturation_threshold: 0.5,
                reserve_ratio: 0.25,
                ..StopPolicy::default()
            },
        );
        t.completed = 7;
        let clusters = vec![cluster_with_cohesion("c1", "x", vec!["sk_001".into()], 0.1)];
        let decision = t.update(&[], &clusters);
        assert_eq!(decision, StopDecision::Continue);
    }

    /// Spec D.13.2: per-model saturation. A model that
    /// produces two identical-thesis sketches in the same
    /// batch is marked saturated.
    #[test]
    fn update_marks_model_saturated_on_repeat_thesis() {
        let mut t = SaturationTracker::new(80);
        let batch = vec![
            sketch("sk_001", "alpha", "minimalist"),
            sketch("sk_002", "alpha", "minimalist"),
        ];
        t.update(&batch, &[]);
        assert!(
            t.model_saturated("minimalist"),
            "model 'minimalist' must be marked saturated after producing the same thesis twice"
        );
    }

    /// Spec D.13.1: outlier cap. When the outlier counter
    /// reaches `min_sketches / 2`, the tracker returns
    /// `Stop(OutliersCollected)`. Pins the safety net so a
    /// future refactor cannot silently grow the cap.
    ///
    /// Note: the matrix loop drives `update` with `clusters: &[]`
    /// — the cluster-aware guard inside `update` skips outlier
    /// accumulation in that case, so we model the post-matrix
    /// scenario here: clusters exist but none of them contain the
    /// sketches in `batch`. With `min_sketches = 4` the cap is
    /// `4 / 2 = 2`, so the four unclustered sketches trip it.
    #[test]
    fn update_outliers_collected_cap() {
        let mut t = SaturationTracker::with_policy(
            80,
            StopPolicy {
                min_sketches: 4,
                ..StopPolicy::default()
            },
        );
        t.completed = 4;
        // 4 unclustered sketches → 4 outliers. cap = 4 / 2 = 2.
        let batch = vec![
            sketch("sk_001", "alpha", "minimalist"),
            sketch("sk_002", "beta", "minimalist"),
            sketch("sk_003", "gamma", "minimalist"),
            sketch("sk_004", "delta", "minimalist"),
        ];
        let clusters = vec![cluster_with_cohesion(
            "c_unrelated",
            "off-topic",
            vec![],
            0.0,
        )];
        let decision = t.update(&batch, &clusters);
        assert_eq!(
            decision,
            StopDecision::Stop {
                reason: StopReason::OutliersCollected
            }
        );
    }

    /// PR-D1 contract pin: the coordinator drives the matrix loop
    /// with `clusters: &[]` (the clusterer runs in a post-matrix
    /// phase). The outlier detector classifies every unclustered
    /// sketch as an outlier, so without the cluster-aware guard
    /// the `outliers_collected` counter would grow once per sketch
    /// and trip `OutliersCollected` at `min_sketches / 2`, killing
    /// the loop prematurely. The guard inside `update` makes the
    /// counter inert while clusters are empty so the loop can
    /// complete its full fan-out. This test pins the contract so
    /// a future refactor that reverts the guard trips the test.
    #[test]
    fn update_does_not_count_outliers_when_clusters_empty() {
        let mut t = SaturationTracker::with_policy(
            2000,
            StopPolicy {
                min_sketches: 40,
                max_sketches: 2000,
                hard_cap: 2000,
                outlier_distance: 0.3,
                ..StopPolicy::default()
            },
        );
        // Drive 1680 single-sketch iterations with empty clusters,
        // mirroring the matrix loop under the operator's
        // `[7 temps × 3 replicas]` profile. The caller (the
        // coordinator) records the completion before consulting
        // `update`, so the test mirrors the same call order.
        for i in 0..1680 {
            let sketch = sketch(
                &format!("sk_{i:04}"),
                &format!("thesis {i} alpha beta gamma"),
                "minimalist",
            );
            t.record_completions(1);
            let decision = t.update(&[sketch], &[]);
            assert_eq!(
                decision,
                StopDecision::Continue,
                "matrix loop must not trip OutliersCollected at iteration {i}"
            );
        }
        assert_eq!(
            t.outliers_collected, 0,
            "outliers_collected must stay at 0 while clusters is empty"
        );
        assert_eq!(t.completed, 1680);
        // The loop's outer `if n >= total` check is what stops the
        // matrix loop, not the tracker. The tracker has not tripped
        // any stop because none of the post-matrix conditions apply.
    }

    /// PR-19 verification: with `--cardinality 100` and 50%
    /// saturation, the run should terminate well before the
    /// hard cap. The check is end-to-end on the tracker, not
    /// the full pipeline (the full integration lives in
    /// `tests/integration_pr19_stop_policy.rs`). The tracker
    /// behaviour is: saturation_point = 50, reserve = ceil(50 *
    /// 0.25) = 13, trip_point = 50 + 13 = 63. With
    /// completed=63 AND mean_similarity=0.5 (above the 0.5
    /// threshold) the next update flips the decision to
    /// `Stop(Saturated)`. The "~60 sketches" check from the
    /// PR-19 spec is verified at the integration level (the
    /// matrix phase trims `paths` to `tracker.completed`).
    #[test]
    fn pr19_verification_50_percent_saturation_stops() {
        let mut t = SaturationTracker::with_policy(
            100,
            StopPolicy {
                saturation_threshold: 0.5,
                reserve_ratio: 0.25,
                min_sketches: 40,
                max_sketches: 80,
                hard_cap: 500,
                outlier_distance: 0.3,
            },
        );
        t.completed = 63;
        let clusters = vec![cluster_with_cohesion("c1", "x", vec!["sk_001".into()], 0.5)];
        let decision = t.update(&[], &clusters);
        assert_eq!(
            decision,
            StopDecision::Stop {
                reason: StopReason::Saturated
            },
            "50% saturation + reserve_ratio must trip Saturated before the hard cap"
        );
    }

    /// Companion to the previous test: 50% saturation but the
    /// loop has not yet spent the reserve_ratio margin → still
    /// `Continue` (the loop has budget left to fire the
    /// reserve batch).
    #[test]
    fn saturation_with_reserve_left_continues() {
        let mut t = SaturationTracker::with_policy(
            100,
            StopPolicy {
                saturation_threshold: 0.5,
                reserve_ratio: 0.25,
                min_sketches: 40,
                max_sketches: 80,
                hard_cap: 500,
                outlier_distance: 0.3,
            },
        );
        t.completed = 50;
        let clusters = vec![cluster_with_cohesion("c1", "x", vec!["sk_001".into()], 0.5)];
        // saturation_point=50, cap=ceil(50*1.25)=63. 50 < 63,
        // so the loop still has reserve budget → Continue.
        // The non-empty batch keeps the `MinSketchesReached`
        // branch (which fires only on an empty batch) from
        // shadowing the saturation check.
        let batch = vec![sketch("sk_001", "alpha", "minimalist")];
        let decision = t.update(&batch, &clusters);
        assert_eq!(decision, StopDecision::Continue);
    }

    /// v0.13.2 floor (PR #688) regression pin: when the operator
    /// runs a small matrix with `--sketches-per-cell 1`, the
    /// saturation tracker's `min_sketches = 40` floor can be
    /// reached exactly when the matrix exhausts itself. The
    /// tracker must surface this as `MinSketchesReached` (a clean
    /// "we hit the floor with no work left" early exit), NOT
    /// `Continue`. A future refactor that loses the early-exit
    /// branch would silently let the matrix loop finish on its
    /// outer "exhausted matrix" check, hiding the contract.
    ///
    /// Setup mirrors the v0.13.2 default policy at the smallest
    /// matrix size that reaches the floor exactly: 40 cells × 1
    /// sketch per cell × 1 default profile slot = 40 total
    /// sketches, with `target = 40`. The loop completes the 40
    /// sketches (the matrix loop drives `update` with the
    /// sketch it just recorded, then a final empty batch on the
    /// natural end-of-matrix signal), so the tracker's `update`
    /// sees `completed = 40` and `batch = []`.
    #[test]
    fn min_sketches_reached_pins_spc_1_small_matrix_contract() {
        // The v0.13.2 floor of 1 (PR #688) lets operators pick
        // `sketches_per_cell = 1`. The smallest matrix that
        // reaches `DEFAULT_MIN_SKETCHES = 40` exactly is
        // 40 cells × 1 sketch per cell × 1 default profile
        // slot, giving `target = 40` and `min_sketches = 40`.
        let mut t = SaturationTracker::with_policy(40, StopPolicy::default());
        // Drive the loop: every iteration records one completion
        // and asks the tracker for a decision. The final
        // iteration is the "matrix is exhausted" signal — the
        // loop passes an empty batch to `update`.
        for i in 0..40 {
            t.record_completions(1);
            let sketch = sketch(
                &format!("sk_{i:02}"),
                &format!("thesis {i} alpha beta gamma"),
                "minimalist",
            );
            let decision = t.update(&[sketch], &[]);
            assert_eq!(
                decision,
                StopDecision::Continue,
                "matrix loop must continue while there is still work at iteration {i}"
            );
        }
        assert_eq!(t.completed, 40);
        // Final probe: batch is empty, completed (40) >= min_sketches
        // (40) → MinSketchesReached. This is the v0.13.2 contract:
        // the floor is reachable from spc=1, and the tracker
        // surfaces it as a clean stop reason.
        let decision = t.update(&[], &[]);
        assert_eq!(
            decision,
            StopDecision::Stop {
                reason: StopReason::MinSketchesReached
            },
            "spc=1 small matrix must trip MinSketchesReached once the matrix exhausts at the floor"
        );
    }
}
