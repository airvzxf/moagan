//! D.22.1: five adversary patterns extending the single-pattern
//! dispatch in the judge phase. Each pattern maps a metric to a
//! boolean verdict and a free-form detail string. The judge phase
//! runs [`run_all_patterns`] over a (re-)scored proposal and
//! promotes any pattern whose `fired == true` into a `RefineAction`
//! (see `super::refine_action`).
//!
//! Spec contract:
//!
//! - [`AdversaryPattern::all_five`] returns the canonical five
//!   patterns in stable order so callers can iterate deterministically.
//! - [`run_all_patterns`] is a pure function: no I/O, no LLM, no DB.
//!   It takes the per-judge scores, the evidence count, and a
//!   concatenated provenance/justification string and returns a
//!   `Vec<PatternVerdict>` with one entry per pattern. Patterns
//!   that need richer context (`ProvenanceMismatch`) are emitted
//!   with `fired = false` and a `detail` payload the caller can
//!   inspect; the caller is responsible for the actual provenance
//!   comparison.
//!
//! Thresholds (spread > 1.0, stddev > 0.5, evidence < 2) are
//! intentional defaults chosen so a single mock judge never trips
//! any of them and a five-judge panel with strong disagreement
//! trips at least one. Tests pin these defaults.

/// One of the five adversary patterns the judge phase evaluates
/// per proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum AdversaryPattern {
    /// `(max - min)` of the per-judge scores is above the spread
    /// threshold. Catches a single dissenting judge whose score
    /// is far from the rest.
    ScoreSpread,
    /// Standard deviation of the per-judge scores is above the
    /// dispersion threshold. Catches a panel that broadly
    /// disagrees even when no single judge is an outlier.
    StdDeviation,
    /// Number of evidence items backing the proposal is below the
    /// minimum. Catches a proposal that the LLM justified with
    /// hand-waving rather than a citation.
    InsufficientEvidence,
    /// Provenance hash mismatch across reviewers. The dispatcher
    /// only records the payload; the caller compares hashes and
    /// sets `fired` accordingly.
    ProvenanceMismatch,
    /// Hallucination signature: a known LLM-meta phrase
    /// ("as an ai", "i cannot", "i don't have") appears in the
    /// provenance string. Catches refusals and self-references
    /// smuggled into a justification.
    HallucinationSignature,
}

impl AdversaryPattern {
    /// Canonical five-pattern array. Returned in stable order so
    /// callers can iterate deterministically and the unit tests
    /// can pin the count.
    pub fn all_five() -> [Self; 5] {
        [
            Self::ScoreSpread,
            Self::StdDeviation,
            Self::InsufficientEvidence,
            Self::ProvenanceMismatch,
            Self::HallucinationSignature,
        ]
    }
}

/// Verdict for one pattern. The judge phase collects these into a
/// `Vec<PatternVerdict>` and dispatches a [`super::refine_action::RefineAction`]
/// for every entry with `fired == true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternVerdict {
    /// Which pattern produced this verdict.
    pub pattern: AdversaryPattern,
    /// `true` when the metric tripped the threshold.
    pub fired: bool,
    /// Human-readable payload: the raw metric (e.g. `spread=1.42`)
    /// or the matched token. Logged alongside the verdict so a
    /// post-mortem can see *why* the pattern fired.
    pub detail: String,
}

/// Run every pattern in [`AdversaryPattern::all_five`] against the
/// supplied inputs. Pure function: no I/O, no LLM, no DB.
///
/// - `scores` is the per-judge overall score slice; the function
///   tolerates `len() == 0` (returns `fired = false` for every
///   pattern with `detail = "no_scores"` so the dispatcher does
///   not spuriously promote a pattern on an empty panel).
/// - `evidence_count` is the number of evidence items the
///   proposal's author attached.
/// - `provenance` is a free-form string the caller concatenates
///   from the per-judge justification blocks; the
///   `HallucinationSignature` pattern scans it for known
///   LLM-meta phrases, and the `ProvenanceMismatch` pattern
///   records the payload verbatim so the caller can compare
///   hashes against `fired = false` (the per-judge hash
///   comparison is the caller's responsibility because the
///   function is single-string-only by design).
pub fn run_all_patterns(
    scores: &[f64],
    evidence_count: usize,
    provenance: &str,
) -> Vec<PatternVerdict> {
    if scores.is_empty() {
        return AdversaryPattern::all_five()
            .into_iter()
            .map(|pattern| PatternVerdict {
                pattern,
                fired: false,
                detail: "no_scores".to_owned(),
            })
            .collect();
    }

    let max = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let min = scores.iter().copied().fold(f64::INFINITY, f64::min);
    let spread = max - min;

    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    let variance = scores.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / scores.len() as f64;
    let stddev = variance.sqrt();

    const HALLUCINATION_TOKENS: &[&str] = &["as an ai", "i cannot", "i don't have"];
    let provenance_lc = provenance.to_lowercase();
    let hallucination_match = HALLUCINATION_TOKENS
        .iter()
        .find(|t| provenance_lc.contains(*t))
        .copied();

    vec![
        PatternVerdict {
            pattern: AdversaryPattern::ScoreSpread,
            fired: spread > 1.0,
            detail: format!("spread={:.3}", spread),
        },
        PatternVerdict {
            pattern: AdversaryPattern::StdDeviation,
            fired: stddev > 0.5,
            detail: format!("stddev={:.3}", stddev),
        },
        PatternVerdict {
            pattern: AdversaryPattern::InsufficientEvidence,
            fired: evidence_count < 2,
            detail: format!("count={}", evidence_count),
        },
        PatternVerdict {
            pattern: AdversaryPattern::ProvenanceMismatch,
            // The actual cross-reviewer comparison is the
            // caller's job; the function records the payload
            // so the caller can decide.
            fired: false,
            detail: format!("provenance={}", provenance),
        },
        PatternVerdict {
            pattern: AdversaryPattern::HallucinationSignature,
            fired: hallucination_match.is_some(),
            detail: format!("matched={}", hallucination_match.unwrap_or("none")),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `all_five` returns exactly the five canonical variants in a
    /// stable order. Pins the count so a future refactor that adds
    /// a sixth pattern trips the test before it lands.
    #[test]
    fn adversary_patterns_all_five_returns_five() {
        let patterns = AdversaryPattern::all_five();
        assert_eq!(patterns.len(), 5);
        assert_eq!(
            patterns,
            [
                AdversaryPattern::ScoreSpread,
                AdversaryPattern::StdDeviation,
                AdversaryPattern::InsufficientEvidence,
                AdversaryPattern::ProvenanceMismatch,
                AdversaryPattern::HallucinationSignature,
            ]
        );
    }

    /// `ScoreSpread` fires when the (max - min) of the per-judge
    /// scores is above 1.0. `[1.0, 3.0]` has spread 2.0; the
    /// verdict must fire.
    #[test]
    fn run_all_patterns_fires_score_spread_when_far_apart() {
        let verdicts = run_all_patterns(&[1.0, 3.0], 5, "");
        let spread = verdicts
            .iter()
            .find(|v| v.pattern == AdversaryPattern::ScoreSpread)
            .expect("ScoreSpread verdict must be present");
        assert!(spread.fired, "spread should fire, detail={}", spread.detail);
        assert!(spread.detail.starts_with("spread="));
    }

    /// `HallucinationSignature` fires when the provenance string
    /// contains a known LLM-meta phrase. The test covers two
    /// phrases (case-insensitive) and confirms the detail string
    /// names the matched token.
    #[test]
    fn run_all_patterns_fires_hallucination_signature() {
        let verdicts = run_all_patterns(&[0.5, 0.6], 5, "As an AI, I cannot confirm");
        let hallu = verdicts
            .iter()
            .find(|v| v.pattern == AdversaryPattern::HallucinationSignature)
            .expect("HallucinationSignature verdict must be present");
        assert!(
            hallu.fired,
            "hallucination should fire, detail={}",
            hallu.detail
        );
        assert!(
            hallu.detail.contains("as an ai") || hallu.detail.contains("i cannot"),
            "detail should name the matched token; got {}",
            hallu.detail
        );

        // Lower-case token ("i don't have access") also fires.
        let verdicts_lc = run_all_patterns(&[0.5, 0.6], 5, "Sorry, I don't have access to that");
        let hallu_lc = verdicts_lc
            .iter()
            .find(|v| v.pattern == AdversaryPattern::HallucinationSignature)
            .expect("HallucinationSignature verdict must be present");
        assert!(hallu_lc.fired);
    }

    /// `ProvenanceMismatch` is the caller's responsibility: the
    /// function always emits `fired = false` and records the
    /// payload verbatim so the caller can decide after a hash
    /// comparison. Pins that contract.
    #[test]
    fn run_all_patterns_provenance_mismatch_never_fires_from_helper() {
        let verdicts = run_all_patterns(&[0.5], 5, "reviewer-a says X | reviewer-b says X");
        let prov = verdicts
            .iter()
            .find(|v| v.pattern == AdversaryPattern::ProvenanceMismatch)
            .expect("ProvenanceMismatch verdict must be present");
        assert!(!prov.fired);
        assert!(prov.detail.contains("reviewer-a"));
    }

    /// Empty scores slice is a no-op: every pattern returns
    /// `fired = false` and `detail = "no_scores"`. Pins the
    /// zero-input contract.
    #[test]
    fn run_all_patterns_empty_scores_returns_no_fire() {
        let verdicts = run_all_patterns(&[], 0, "");
        assert_eq!(verdicts.len(), 5);
        for v in &verdicts {
            assert!(!v.fired, "{:?} should not fire on empty input", v.pattern);
        }
    }
}
