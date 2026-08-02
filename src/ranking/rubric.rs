//! Rubric anchoring for the 6 criteria used by the rank phase.
//!
//! Each criterion has 3 anchor strings: a "1" (low) anchor, a "3"
//! (mid) anchor, and a "5" (high) anchor. The anchors are short,
//! concrete phrases that the LLM-side critic / judge can use to
//! calibrate its score without re-reading the full rubric.
//!
//! Refs: D.7.4, T00-03 §1087, T15-02 §9.3, T05-06.

use std::collections::HashMap;

/// The 6 criteria the rank phase uses. Each one is rated on a 1/3/5
/// scale; the LLM-side critic / judge interpolates a continuous
/// score by interpolating between the rubric anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Criterion {
    /// Is the proposed solution correct?
    Correctness,
    /// Does it cover every required deliverable?
    Completeness,
    /// Does it respect the brief's constraints?
    Fit,
    /// Is every claim sourced / verifiable?
    Evidence,
    /// Is the prose crisp and unambiguous?
    Clarity,
    /// Overall shippability, holistically.
    Overall,
}

/// The 6-criterion rubric with anchored 1/3/5 phrases for each.
/// Constructed via [`Rubric::default`] which seeds every (criterion,
/// level) pair with a short, concrete phrase. The LLM-side judge
/// embeds the anchor strings directly in its prompt to calibrate
/// the score without re-reading the full rubric.
#[derive(Debug, Clone)]
pub struct Rubric {
    anchors: HashMap<(Criterion, u8), String>,
}

impl Rubric {
    /// Mid-tier anchor for `c` ("what a 3 looks like").
    pub fn anchored_3(&self, c: Criterion) -> &str {
        self.anchors.get(&(c, 3)).map(|s| s.as_str()).unwrap_or("")
    }

    /// High-tier anchor for `c` ("what a 5 looks like").
    pub fn anchored_5(&self, c: Criterion) -> &str {
        self.anchors.get(&(c, 5)).map(|s| s.as_str()).unwrap_or("")
    }

    /// Low-tier anchor for `c` ("what a 1 looks like").
    pub fn anchored_1(&self, c: Criterion) -> &str {
        self.anchors.get(&(c, 1)).map(|s| s.as_str()).unwrap_or("")
    }
}

impl Default for Rubric {
    fn default() -> Self {
        let mut m: HashMap<(Criterion, u8), String> = HashMap::new();
        m.insert(
            (Criterion::Correctness, 1),
            "Not verifiable; unverified hypothesis".to_string(),
        );
        m.insert(
            (Criterion::Correctness, 3),
            "Correct under reasonable assumptions".to_string(),
        );
        m.insert(
            (Criterion::Correctness, 5),
            "Verified, executable evidence".to_string(),
        );
        m.insert(
            (Criterion::Completeness, 1),
            "Missing major required deliverables".to_string(),
        );
        m.insert(
            (Criterion::Completeness, 3),
            "Covers most required deliverables".to_string(),
        );
        m.insert(
            (Criterion::Completeness, 5),
            "Covers every required deliverable".to_string(),
        );
        m.insert(
            (Criterion::Fit, 1),
            "Ignores the brief's constraints".to_string(),
        );
        m.insert((Criterion::Fit, 3), "Honors most constraints".to_string());
        m.insert(
            (Criterion::Fit, 5),
            "Honors every hard constraint".to_string(),
        );
        m.insert(
            (Criterion::Evidence, 1),
            "No sources, no verification".to_string(),
        );
        m.insert((Criterion::Evidence, 3), "Some sources cited".to_string());
        m.insert(
            (Criterion::Evidence, 5),
            "Every claim is sourced".to_string(),
        );
        m.insert(
            (Criterion::Clarity, 1),
            "Hard to follow, ambiguous language".to_string(),
        );
        m.insert(
            (Criterion::Clarity, 3),
            "Readable, mostly unambiguous".to_string(),
        );
        m.insert(
            (Criterion::Clarity, 5),
            "Crisp, unambiguous, well-structured".to_string(),
        );
        m.insert((Criterion::Overall, 1), "Not shippable".to_string());
        m.insert(
            (Criterion::Overall, 3),
            "Shippable with iteration".to_string(),
        );
        m.insert((Criterion::Overall, 5), "Shippable as-is".to_string());
        Self { anchors: m }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rubric_has_anchor_for_every_criterion() {
        let r = Rubric::default();
        for c in [
            Criterion::Correctness,
            Criterion::Completeness,
            Criterion::Fit,
            Criterion::Evidence,
            Criterion::Clarity,
            Criterion::Overall,
        ] {
            assert!(!r.anchored_1(c).is_empty(), "1-anchor missing for {c:?}");
            assert!(!r.anchored_3(c).is_empty(), "3-anchor missing for {c:?}");
            assert!(!r.anchored_5(c).is_empty(), "5-anchor missing for {c:?}");
        }
    }

    #[test]
    fn anchor_1_returns_low_anchor() {
        let r = Rubric::default();
        assert_eq!(
            r.anchored_1(Criterion::Correctness),
            "Not verifiable; unverified hypothesis"
        );
        assert_eq!(
            r.anchored_1(Criterion::Completeness),
            "Missing major required deliverables"
        );
        assert_eq!(
            r.anchored_1(Criterion::Fit),
            "Ignores the brief's constraints"
        );
        assert_eq!(
            r.anchored_1(Criterion::Evidence),
            "No sources, no verification"
        );
        assert_eq!(
            r.anchored_1(Criterion::Clarity),
            "Hard to follow, ambiguous language"
        );
        assert_eq!(r.anchored_1(Criterion::Overall), "Not shippable");
    }

    #[test]
    fn anchor_3_returns_mid_anchor() {
        let r = Rubric::default();
        assert_eq!(
            r.anchored_3(Criterion::Correctness),
            "Correct under reasonable assumptions"
        );
        assert_eq!(
            r.anchored_3(Criterion::Completeness),
            "Covers most required deliverables"
        );
        assert_eq!(r.anchored_3(Criterion::Fit), "Honors most constraints");
        assert_eq!(r.anchored_3(Criterion::Evidence), "Some sources cited");
        assert_eq!(
            r.anchored_3(Criterion::Clarity),
            "Readable, mostly unambiguous"
        );
        assert_eq!(r.anchored_3(Criterion::Overall), "Shippable with iteration");
    }

    #[test]
    fn anchor_5_returns_high_anchor() {
        let r = Rubric::default();
        assert_eq!(
            r.anchored_5(Criterion::Correctness),
            "Verified, executable evidence"
        );
        assert_eq!(
            r.anchored_5(Criterion::Completeness),
            "Covers every required deliverable"
        );
        assert_eq!(r.anchored_5(Criterion::Fit), "Honors every hard constraint");
        assert_eq!(r.anchored_5(Criterion::Evidence), "Every claim is sourced");
        assert_eq!(
            r.anchored_5(Criterion::Clarity),
            "Crisp, unambiguous, well-structured"
        );
        assert_eq!(r.anchored_5(Criterion::Overall), "Shippable as-is");
    }

    #[test]
    fn unknown_criterion_returns_empty() {
        // Anchor 2 and 4 are intentionally not seeded; the public API
        // must return an empty slice rather than panic so callers can
        // interpolate the anchor string unconditionally.
        let r = Rubric::default();
        assert_eq!(
            r.anchors
                .get(&(Criterion::Correctness, 2))
                .map(String::as_str),
            None,
        );
        assert_eq!(
            r.anchors.get(&(Criterion::Overall, 4)).map(String::as_str),
            None,
        );
    }

    #[test]
    fn rubric_anchors_are_non_empty() {
        let r = Rubric::default();
        assert!(!r.anchored_1(Criterion::Correctness).is_empty());
        assert!(!r.anchored_3(Criterion::Correctness).is_empty());
        assert!(!r.anchored_5(Criterion::Correctness).is_empty());
        assert!(!r.anchored_1(Criterion::Completeness).is_empty());
        assert!(!r.anchored_3(Criterion::Completeness).is_empty());
        assert!(!r.anchored_5(Criterion::Completeness).is_empty());
        assert!(!r.anchored_1(Criterion::Fit).is_empty());
        assert!(!r.anchored_3(Criterion::Fit).is_empty());
        assert!(!r.anchored_5(Criterion::Fit).is_empty());
        assert!(!r.anchored_1(Criterion::Evidence).is_empty());
        assert!(!r.anchored_3(Criterion::Evidence).is_empty());
        assert!(!r.anchored_5(Criterion::Evidence).is_empty());
        assert!(!r.anchored_1(Criterion::Clarity).is_empty());
        assert!(!r.anchored_3(Criterion::Clarity).is_empty());
        assert!(!r.anchored_5(Criterion::Clarity).is_empty());
        assert!(!r.anchored_1(Criterion::Overall).is_empty());
        assert!(!r.anchored_3(Criterion::Overall).is_empty());
        assert!(!r.anchored_5(Criterion::Overall).is_empty());
    }
}
