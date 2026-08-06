//! D.22.1: seven adversary patterns extending the single-pattern
//! dispatch in the judge phase. Each pattern maps a metric to a
//! boolean verdict and a free-form detail string. The judge phase
//! runs [`run_all_patterns`] over a (re-)scored proposal and
//! promotes any pattern whose `fired == true` into a `RefineAction`
//! (see `super::refine_action`).
//!
//! Spec contract:
//!
//! - [`AdversaryPattern::all_seven`] returns the canonical seven
//!   patterns in stable order so callers can iterate deterministically.
//! - [`run_all_patterns`] is a pure function: no I/O, no LLM, no DB.
//!   It takes the per-judge scores, the evidence count, and a
//!   concatenated provenance/justification string and returns a
//!   `Vec<PatternVerdict>` with one entry per pattern. Patterns
//!   that need richer context (`ProvenanceMismatch`,
//!   `ProvenanceDrift`, `AudienceMismatch`) are emitted with
//!   `fired = false` and a `detail` payload the caller can inspect;
//!   the caller is responsible for the actual comparison against
//!   the brief context.
//!
//! Thresholds (spread > 1.0, stddev > 0.5, evidence < 2) are
//! intentional defaults chosen so a single mock judge never trips
//! any of them and a five-judge panel with strong disagreement
//! trips at least one. Tests pin these defaults.

/// One of the seven adversary patterns the judge phase evaluates
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
    /// Provenance drift: the proposal claims sources / citations
    /// that are not present in the brief context. The dispatcher
    /// records the candidate drift span; the caller compares the
    /// span against the brief's source list and sets `fired`.
    ProvenanceDrift,
    /// Audience mismatch: the proposal's tone, scope, or
    /// vocabulary does not fit the audience cue the brief
    /// establishes (e.g. a beginner audience receiving a
    /// kernel-internals deep-dive). The dispatcher records the
    /// detected tone token; the caller classifies against the
    /// brief's audience cue and sets `fired`.
    AudienceMismatch,
}

impl AdversaryPattern {
    /// Canonical seven-pattern array. Returned in stable order so
    /// callers can iterate deterministically and the unit tests
    /// can pin the count.
    pub fn all_seven() -> [Self; 7] {
        [
            Self::ScoreSpread,
            Self::StdDeviation,
            Self::InsufficientEvidence,
            Self::ProvenanceMismatch,
            Self::HallucinationSignature,
            Self::ProvenanceDrift,
            Self::AudienceMismatch,
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

/// Run every pattern in [`AdversaryPattern::all_seven`] against the
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
///   LLM-meta phrases, and the `ProvenanceMismatch`,
///   `ProvenanceDrift`, and `AudienceMismatch` patterns record
///   the payload verbatim so the caller can compare against the
///   brief context (the per-judge comparison is the caller's
///   responsibility because the function is single-string-only by
///   design).
pub fn run_all_patterns(
    scores: &[f64],
    evidence_count: usize,
    provenance: &str,
) -> Vec<PatternVerdict> {
    if scores.is_empty() {
        return AdversaryPattern::all_seven()
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
        PatternVerdict {
            pattern: AdversaryPattern::ProvenanceDrift,
            // The brief-context comparison is the caller's job;
            // the function records the candidate drift span so
            // the caller can decide. Marker token "<claim:">
            // surfaces the unverified references the LLM
            // inserted without sourcing them; absence means no
            // drift signal in the payload.
            fired: provenance_lc.contains("<claim:") && !provenance_lc.contains("brief:source:"),
            detail: format!("drift_span={}", provenance),
        },
        PatternVerdict {
            pattern: AdversaryPattern::AudienceMismatch,
            // The brief-context comparison is the caller's job;
            // the function detects a coarse tone signal
            // (technical jargon paired with no beginner cue)
            // and lets the caller confirm against the brief's
            // audience cue.
            fired: AUDIENCE_JARGON.iter().any(|t| provenance_lc.contains(t))
                && !provenance_lc.contains("for beginners")
                && !provenance_lc.contains("introductory"),
            detail: format!("tone_signal={}", provenance),
        },
    ]
}

const AUDIENCE_JARGON: &[&str] = &["page tables", "tlb shootdown", "ring -1", "vfs layer"];

#[cfg(test)]
mod tests {
    use super::*;

    /// `all_seven` returns exactly the seven canonical variants in
    /// a stable order. Pins the count so a future refactor that
    /// adds an eighth pattern trips the test before it lands.
    #[test]
    fn adversary_patterns_all_seven_returns_seven() {
        let patterns = AdversaryPattern::all_seven();
        assert_eq!(patterns.len(), 7);
        assert_eq!(
            patterns,
            [
                AdversaryPattern::ScoreSpread,
                AdversaryPattern::StdDeviation,
                AdversaryPattern::InsufficientEvidence,
                AdversaryPattern::ProvenanceMismatch,
                AdversaryPattern::HallucinationSignature,
                AdversaryPattern::ProvenanceDrift,
                AdversaryPattern::AudienceMismatch,
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

    /// `ProvenanceDrift` fires when the provenance string contains
    /// a `<claim:>` marker (an unsourced reference the LLM
    /// inserted) and no `brief:source:` anchor (a citation the
    /// brief actually supplies). The marker-based heuristic lets
    /// the helper surface drift without needing access to the
    /// brief itself; the caller still confirms against the brief
    /// context.
    #[test]
    fn run_all_seven_patterns_fires_provenance_drift() {
        let verdicts = run_all_patterns(
            &[0.5, 0.6],
            3,
            "The proposal cites <claim:kernel-2003> without grounding it.",
        );
        let drift = verdicts
            .iter()
            .find(|v| v.pattern == AdversaryPattern::ProvenanceDrift)
            .expect("ProvenanceDrift verdict must be present");
        assert!(
            drift.fired,
            "drift should fire on <claim:> without brief:source: anchor, detail={}",
            drift.detail
        );

        // Anchored claims (brief:source: token present) do not
        // fire — the caller is still in charge of the real check,
        // but the helper stays conservative on anchored payloads.
        let verdicts_anchored = run_all_patterns(
            &[0.5, 0.6],
            3,
            "brief:source:kernel-2003 <claim:kernel-2003>",
        );
        let drift_anchored = verdicts_anchored
            .iter()
            .find(|v| v.pattern == AdversaryPattern::ProvenanceDrift)
            .expect("ProvenanceDrift verdict must be present");
        assert!(!drift_anchored.fired);
    }

    /// `AudienceMismatch` fires when the provenance string is
    /// dense in jargon and contains no beginner cue. Pins the
    /// "kernel-internals prose + no `for beginners` / `introductory`
    /// marker" heuristic the helper uses as a coarse tone
    /// signal.
    #[test]
    fn run_all_seven_patterns_fires_audience_mismatch() {
        let verdicts = run_all_patterns(
            &[0.5, 0.6],
            3,
            "Walks through page tables and TLB shootdown mechanics in detail.",
        );
        let aud = verdicts
            .iter()
            .find(|v| v.pattern == AdversaryPattern::AudienceMismatch)
            .expect("AudienceMismatch verdict must be present");
        assert!(
            aud.fired,
            "audience mismatch should fire on jargon without a beginner cue, detail={}",
            aud.detail
        );

        // Same jargon with a beginner cue does NOT fire — the
        // caller still confirms, but the helper stays conservative
        // on beginner-tagged payloads.
        let verdicts_beginner = run_all_patterns(
            &[0.5, 0.6],
            3,
            "Introductory walk-through of page tables and TLB shootdown for beginners.",
        );
        let aud_b = verdicts_beginner
            .iter()
            .find(|v| v.pattern == AdversaryPattern::AudienceMismatch)
            .expect("AudienceMismatch verdict must be present");
        assert!(!aud_b.fired);
    }

    /// Empty scores slice is a no-op: every pattern returns
    /// `fired = false` and `detail = "no_scores"`. Pins the
    /// zero-input contract for all seven patterns.
    #[test]
    fn run_all_patterns_empty_scores_returns_no_fire() {
        let verdicts = run_all_patterns(&[], 0, "");
        assert_eq!(verdicts.len(), 7);
        for v in &verdicts {
            assert!(!v.fired, "{:?} should not fire on empty input", v.pattern);
        }
    }
}
