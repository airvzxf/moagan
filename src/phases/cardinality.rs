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
//! All three selection strategies are deterministic and free of
//! LLM calls; they run entirely on the `(Id, score)` slice the rank
//! phase already produced.
//!
//! ### `keep_top(n)` — sort by score, take the first N
//!
//! The default for `Mode::Standard` / `Mode::Deep`. Sort the input
//! by `score` descending, take the first `n` ids. Stable so ties
//! preserve insertion order. Use when the operator wants the
//! "highest expected utility" subset.
//!
//! ### `keep_diverse(n)` — greedy farthest-first over Jaccard
//!
//! Maximise the spread. The first pick is the highest-scoring
//! entry; subsequent picks maximise the minimum Jaccard distance
//! to the already-chosen set. Jaccard distance runs on token
//! features derived from each id's `Debug` representation — cheap,
//! deterministic, and good enough for the operator-facing
//! diversification. Useful when the operator wants the spread,
//! not the average.
//!
//! ### `keep_outlier(n)` — largest distance from centroid
//!
//! Pick the N ids with the largest distance from the
//! score-weighted centroid (Jaccard space). Useful for surfacing
//! the contrarian ideas that the rank would otherwise drop.
//!
//! ## Examples
//!
//! ```ignore
//! use moagan::cli::Mode;
//! use moagan::phases::cardinality::{Cardinality, SelectionPlan, judge_quorum};
//!
//! // Cardinality table for the current mode.
//! let c = Cardinality::for_mode_default(Mode::Deep);
//! assert_eq!(c.soft, 17);
//! assert_eq!(c.hard, 25);
//!
//! // Quorum of judges required for the mode.
//! assert_eq!(judge_quorum(Mode::Deep), 5);
//!
//! // Selection plan: keep the top 3 by score.
//! let plan = SelectionPlan::keep_top(3);
//! let scored = vec![("p1", 0.7), ("p2", 0.9), ("p3", 0.5), ("p4", 0.8)];
//! let chosen = plan.apply(&scored);
//! assert_eq!(chosen, vec!["p2", "p4", "p1"]);
//! ```

use std::collections::HashSet;
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
    /// - `TopN`     → score-descending sort, take first N. Stable.
    /// - `DiverseN` → greedy farthest-first traversal over Jaccard
    ///   distance on token features. The first pick is the highest
    ///   scorer; subsequent picks maximise the minimum distance to
    ///   the already-chosen set.
    /// - `OutlierN` → distance from the score-weighted centroid in
    ///   Jaccard space; keep the N with the largest distance.
    pub fn apply<Id: Clone + Eq + std::fmt::Debug + std::hash::Hash>(
        &self,
        scored: &[(Id, f64)],
    ) -> Vec<Id> {
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
            SelectionKind::DiverseN => {
                // Greedy farthest-first traversal. The first pick
                // is the highest-scoring entry; each subsequent
                // pick maximises the minimum Jaccard distance to
                // the already-chosen set. Ties on min-distance
                // break by score descending so the highest scorer
                // wins.
                let mut sorted: Vec<(Id, f64)> = scored.to_vec();
                sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let n = self.count.min(sorted.len());
                let mut chosen: Vec<Id> = Vec::with_capacity(n);
                let mut chosen_features: Vec<HashSet<String>> = Vec::with_capacity(n);
                let mut remaining: Vec<(Id, f64)> = sorted;
                for _ in 0..n {
                    let mut best_idx = 0usize;
                    let mut best_min_dist = f64::NEG_INFINITY;
                    let mut best_score = f64::NEG_INFINITY;
                    for (idx, (id, score)) in remaining.iter().enumerate() {
                        let feats = token_features_for(id);
                        let min_dist = if chosen.is_empty() {
                            // First pick: any min_dist is a tie;
                            // break by score descending.
                            0.0
                        } else {
                            chosen_features
                                .iter()
                                .map(|c| jaccard_distance(&feats, c))
                                .fold(f64::INFINITY, f64::min)
                        };
                        if min_dist > best_min_dist
                            || (min_dist == best_min_dist && *score > best_score)
                        {
                            best_idx = idx;
                            best_min_dist = min_dist;
                            best_score = *score;
                        }
                        let _ = feats;
                    }
                    let (id, _) = remaining.remove(best_idx);
                    chosen_features.push(token_features_for(&id));
                    chosen.push(id);
                }
                chosen
            }
            SelectionKind::OutlierN => {
                // Distance from the score-weighted centroid in
                // Jaccard space; keep the N with the largest
                // distance. The centroid weights each token by
                // the sum of its proposals' normalised scores.
                let total: f64 = scored.iter().map(|(_, s)| *s).sum();
                let mut weights: std::collections::HashMap<String, f64> =
                    std::collections::HashMap::new();
                for (id, score) in scored {
                    let w = if total > 0.0 {
                        score / total
                    } else {
                        1.0 / scored.len() as f64
                    };
                    for tok in token_features_for(id) {
                        *weights.entry(tok).or_insert(0.0) += w;
                    }
                }
                let centroid: HashSet<String> = weights.keys().cloned().collect();
                let mut distances: Vec<(Id, f64)> = scored
                    .iter()
                    .map(|(id, _)| {
                        let feats = token_features_for(id);
                        let d = jaccard_distance(&feats, &centroid);
                        (id.clone(), d)
                    })
                    .collect();
                distances
                    .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                distances
                    .into_iter()
                    .take(self.count)
                    .map(|(id, _)| id)
                    .collect()
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

/// Quorum of judges required for the mode, with profile-level
/// overrides applied first. Spec D.6 / D.21.7: the per-mode
/// judge-quorum baseline lives in [`judge_quorum`]; when the
/// active domain profile (`Config.profile_judge_quorum_overrides`)
/// defines an entry for the mode, that value wins. Profiles that
/// don't override a mode fall through to the spec baseline.
///
/// This is the helper `cli::run::build_pipeline_for_mode` should
/// call so that `--profile <name>` actually changes the judge
/// panel size (the previous table hard-coded the numbers and
/// ignored the profile entirely).
pub fn judge_quorum_for_mode(mode: Mode, cfg: &crate::config::Config) -> usize {
    if let Some(v) = cfg.profile_judge_quorum_overrides.get(mode.as_str()) {
        return *v;
    }
    judge_quorum(mode)
}

/// Heuristic: derive a token-feature set from an id's
/// `Debug` representation. Cheap, deterministic, and good enough
/// for Jaccard-based distance. The caller can layer richer
/// features later (e.g. proposal text) without changing the
/// [`SelectionPlan::apply`] contract.
fn token_features_for<Id: std::fmt::Debug>(id: &Id) -> HashSet<String> {
    let raw = format!("{id:?}");
    raw.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// Jaccard distance between two token sets: `1 - |A ∩ B| / |A ∪ B|`.
/// Returns `1.0` for two empty sets (everything is maximally far
/// from nothing).
fn jaccard_distance(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        1.0
    } else {
        1.0 - (intersection as f64 / union as f64)
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
    /// strategy + count) and the `apply` path actually
    /// diversifies: the first pick is the highest scorer and
    /// subsequent picks maximise the min Jaccard distance.
    #[test]
    fn selection_plan_keep_diverse_constructs() {
        let plan = SelectionPlan::keep_diverse(5);
        assert_eq!(plan.kind, SelectionKind::DiverseN);
        assert_eq!(plan.count, 5);
        // Apply on a 3-entry slice: the apply path caps the
        // pick count at `min(count, scored.len())` = 3 so every
        // id ends up chosen. First pick is the highest scorer;
        // subsequent picks break ties by score descending.
        let scored = vec![("a", 0.3_f64), ("b", 0.9), ("c", 0.5)];
        let chosen = plan.apply(&scored);
        assert_eq!(chosen.len(), 3);
        // First pick is the highest scorer (`b`).
        assert_eq!(chosen[0], "b");
        // Every id ends up in the chosen set.
        let set: std::collections::HashSet<&str> = chosen.iter().copied().collect();
        assert_eq!(set, ["a", "b", "c"].iter().copied().collect());
    }

    /// `keep_diverse` with N < scored.len() picks the N most
    /// diverse. The first pick is the highest scorer; the second
    /// is the entry most distant from the first.
    #[test]
    fn selection_plan_keep_diverse_actually_diversifies() {
        let plan = SelectionPlan::keep_diverse(2);
        // `b` is the highest scorer. Its tokens are
        // {"b"} (single char — splits on alphanumeric, so just
        // "b"). The other two share no token with `b`, so either
        // is a valid second pick.
        let scored = vec![("a", 0.3_f64), ("b", 0.9), ("c", 0.5)];
        let chosen = plan.apply(&scored);
        assert_eq!(chosen.len(), 2);
        assert_eq!(chosen[0], "b");
        assert!(chosen[1] == "a" || chosen[1] == "c", "got {:?}", chosen);
    }

    /// `keep_outlier` constructs the right plan and the `apply`
    /// path keeps the entries with the largest distance from the
    /// score-weighted centroid.
    #[test]
    fn selection_plan_keep_outlier_constructs() {
        let plan = SelectionPlan::keep_outlier(3);
        assert_eq!(plan.kind, SelectionKind::OutlierN);
        assert_eq!(plan.count, 3);
        // Apply on a 3-entry slice with N=3 returns every id
        // sorted by centroid-distance descending.
        let scored = vec![("a", 0.3_f64), ("b", 0.9), ("c", 0.5)];
        let chosen = plan.apply(&scored);
        assert_eq!(chosen.len(), 3);
        // All three ids present (order may vary but set equality
        // is well-defined for this slice).
        let set: std::collections::HashSet<&str> = chosen.iter().copied().collect();
        assert_eq!(set, ["a", "b", "c"].iter().copied().collect());
    }

    /// `keep_outlier` with N=1 returns exactly one id (the most
    /// outlier-ish entry).
    #[test]
    fn selection_plan_keep_outlier_n_one() {
        let plan = SelectionPlan::keep_outlier(1);
        let scored = vec![("alpha", 0.5), ("beta", 0.5), ("alpha-dup", 0.5)];
        let chosen = plan.apply(&scored);
        assert_eq!(chosen.len(), 1);
    }

    /// `keep_diverse` with N=1 picks the highest scorer.
    #[test]
    fn selection_plan_keep_diverse_n_one_picks_top() {
        let plan = SelectionPlan::keep_diverse(1);
        let scored = vec![("a", 0.3_f64), ("b", 0.9), ("c", 0.5)];
        let chosen = plan.apply(&scored);
        assert_eq!(chosen, vec!["b"]);
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

    /// Profile-supplied judge-quorum overrides take precedence
    /// over the spec baseline (D.6 + D.21.7). Without an
    /// override `judge_quorum_for_mode(fast, cfg) == 1`; with
    /// `{"fast": 3}` the helper returns 3. Pins the precedence
    /// contract so future refactors cannot silently route around
    /// the profile.
    #[test]
    fn profile_judge_quorum_override_takes_precedence_over_mode_default() {
        let mut cfg = crate::config::Config::default();
        // Baseline, no profile: spec D.21.7 values.
        assert_eq!(judge_quorum_for_mode(Mode::Fast, &cfg), 1);
        assert_eq!(judge_quorum_for_mode(Mode::Deep, &cfg), 5);
        // Profile applies: 3 judges on fast even though the spec
        // baseline is 1.
        cfg.profile_judge_quorum_overrides
            .insert("fast".to_owned(), 3);
        assert_eq!(judge_quorum_for_mode(Mode::Fast, &cfg), 3);
        // Untouched modes still use the spec baseline.
        assert_eq!(judge_quorum_for_mode(Mode::Deep, &cfg), 5);
        // Profile that overrides a different mode is a no-op for
        // the queried mode — pins the key-matching contract.
        cfg.profile_judge_quorum_overrides
            .insert("standard".to_owned(), 7);
        assert_eq!(judge_quorum_for_mode(Mode::Deep, &cfg), 5);
        assert_eq!(judge_quorum_for_mode(Mode::Standard, &cfg), 7);
    }

    /// `jaccard_distance` is a pure function: identical sets → 0;
    /// disjoint → 1; partial overlap in between.
    #[test]
    fn jaccard_distance_is_well_defined() {
        let a: HashSet<String> = ["foo", "bar", "baz"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let b: HashSet<String> = ["foo", "bar", "baz"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let c: HashSet<String> = ["qux"].iter().map(|s| s.to_string()).collect();
        assert!((jaccard_distance(&a, &b) - 0.0).abs() < 1e-9);
        assert!((jaccard_distance(&a, &c) - 1.0).abs() < 1e-9);
    }

    /// `token_features_for` is deterministic and case-insensitive:
    /// two ids with the same alphanumeric content hash to the
    /// same set.
    #[test]
    fn token_features_is_case_insensitive() {
        let a: HashSet<String> = token_features_for(&"FooBarBaz");
        let b: HashSet<String> = token_features_for(&"foobarbaz");
        assert_eq!(a, b);
    }
}
