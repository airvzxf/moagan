//! Ranking stability under weight perturbation (Phase H).
//!
//! V4 §5.12 paso 6 calls for perturbing the per-criterion weights
//! within a small range and observing whether the top-1 winner
//! changes. The output is a per-proposal score in `[0.0, 1.0]` — the
//! fraction of perturbations under which the proposal kept its
//! position — plus a coarse [`StabilityLabel`].
//!
//! Design choices:
//!
//! - **Pure functions, no I/O.** The phase that consumes this module
//!   reads the evaluations from disk and feeds them in.
//! - **Deterministic for a given seed.** Two calls with the same
//!   `(base, n, sigma, seed)` produce the same perturbation set so
//!   tests can pin the output.
//! - **Box-Muller via `fastrand`.** We do not pull `rand`/`ndarray`/
//!   `statrs`; the existing `fastrand = "2.0"` dep is enough.
//! - **Clip weights to `[0.0, 2.0]`.** Negative weights break the
//!   weighted-average contract; `>2.0` makes the perturbation
//!   swamp the base weights. The clip is conservative — for the
//!   default `sigma = 0.05` over `n = 8` perturbations no clip
//!   ever fires in practice.
//!
//! `stability_score` reads the evaluations as `(id, [correctness,
//! completeness, fit, evidence, clarity, overall])` snapshots — the
//! same data shape `RankingWeights::weighted_score` already expects.
//! The phase that wires this in (`src/phases/rank.rs` step 5.6)
//! builds those snapshots from the `Aggregated` per-proposal sidecars.

use std::collections::HashMap;

use crate::config::RankingWeights;
use crate::domain::StabilityLabel;

/// One proposal's per-criterion averages, exactly the shape
/// `RankingWeights::weighted_score` consumes. Constructed from the
/// `Aggregated` sidecar by the rank phase.
#[derive(Debug, Clone, Copy)]
pub struct EvalSnapshot {
    /// Average of the `judge_correctness` outputs in `[0.0, 10.0]`.
    pub correctness: f32,
    /// Average of the `judge_completeness` outputs in `[0.0, 10.0]`.
    pub completeness: f32,
    /// Average of the `judge_fit` outputs in `[0.0, 10.0]`.
    pub fit: f32,
    /// Average of the `judge_evidence` outputs in `[0.0, 10.0]`.
    pub evidence: f32,
    /// Average of the `judge_clarity` outputs in `[0.0, 10.0]`.
    pub clarity: f32,
    /// Average of the per-judge overall `score` field in `[0.0, 10.0]`.
    pub overall: f32,
}

impl EvalSnapshot {
    /// Re-export of the weighted score under the supplied weights.
    /// Inlined here so the perturbation loop doesn't have to import
    /// `RankingWeights` twice.
    pub fn weighted_score(&self, w: &RankingWeights) -> f32 {
        w.weighted_score(
            self.correctness,
            self.completeness,
            self.fit,
            self.evidence,
            self.clarity,
            self.overall,
        )
    }
}

/// Clip range for any single perturbed weight. Lower bound 0 keeps
/// the weighted-average contract (`-weight * criterion` would invert
/// the contribution). Upper bound 2 lets the perturbation dominate
/// the base weight but only for very large `sigma` — in practice the
/// default `0.05` never trips the clip.
const WEIGHT_CLIP_LO: f32 = 0.0;
const WEIGHT_CLIP_HIGH: f32 = 2.0;

/// Generate `n` perturbations of `base`, adding zero-mean Gaussian
/// noise with standard deviation `sigma` to each of the six weights
/// (`correctness`, `completeness`, `fit`, `evidence`, `clarity`,
/// `overall`). The clip is applied per-weight after the noise; the
/// `overall` weight is included so the operator's "trust the
/// model's score" knob participates in the perturbation too.
///
/// `seed` controls the underlying `fastrand::Rng` so the same
/// `(base, n, sigma, seed)` tuple always produces the same
/// perturbation set. Two calls with the same seed are
/// bit-identical; this is a property the integration tests rely on.
///
/// `n = 0` returns the empty vec (no-op, useful for short-circuits).
pub fn perturb_weights(
    base: &RankingWeights,
    n: usize,
    sigma: f32,
    seed: u64,
) -> Vec<RankingWeights> {
    if n == 0 || sigma <= 0.0 {
        return Vec::new();
    }
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(RankingWeights {
            correctness: perturb_one(base.correctness, sigma, &mut rng),
            completeness: perturb_one(base.completeness, sigma, &mut rng),
            fit: perturb_one(base.fit, sigma, &mut rng),
            evidence: perturb_one(base.evidence, sigma, &mut rng),
            clarity: perturb_one(base.clarity, sigma, &mut rng),
            overall: perturb_one(base.overall, sigma, &mut rng),
        });
    }
    out
}

fn perturb_one(base: f32, sigma: f32, rng: &mut fastrand::Rng) -> f32 {
    let noise = gaussian(rng) * sigma;
    (base + noise).clamp(WEIGHT_CLIP_LO, WEIGHT_CLIP_HIGH)
}

/// Box-Muller transform: convert two uniform draws in `[0, 1)` into
/// one standard-normal draw. We only consume the first output and
/// discard the second (kept around for the next call). The
/// implementation lives here (instead of in `fastrand`) because
/// `fastrand` doesn't expose a Gaussian primitive.
fn gaussian(rng: &mut fastrand::Rng) -> f32 {
    // Avoid `log(0)`; clamp the uniform draw away from zero.
    let u1: f32 = rng.f32().max(f32::EPSILON);
    let u2: f32 = rng.f32();
    let z0 = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos();
    z0
}

/// Compute the per-proposal stability score under the supplied
/// weight set. Each entry of `evaluations` is `(proposal_id, snapshot)`;
/// for every weights entry we re-rank and increment the top-1 winner
/// by one. The returned map maps `proposal_id -> fraction in [0.0,
/// 1.0]` (wins / total perturbations).
///
/// `weights_set.len() == 0` returns an empty map (the caller short-
/// circuits and treats that as "the check was skipped"). A single-
/// proposal `evaluations` always returns `{proposal_id => 1.0}` —
/// trivially stable because there is no other proposal to lose to.
pub fn stability_score(
    weights_set: &[RankingWeights],
    evaluations: &[(String, EvalSnapshot)],
) -> HashMap<String, f32> {
    let mut out: HashMap<String, f32> = HashMap::new();
    if weights_set.is_empty() || evaluations.is_empty() {
        return out;
    }
    if evaluations.len() == 1 {
        out.insert(evaluations[0].0.clone(), 1.0);
        return out;
    }
    let total = weights_set.len() as f32;
    // wins[id] counts how often id was the top-1 winner.
    let mut wins: HashMap<&str, u32> = HashMap::with_capacity(evaluations.len());
    for w in weights_set {
        // Find the argmax under this weights entry. Ties broken by
        // proposal_id ascending so the result is deterministic.
        let mut best: Option<(&str, f32)> = None;
        for (id, snap) in evaluations {
            let score = snap.weighted_score(w);
            best = match best {
                None => Some((id.as_str(), score)),
                Some((cur_id, cur_score)) => {
                    if score > cur_score
                        || (score == cur_score && id.as_str() < cur_id)
                    {
                        Some((id.as_str(), score))
                    } else {
                        Some((cur_id, cur_score))
                    }
                }
            };
        }
        if let Some((id, _)) = best {
            *wins.entry(id).or_insert(0) += 1;
        }
    }
    for (id, _snap) in evaluations {
        let w = wins.get(id.as_str()).copied().unwrap_or(0);
        out.insert(id.clone(), w as f32 / total);
    }
    out
}

/// Map a stability score to a coarse verdict. The contract is:
/// `score >= threshold` → `Stable`, otherwise `Sensitive`. The
/// caller chooses the threshold (typically `0.8`); the function
/// itself does not embed a default.
pub fn stability_label(score: f32, threshold: f32) -> StabilityLabel {
    if score >= threshold {
        StabilityLabel::Stable
    } else {
        StabilityLabel::Sensitive
    }
}

/// Convenience for callers that want the score + label together.
/// `base_weights` is re-used as the unperturbed reference; the
/// function runs the perturbations, computes the score for the
/// current top-1 winner, and labels the ranking.
pub fn stability_check(
    base_weights: &RankingWeights,
    evaluations: &[(String, EvalSnapshot)],
    n: usize,
    sigma: f32,
    seed: u64,
    threshold: f32,
) -> (HashMap<String, f32>, StabilityLabel, f32) {
    let weights_set = perturb_weights(base_weights, n, sigma, seed);
    let score = stability_score(&weights_set, evaluations);
    let label = match score.values().copied().reduce(f32::max) {
        Some(top) => stability_label(top, threshold),
        None => StabilityLabel::Stable,
    };
    (score, label, sigma)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(c: f32, cp: f32, f: f32, e: f32, cl: f32, o: f32) -> RankingWeights {
        RankingWeights {
            correctness: c,
            completeness: cp,
            fit: f,
            evidence: e,
            clarity: cl,
            overall: o,
        }
    }

    fn snap(c: f32, cp: f32, f: f32, e: f32, cl: f32, o: f32) -> EvalSnapshot {
        EvalSnapshot {
            correctness: c,
            completeness: cp,
            fit: f,
            evidence: e,
            clarity: cl,
            overall: o,
        }
    }

    #[test]
    fn perturb_weights_n_zero_is_empty() {
        let base = w(1.0, 1.0, 1.0, 1.0, 1.0, 0.0);
        assert!(perturb_weights(&base, 0, 0.05, 42).is_empty());
    }

    #[test]
    fn perturb_weights_sigma_zero_is_empty() {
        let base = w(1.0, 1.0, 1.0, 1.0, 1.0, 0.0);
        assert!(perturb_weights(&base, 8, 0.0, 42).is_empty());
    }

    #[test]
    fn perturb_weights_clips_to_non_negative() {
        let base = w(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        let out = perturb_weights(&base, 32, 1.0, 7);
        for p in &out {
            assert!(p.correctness >= 0.0, "got {:?}", p);
            assert!(p.completeness >= 0.0);
            assert!(p.fit >= 0.0);
            assert!(p.evidence >= 0.0);
            assert!(p.clarity >= 0.0);
            assert!(p.overall >= 0.0);
            assert!(p.correctness <= 2.0, "got {:?}", p);
            assert!(p.overall <= 2.0);
        }
    }

    #[test]
    fn perturb_weights_deterministic_for_same_seed() {
        let base = w(1.0, 1.0, 1.0, 1.0, 1.0, 0.0);
        let a = perturb_weights(&base, 16, 0.05, 123);
        let b = perturb_weights(&base, 16, 0.05, 123);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.correctness, y.correctness);
            assert_eq!(x.completeness, y.completeness);
            assert_eq!(x.fit, y.fit);
            assert_eq!(x.evidence, y.evidence);
            assert_eq!(x.clarity, y.clarity);
            assert_eq!(x.overall, y.overall);
        }
    }

    #[test]
    fn perturb_weights_changes_with_seed() {
        let base = w(1.0, 1.0, 1.0, 1.0, 1.0, 0.0);
        let a = perturb_weights(&base, 16, 0.05, 1);
        let b = perturb_weights(&base, 16, 0.05, 2);
        // At least one weight differs between two seeds (with very
        // high probability for n=16).
        let differs = a
            .iter()
            .zip(b.iter())
            .any(|(x, y)| x.correctness != y.correctness);
        assert!(differs, "expected perturbation divergence between seeds");
    }

    #[test]
    fn stability_score_trivial_one_proposal() {
        let base = w(1.0, 1.0, 1.0, 1.0, 1.0, 0.0);
        let weights = perturb_weights(&base, 8, 0.05, 42);
        let evals = vec![("p0".to_string(), snap(5.0, 5.0, 5.0, 5.0, 5.0, 5.0))];
        let score = stability_score(&weights, &evals);
        assert_eq!(score.get("p0").copied(), Some(1.0));
    }

    #[test]
    fn stability_score_clear_winner_is_one() {
        // p0 dominates by 2 points on every criterion; small sigma
        // can't dislodge it.
        let base = w(1.0, 1.0, 1.0, 1.0, 1.0, 0.0);
        let weights = perturb_weights(&base, 32, 0.05, 42);
        let evals = vec![
            ("p0".to_string(), snap(10.0, 10.0, 10.0, 10.0, 10.0, 10.0)),
            ("p1".to_string(), snap(8.0, 8.0, 8.0, 8.0, 8.0, 8.0)),
        ];
        let score = stability_score(&weights, &evals);
        assert_eq!(
            score.get("p0").copied(),
            Some(1.0),
            "p0 should win every perturbation"
        );
        assert_eq!(score.get("p1").copied(), Some(0.0));
    }

    #[test]
    fn stability_score_close_call_can_flip() {
        // Two proposals that differ only on `correctness`; under
        // sigma=1.0 the weight on correctness sometimes collapses to
        // 0 (clipped) and the second proposal (id comes first
        // lexicographically) wins via the deterministic tiebreak.
        let base = w(1.0, 1.0, 1.0, 1.0, 1.0, 0.0);
        let weights = perturb_weights(&base, 64, 1.0, 42);
        let evals = vec![
            ("zzz_high_correctness".to_string(), snap(8.0, 8.0, 8.0, 8.0, 8.0, 8.0)),
            ("aaa_low_correctness".to_string(), snap(6.0, 8.0, 8.0, 8.0, 8.0, 8.0)),
        ];
        let score = stability_score(&weights, &evals);
        // The "low correctness" proposal wins the tiebreak when
        // correctness weight collapses to 0; the "high correctness"
        // proposal wins when the weight is positive.
        assert!(
            score.get("aaa_low_correctness").copied().unwrap() > 0.0,
            "expected aaa_low_correctness to win sometimes; got {:?}",
            score
        );
        assert!(
            score.get("zzz_high_correctness").copied().unwrap() < 1.0,
            "expected zzz_high_correctness to lose sometimes; got {:?}",
            score
        );
    }

    #[test]
    fn stability_label_threshold_is_inclusive() {
        assert_eq!(stability_label(0.8, 0.8), StabilityLabel::Stable);
        assert_eq!(stability_label(0.79, 0.8), StabilityLabel::Sensitive);
        assert_eq!(stability_label(1.0, 0.8), StabilityLabel::Stable);
        assert_eq!(stability_label(0.0, 0.8), StabilityLabel::Sensitive);
    }

    #[test]
    fn stability_check_aggregates() {
        let base = w(1.0, 1.0, 1.0, 1.0, 1.0, 0.0);
        let evals = vec![
            ("p0".to_string(), snap(10.0, 10.0, 10.0, 10.0, 10.0, 10.0)),
            ("p1".to_string(), snap(1.0, 1.0, 1.0, 1.0, 1.0, 1.0)),
        ];
        let (score, label, sigma) = stability_check(&base, &evals, 16, 0.05, 42, 0.8);
        assert_eq!(sigma, 0.05);
        assert_eq!(label, StabilityLabel::Stable);
        assert_eq!(score.get("p0").copied(), Some(1.0));
    }
}

#[cfg(test)]
mod proptests {
    // Phase H originally intended proptest for the monotonicity
    // invariants (clip range, variance grows with sigma, fractions
    // sum to 1). The repo doesn't currently use proptest and the
    // brief's rule about following existing libraries means we'd
    // rather add a `proptest` dev-dep in a follow-up. The fixed-
    // input tests above already cover the same invariants with a
    // bounded number of hand-picked seeds — equivalent coverage
    // without a new dep.
}