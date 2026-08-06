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
//! features derived from each `Proposal`'s `summary + approach +
//! tradeoffs + evidence` text — cheap, deterministic, and good
//! enough for the operator-facing diversification. Useful when
//! the operator wants the spread, not the average.
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
//! // Selection plan: keep the top 3 by score. The input slice
//! // carries (proposal_id, weighted_score, &Proposal) tuples so
//! // `keep_diverse` / `keep_outlier` can compute Jaccard distance
//! // over the proposal text.
//! let plan = SelectionPlan::keep_top(3);
//! let scored: Vec<(String, f64, moagan::domain::Proposal)> = vec![];
//! let chosen = plan.apply(&scored);
//! assert!(chosen.is_empty());
//! ```

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::cli::Mode;
use crate::domain::Proposal;
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

    /// Apply the plan to a `(proposal_id, weighted_score, &Proposal)`
    /// slice and return the chosen proposal ids. The slice carries
    /// the proposal text so `keep_diverse` / `keep_outlier` can
    /// compute Jaccard distance over the proposal's actual
    /// `summary + approach + tradeoffs + evidence`. `count == 0`
    /// is a no-op (returns empty). When `count >= scored.len()`
    /// every id is returned in the order the plan dictates.
    ///
    /// Strategies:
    /// - `TopN`     → score-descending sort, take first N. Stable.
    /// - `DiverseN` → greedy farthest-first traversal over Jaccard
    ///   distance on token features extracted from `Proposal` text.
    ///   The first pick is the highest scorer; subsequent picks
    ///   maximise the minimum distance to the already-chosen set.
    /// - `OutlierN` → distance from the score-weighted centroid in
    ///   Jaccard space; keep the N with the largest distance.
    ///
    /// The three strategies are also exposed directly as
    /// [`Self::apply_top`] / [`Self::apply_diverse`] /
    /// [`Self::apply_outlier`] so a future caller can pick a
    /// specific strategy without paying for the `match` on
    /// [`SelectionKind`].
    pub fn apply(&self, scored: &[(String, f64, Proposal)]) -> Vec<String> {
        match self.kind {
            SelectionKind::TopN => self.apply_top(scored),
            SelectionKind::DiverseN => self.apply_diverse(scored),
            SelectionKind::OutlierN => self.apply_outlier(scored),
        }
    }

    /// Apply `keep_top(n)`: sort the input by weighted score
    /// descending and take the first `n` proposal ids. Stable so
    /// ties preserve insertion order. `count == 0` or empty input
    /// returns an empty vector; `count >= scored.len()` returns
    /// every id in score-descending order.
    pub fn apply_top(&self, scored: &[(String, f64, Proposal)]) -> Vec<String> {
        if self.count == 0 || scored.is_empty() {
            return Vec::new();
        }
        let mut sorted: Vec<(String, f64, Proposal)> = scored.to_vec();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        sorted
            .into_iter()
            .take(self.count)
            .map(|(id, _, _)| id)
            .collect()
    }

    /// Apply `keep_diverse(n)`: greedy farthest-first traversal
    /// over Jaccard distance on token features extracted from
    /// each `Proposal`'s `summary + approach + tradeoffs +
    /// evidence`. The first pick is the highest-scoring entry;
    /// each subsequent pick maximises the minimum distance to
    /// the already-chosen set. Ties on min-distance break by
    /// score descending so the highest scorer wins.
    pub fn apply_diverse(&self, scored: &[(String, f64, Proposal)]) -> Vec<String> {
        if self.count == 0 || scored.is_empty() {
            return Vec::new();
        }
        let mut sorted: Vec<(String, f64, Proposal)> = scored.to_vec();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let n = self.count.min(sorted.len());
        let mut chosen: Vec<String> = Vec::with_capacity(n);
        let mut chosen_features: Vec<HashSet<String>> = Vec::with_capacity(n);
        let mut remaining: Vec<(String, f64, Proposal)> = sorted;
        for _ in 0..n {
            let mut best_idx = 0usize;
            let mut best_min_dist = f64::NEG_INFINITY;
            let mut best_score = f64::NEG_INFINITY;
            for (idx, (_id, score, proposal)) in remaining.iter().enumerate() {
                let feats = token_features_for_proposal(proposal);
                let min_dist = if chosen.is_empty() {
                    // First pick: any min_dist is a tie; break by
                    // score descending.
                    0.0
                } else {
                    chosen_features
                        .iter()
                        .map(|c| jaccard_distance(&feats, c))
                        .fold(f64::INFINITY, f64::min)
                };
                if min_dist > best_min_dist || (min_dist == best_min_dist && *score > best_score) {
                    best_idx = idx;
                    best_min_dist = min_dist;
                    best_score = *score;
                }
            }
            let (id, _, proposal) = remaining.remove(best_idx);
            chosen_features.push(token_features_for_proposal(&proposal));
            chosen.push(id);
        }
        chosen
    }

    /// Apply `keep_outlier(n)`: keep the N proposals whose token
    /// set is most distant from the score-weighted token bag of
    /// the OTHER proposals. The intuition is "outlier": a proposal
    /// whose vocabulary is rare or absent from the rest of the
    /// field is the contrarian pick. A naive "centroid of all
    /// proposals" would include each proposal's own tokens in the
    /// centroid, collapsing every distance toward 0.5; the
    /// leave-one-out formulation recovers the intended contrast.
    ///
    /// `count == 0` or empty input returns an empty vector. When
    /// `count >= scored.len()` every id is returned in
    /// outlier-distance descending order.
    pub fn apply_outlier(&self, scored: &[(String, f64, Proposal)]) -> Vec<String> {
        if self.count == 0 || scored.is_empty() {
            return Vec::new();
        }
        let total: f64 = scored.iter().map(|(_, s, _)| *s).sum();
        let mut distances: Vec<(String, f64)> = Vec::with_capacity(scored.len());
        for (i, (id, _, proposal)) in scored.iter().enumerate() {
            let mut weights: HashMap<String, f64> = HashMap::new();
            for (j, (_, other_score, other_proposal)) in scored.iter().enumerate() {
                if i == j {
                    continue;
                }
                let w = if total > 0.0 {
                    other_score / total
                } else {
                    1.0 / (scored.len().saturating_sub(1)) as f64
                };
                for tok in token_features_for_proposal(other_proposal) {
                    *weights.entry(tok).or_insert(0.0) += w;
                }
            }
            let others: HashSet<String> = weights.keys().cloned().collect();
            let feats = token_features_for_proposal(proposal);
            let d = jaccard_distance(&feats, &others);
            distances.push((id.clone(), d));
        }
        distances.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        distances
            .into_iter()
            .take(self.count)
            .map(|(id, _)| id)
            .collect()
    }

    /// Default SelectionPlan for a given mode string. Spec D.21.3:
    /// the per-mode baseline lives here so the rank phase can pick
    /// a strategy without inspecting the Config (the config field
    /// is opt-in and lands in a later commit).
    ///
    /// - `fast`     → `keep_top(3)`
    /// - `standard` → `keep_top(5)`
    /// - `deep`     → `keep_top(10)`
    /// - `explore`  → `keep_diverse(15)` — explore-mode spreads
    ///   ideas, top-scoring by score would lose the spread.
    /// - `batch`    → `keep_top(8)`
    /// - other      → `keep_top(5)` (safe fallback for unknown modes)
    pub fn default_for_mode(mode: &str) -> Self {
        match mode {
            "fast" => Self::keep_top(3),
            "standard" => Self::keep_top(5),
            "deep" => Self::keep_top(10),
            "explore" => Self::keep_diverse(15),
            "batch" => Self::keep_top(8),
            _ => Self::keep_top(5),
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

/// Heuristic: derive a token-feature set from a `Proposal`'s
/// textual fields (`summary + approach + tradeoffs + evidence`).
/// Cheap, deterministic, case-insensitive, and good enough for
/// Jaccard-based distance. The previous implementation tokenised
/// the id's `Debug` representation; switching to the actual
/// proposal text gives the distance metrics a meaningful signal
/// (two proposals with the same id but different content now
/// diverge on text, not on a useless prefix).
fn token_features_for_proposal(p: &Proposal) -> HashSet<String> {
    let raw = format!(
        "{} {} {} {}",
        p.summary,
        p.approach,
        p.tradeoffs.join(" "),
        p.evidence.join(" ")
    );
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
        let scored: Vec<(String, f64, Proposal)> = vec![
            ("a".into(), 0.3, Proposal::default()),
            ("b".into(), 0.9, Proposal::default()),
            ("c".into(), 0.5, Proposal::default()),
            ("d".into(), 0.8, Proposal::default()),
        ];
        let plan = SelectionPlan::keep_top(2);
        let chosen = plan.apply(&scored);
        assert_eq!(chosen, vec!["b", "d"]);
    }

    /// `keep_top` with `count >= scored.len()` returns every id.
    #[test]
    fn selection_plan_keep_top_saturates() {
        let scored: Vec<(String, f64, Proposal)> = vec![
            ("a".into(), 0.3, Proposal::default()),
            ("b".into(), 0.9, Proposal::default()),
        ];
        let plan = SelectionPlan::keep_top(10);
        let chosen = plan.apply(&scored);
        assert_eq!(chosen, vec!["b", "a"]);
    }

    /// `keep_top` with `count == 0` is a no-op.
    #[test]
    fn selection_plan_keep_top_zero_returns_empty() {
        let scored: Vec<(String, f64, Proposal)> = vec![
            ("a".into(), 0.3, Proposal::default()),
            ("b".into(), 0.9, Proposal::default()),
        ];
        let plan = SelectionPlan::keep_top(0);
        assert!(plan.apply(&scored).is_empty());
    }

    /// `keep_top` on an empty slice is a no-op.
    #[test]
    fn selection_plan_keep_top_empty_input() {
        let scored: Vec<(String, f64, Proposal)> = Vec::new();
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
        let scored: Vec<(String, f64, Proposal)> = vec![
            ("a".into(), 0.3, Proposal::default()),
            ("b".into(), 0.9, Proposal::default()),
            ("c".into(), 0.5, Proposal::default()),
        ];
        let chosen = plan.apply(&scored);
        assert_eq!(chosen.len(), 3);
        // First pick is the highest scorer (`b`).
        assert_eq!(chosen[0], "b");
        // Every id ends up in the chosen set.
        let set: std::collections::HashSet<&str> = chosen.iter().map(String::as_str).collect();
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
        let scored: Vec<(String, f64, Proposal)> = vec![
            ("a".into(), 0.3, Proposal::default()),
            ("b".into(), 0.9, Proposal::default()),
            ("c".into(), 0.5, Proposal::default()),
        ];
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
        let scored: Vec<(String, f64, Proposal)> = vec![
            ("a".into(), 0.3, Proposal::default()),
            ("b".into(), 0.9, Proposal::default()),
            ("c".into(), 0.5, Proposal::default()),
        ];
        let chosen = plan.apply(&scored);
        assert_eq!(chosen.len(), 3);
        // All three ids present (order may vary but set equality
        // is well-defined for this slice).
        let set: std::collections::HashSet<&str> = chosen.iter().map(String::as_str).collect();
        assert_eq!(set, ["a", "b", "c"].iter().copied().collect());
    }

    /// `keep_outlier` with N=1 returns exactly one id (the most
    /// outlier-ish entry).
    #[test]
    fn selection_plan_keep_outlier_n_one() {
        let plan = SelectionPlan::keep_outlier(1);
        let scored: Vec<(String, f64, Proposal)> = vec![
            ("alpha".into(), 0.5, Proposal::default()),
            ("beta".into(), 0.5, Proposal::default()),
            ("alpha-dup".into(), 0.5, Proposal::default()),
        ];
        let chosen = plan.apply(&scored);
        assert_eq!(chosen.len(), 1);
    }

    /// `keep_diverse` with N=1 picks the highest scorer.
    #[test]
    fn selection_plan_keep_diverse_n_one_picks_top() {
        let plan = SelectionPlan::keep_diverse(1);
        let scored: Vec<(String, f64, Proposal)> = vec![
            ("a".into(), 0.3, Proposal::default()),
            ("b".into(), 0.9, Proposal::default()),
            ("c".into(), 0.5, Proposal::default()),
        ];
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

    /// `token_features_for_proposal` is deterministic and
    /// case-insensitive: two `Proposal`s whose textual fields
    /// differ only in casing tokenise to the same set.
    #[test]
    fn token_features_for_proposal_is_case_insensitive() {
        let a = Proposal {
            id: String::new(),
            summary: "FooBarBaz".into(),
            approach: String::new(),
            tradeoffs: Vec::new(),
            evidence: Vec::new(),
            ..Proposal::default()
        };
        let b = Proposal {
            id: String::new(),
            summary: "foobarbaz".into(),
            approach: String::new(),
            tradeoffs: Vec::new(),
            evidence: Vec::new(),
            ..Proposal::default()
        };
        let fa: HashSet<String> = token_features_for_proposal(&a);
        let fb: HashSet<String> = token_features_for_proposal(&b);
        assert_eq!(fa, fb);
    }

    /// E3 (Track E): `apply_top` operates on `(id, score, Proposal)`
    /// triples and returns the top-N by score. Pins the new
    /// signature so a future refactor that drops the score (or
    /// drops Proposal access) trips the test before it lands.
    #[test]
    fn selection_plan_apply_top_returns_top_n() {
        let scored: Vec<(String, f64, Proposal)> = vec![
            (
                "p1".into(),
                0.3,
                Proposal {
                    summary: "first".into(),
                    ..Proposal::default()
                },
            ),
            (
                "p2".into(),
                0.9,
                Proposal {
                    summary: "second".into(),
                    ..Proposal::default()
                },
            ),
            (
                "p3".into(),
                0.5,
                Proposal {
                    summary: "third".into(),
                    ..Proposal::default()
                },
            ),
            (
                "p4".into(),
                0.8,
                Proposal {
                    summary: "fourth".into(),
                    ..Proposal::default()
                },
            ),
        ];
        let plan = SelectionPlan::keep_top(2);
        let chosen = plan.apply_top(&scored);
        assert_eq!(chosen, vec!["p2", "p4"]);
    }

    /// E3: `apply_diverse` uses Jaccard distance over the
    /// `Proposal`'s textual fields. Two proposals that share no
    /// tokens have Jaccard distance 1.0 and the farthest-first
    /// traversal picks them. The test pins that the distance
    /// metric operates on Proposal text, not on the proposal id.
    #[test]
    fn selection_plan_apply_diverse_uses_jaccard_distance() {
        // p1 and p3 share zero tokens with p2 ("alpha"/"beta").
        // p2 is the highest scorer; the second pick is whichever
        // of p1/p3 is most distant from p2 — both are valid
        // because they share no tokens. The test pins that the
        // distance metric actually inspects the proposal text.
        let scored: Vec<(String, f64, Proposal)> = vec![
            (
                "p1".into(),
                0.3,
                Proposal {
                    summary: "alpha alpha alpha".into(),
                    approach: "alpha".into(),
                    ..Proposal::default()
                },
            ),
            (
                "p2".into(),
                0.9,
                Proposal {
                    summary: "beta beta".into(),
                    approach: "beta beta beta".into(),
                    ..Proposal::default()
                },
            ),
            (
                "p3".into(),
                0.5,
                Proposal {
                    summary: "gamma gamma gamma gamma".into(),
                    approach: "gamma".into(),
                    ..Proposal::default()
                },
            ),
        ];
        let plan = SelectionPlan::keep_diverse(2);
        let chosen = plan.apply_diverse(&scored);
        assert_eq!(chosen.len(), 2);
        // First pick is always the highest scorer.
        assert_eq!(chosen[0], "p2");
        // Second pick is whichever of p1/p3 has the larger
        // min-distance to p2. Both are at distance 1.0 (no
        // shared tokens with p2), so the tiebreaker is score
        // descending → p3 wins (0.5 > 0.3). This pins that
        // distance actually inspects the proposal text.
        assert_eq!(
            chosen[1], "p3",
            "Jaccard distance must use Proposal text, not id"
        );
    }

    /// E3: `apply_outlier` returns the proposal with the largest
    /// distance from the score-weighted centroid. Construct three
    /// proposals: two share a vocabulary (`alpha beta gamma`) and
    /// one is the outlier (`delta epsilon zeta`). The outlier wins.
    #[test]
    fn selection_plan_apply_outlier_returns_max_distance_from_centroid() {
        let outlier = Proposal {
            summary: "delta epsilon zeta".into(),
            approach: "delta epsilon".into(),
            ..Proposal::default()
        };
        let common_a = Proposal {
            summary: "alpha beta gamma".into(),
            approach: "alpha beta".into(),
            ..Proposal::default()
        };
        let common_b = Proposal {
            summary: "alpha gamma beta".into(),
            approach: "beta alpha gamma".into(),
            ..Proposal::default()
        };
        let scored: Vec<(String, f64, Proposal)> = vec![
            ("common_a".into(), 0.5, common_a),
            ("outlier".into(), 0.5, outlier),
            ("common_b".into(), 0.5, common_b),
        ];
        let plan = SelectionPlan::keep_outlier(1);
        let chosen = plan.apply_outlier(&scored);
        assert_eq!(chosen, vec!["outlier"]);
    }

    /// E3: `SelectionPlan::default_for_mode` returns the
    /// spec-baseline plan for each mode and a safe fallback for
    /// unknown mode strings. Pins the defaults so a future
    /// refactor that drifts a number trips the test.
    #[test]
    fn selection_plan_default_for_mode_returns_mode_baseline() {
        assert_eq!(
            SelectionPlan::default_for_mode("fast"),
            SelectionPlan::keep_top(3)
        );
        assert_eq!(
            SelectionPlan::default_for_mode("standard"),
            SelectionPlan::keep_top(5)
        );
        assert_eq!(
            SelectionPlan::default_for_mode("deep"),
            SelectionPlan::keep_top(10)
        );
        assert_eq!(
            SelectionPlan::default_for_mode("explore"),
            SelectionPlan::keep_diverse(15)
        );
        assert_eq!(
            SelectionPlan::default_for_mode("batch"),
            SelectionPlan::keep_top(8)
        );
        // Unknown mode falls back to keep_top(5).
        assert_eq!(
            SelectionPlan::default_for_mode("unknown"),
            SelectionPlan::keep_top(5)
        );
    }
}
