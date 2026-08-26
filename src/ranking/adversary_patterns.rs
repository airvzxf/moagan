//! D.22.1 + D.12.5: twelve adversary patterns extending the
//! single-pattern dispatch in the judge phase. Each pattern maps a
//! metric to a boolean verdict and a free-form detail string. The
//! judge phase runs [`run_all_patterns`] over a (re-)scored proposal
//! and promotes any pattern whose `fired == true` into a
//! `RefineAction` (see `super::refine_action`).
//!
//! Spec contract:
//!
//! - Patterns split into two historical layers:
//!   - The original **seven** patterns from PR-11 / v0.5 (D.22.1),
//!     returned by [`AdversaryPattern::all_seven`] for backward
//!     compatibility with reports shipped before the v0.7 add-on.
//!   - The **five** add-on patterns from D.12.5
//!     (`shared_blind_spots`, `unanimous_claims_without_evidence`,
//!     `hidden_assumptions`, `omitted_risks`, `unverified_claims`),
//!     catalogued in D.22.1 as the "5-pattern add-on".
//! - The canonical twelve-pattern array is returned by
//!   [`AdversaryPattern::all`] in stable order so callers can
//!   iterate deterministically and the unit tests can pin the
//!   count. The phase writes one section per entry.
//! - [`run_all_patterns`] is a pure function: no I/O, no LLM, no DB.
//!   It takes the per-judge scores, the evidence count, and a
//!   concatenated provenance/justification string and returns a
//!   `Vec<PatternVerdict>` with one entry per pattern. Patterns
//!   that need richer context (`ProvenanceMismatch`,
//!   `ProvenanceDrift`) are emitted with `fired = false` and a
//!   `detail` payload the caller can inspect; the caller is
//!   responsible for the actual comparison against the brief
//!   context.
//!
//! Thresholds (spread > 1.0, stddev > 0.5, evidence < 2,
//! provenance length > 200 for the `OmittedRisks` heuristic) are
//! intentional defaults chosen so a single mock judge never trips
//! any of them and a five-judge panel with strong disagreement
//! trips at least one. Tests pin these defaults.

/// One of the twelve adversary patterns the judge phase evaluates
/// per proposal.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum AdversaryPattern {
    /// `(max - min)` of the per-judge scores is above the spread
    /// threshold. Catches a single dissenting judge whose score
    /// is far from the rest.
    #[default]
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
    /// Shared blind spots (D.12.5 add-on): the proposal claims a
    /// consensus the rest of the field does not actually share
    /// (e.g. "everyone agrees", "the consensus is", "universally
    /// accepted", "all agree"). Catches proposals that lean on
    /// invented agreement to dodge the burden of proof.
    SharedBlindSpots,
    /// Unanimous claims without evidence (D.12.5 add-on): the
    /// proposal uses high-confidence markers ("definitely",
    /// "certainly", "clearly", "obviously", "without a doubt")
    /// while carrying fewer than two evidence items. Catches
    /// proposals that sound confident but cite nothing.
    UnanimousClaimsWithoutEvidence,
    /// Hidden assumptions (D.12.5 add-on): the proposal smuggles
    /// in implicit assumptions ("assuming", "we assume",
    /// "presumably", "implicitly", "given that") without
    /// grounding them in the brief. Catches proposals whose
    /// reasoning rests on premises the brief never granted.
    HiddenAssumptions,
    /// Omitted risks (D.12.5 add-on): the proposal is non-trivial
    /// (provenance length above the threshold) but contains no
    /// risk vocabulary ("risk", "downside", "caveat", "trade-off",
    /// "drawback", "edge case", "limitation", "failure mode").
    /// Catches proposals that radiate unwarranted certainty by
    /// silently eliding every failure mode.
    OmittedRisks,
    /// Unverified claims (D.12.5 add-on): the proposal uses
    /// attribution markers ("studies show", "research indicates",
    /// "experts say", "reportedly") without naming a source
    /// identifier. Catches proposals that pass off folklore as
    /// evidence by dressing it up in citation-shaped language.
    UnverifiedClaims,
}

impl AdversaryPattern {
    /// Backward-compatible alias returning the original seven
    /// patterns from PR-11 / v0.5 (D.22.1). Kept so reports
    /// shipped before the v0.7 add-on continue to round-trip, and
    /// so existing tests pinning the seven-variant contract do
    /// not need to be rewritten. New code should prefer
    /// [`AdversaryPattern::all`].
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

    /// Canonical twelve-pattern array (D.22.1 + D.12.5 add-on).
    /// Returned in stable order so callers can iterate
    /// deterministically and the unit tests can pin the count.
    /// The first seven entries are exactly
    /// [`AdversaryPattern::all_seven`] so the historical order is
    /// preserved; the last five are the D.12.5 add-on.
    pub fn all() -> [Self; 12] {
        [
            Self::ScoreSpread,
            Self::StdDeviation,
            Self::InsufficientEvidence,
            Self::ProvenanceMismatch,
            Self::HallucinationSignature,
            Self::ProvenanceDrift,
            Self::AudienceMismatch,
            Self::SharedBlindSpots,
            Self::UnanimousClaimsWithoutEvidence,
            Self::HiddenAssumptions,
            Self::OmittedRisks,
            Self::UnverifiedClaims,
        ]
    }
}

/// Verdict for one pattern. The judge phase collects these into a
/// `Vec<PatternVerdict>` and dispatches a [`super::refine_action::RefineAction`]
/// for every entry with `fired == true`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// Look up the first token in `tokens` that appears in `haystack`
/// (assumed pre-lowercased). Returns the matched token from the
/// `&'static` token table, or `"none"` when nothing matches. A
/// free function instead of a closure so the caller's lifetime
/// does not have to outlive the `provenance_lc` borrow used for
/// the substring check.
fn first_match_token(tokens: &[&'static str], haystack: &str) -> &'static str {
    let out = tokens
        .iter()
        .copied()
        .find(|t| haystack.contains(t))
        .unwrap_or("none");
    tracing::trace!(matched = %out, "ranking::adversary_patterns::first_match_token");
    out
}

/// Run every pattern in [`AdversaryPattern::all`] against the
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
///   `HallucinationSignature`, `SharedBlindSpots`,
///   `HiddenAssumptions`, `UnverifiedClaims`, and
///   `UnanimousClaimsWithoutEvidence` patterns scan it for known
///   text markers, and the `ProvenanceMismatch`,
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
    tracing::debug!(
        scores = scores.len(),
        evidence_count,
        provenance_len = provenance.len(),
        "ranking::adversary_patterns::run_all_patterns: enter"
    );
    if scores.is_empty() {
        tracing::warn!("ranking::adversary_patterns::run_all_patterns: empty scores slice");
        return AdversaryPattern::all()
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
    tracing::trace!(
        spread,
        stddev,
        "ranking::adversary_patterns::run_all_patterns: spread/stddev"
    );

    const HALLUCINATION_TOKENS: &[&str] = &["as an ai", "i cannot", "i don't have"];
    const AUDIENCE_JARGON: &[&str] = &["page tables", "tlb shootdown", "ring -1", "vfs layer"];
    // D.12.5 add-on token tables. The token lists are intentionally
    // broad (multi-word phrases) so a single false-positive on
    // any one token cannot be reduced to a typo-skip; the helper
    // still falls back to `fired = false` when nothing matches
    // so the per-pattern `detail` exposes the no-match signal.
    const SHARED_BLIND_SPOT_TOKENS: &[&str] = &[
        "everyone agrees",
        "experts agree",
        "the consensus",
        "universally accepted",
        "all agree",
        "widely accepted",
        "the field agrees",
    ];
    const HIDDEN_ASSUMPTION_TOKENS: &[&str] = &[
        "assuming ",
        "we assume",
        "i assume",
        "presumably",
        "implicitly",
        "it is implied",
        "given that",
        "suppose ",
        "let's assume",
    ];
    const UNANIMOUS_MARKERS: &[&str] = &[
        "definitely",
        "certainly",
        "clearly",
        "obviously",
        "without a doubt",
        "undoubtedly",
        "unquestionably",
        "without doubt",
    ];
    const RISK_TOKENS: &[&str] = &[
        "risk",
        "downside",
        "caveat",
        "trade-off",
        "tradeoff",
        "drawback",
        "edge case",
        "limitation",
        "failure mode",
        "pitfall",
    ];
    const UNVERIFIED_CLAIM_TOKENS: &[&str] = &[
        "studies show",
        "research indicates",
        "experts say",
        "reportedly",
        "it is said that",
        "people say",
        "it is well known",
        "conventional wisdom",
    ];

    let provenance_lc = provenance.to_lowercase();

    let hallucination_match = HALLUCINATION_TOKENS
        .iter()
        .find(|t| provenance_lc.contains(*t))
        .copied();
    let shared_blind_spots_match = first_match_token(SHARED_BLIND_SPOT_TOKENS, &provenance_lc);
    let hidden_assumptions_match = first_match_token(HIDDEN_ASSUMPTION_TOKENS, &provenance_lc);
    let unverified_claims_match = first_match_token(UNVERIFIED_CLAIM_TOKENS, &provenance_lc);
    let unanimous_match = first_match_token(UNANIMOUS_MARKERS, &provenance_lc);
    let unanimous_present = unanimous_match != "none";
    let risk_tokens_present = RISK_TOKENS.iter().any(|t| provenance_lc.contains(t));

    // `OmittedRisks` is the only pattern that needs the raw
    // provenance length. The 200-char threshold is intentionally
    // chosen so a short justification (which legitimately has no
    // risk section) does not trip the pattern; only non-trivial
    // proposals that *should* mention a risk are flagged.
    const OMITTED_RISK_MIN_LEN: usize = 200;

    let verdicts = vec![
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
        PatternVerdict {
            pattern: AdversaryPattern::SharedBlindSpots,
            fired: shared_blind_spots_match != "none",
            detail: format!("consensus_claim={}", shared_blind_spots_match),
        },
        PatternVerdict {
            pattern: AdversaryPattern::UnanimousClaimsWithoutEvidence,
            fired: unanimous_present && evidence_count < 2,
            detail: format!(
                "confidence_marker={}, evidence_count={}",
                unanimous_match, evidence_count
            ),
        },
        PatternVerdict {
            pattern: AdversaryPattern::HiddenAssumptions,
            fired: hidden_assumptions_match != "none",
            detail: format!("implicit_assumption={}", hidden_assumptions_match),
        },
        PatternVerdict {
            pattern: AdversaryPattern::OmittedRisks,
            fired: provenance.len() > OMITTED_RISK_MIN_LEN && !risk_tokens_present,
            detail: format!(
                "provenance_len={}, risk_tokens_present={}",
                provenance.len(),
                risk_tokens_present
            ),
        },
        PatternVerdict {
            pattern: AdversaryPattern::UnverifiedClaims,
            fired: unverified_claims_match != "none",
            detail: format!("sourceless_attribution={}", unverified_claims_match),
        },
    ];
    let fired_count = verdicts.iter().filter(|v| v.fired).count();
    tracing::debug!(
        fired = fired_count,
        total = verdicts.len(),
        "ranking::adversary_patterns::run_all_patterns: verdicts computed"
    );
    verdicts
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `all_seven` returns exactly the seven canonical variants in
    /// a stable order. Pins the historical PR-11 contract so the
    /// backward-compatible alias never silently drifts.
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

    /// `all` returns the full twelve canonical variants in a
    /// stable order. Pins the count so a future refactor that
    /// adds a thirteenth pattern trips the test before it lands.
    /// The first seven entries must equal `all_seven()` so the
    /// historical order is preserved.
    #[test]
    fn adversary_patterns_all_returns_twelve() {
        let patterns = AdversaryPattern::all();
        assert_eq!(patterns.len(), 12);
        let (first_seven, rest) = patterns.split_at(7);
        assert_eq!(first_seven, &AdversaryPattern::all_seven());
        // The five add-on patterns are the new ones.
        assert_eq!(
            rest,
            &[
                AdversaryPattern::SharedBlindSpots,
                AdversaryPattern::UnanimousClaimsWithoutEvidence,
                AdversaryPattern::HiddenAssumptions,
                AdversaryPattern::OmittedRisks,
                AdversaryPattern::UnverifiedClaims,
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
    fn run_all_patterns_fires_provenance_drift() {
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
    fn run_all_patterns_fires_audience_mismatch() {
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
    /// zero-input contract for all twelve patterns.
    #[test]
    fn run_all_patterns_empty_scores_returns_no_fire() {
        let verdicts = run_all_patterns(&[], 0, "");
        assert_eq!(verdicts.len(), 12);
        for v in &verdicts {
            assert!(!v.fired, "{:?} should not fire on empty input", v.pattern);
        }
    }

    /// `SharedBlindSpots` fires when the provenance string
    /// asserts a consensus the proposal does not back up
    /// ("everyone agrees", "the consensus is", ...). Pins the
    /// D.12.5 add-on contract.
    #[test]
    fn run_all_patterns_fires_shared_blind_spots() {
        let verdicts = run_all_patterns(
            &[0.5, 0.6],
            5,
            "Everyone agrees this is the canonical approach.",
        );
        let blind = verdicts
            .iter()
            .find(|v| v.pattern == AdversaryPattern::SharedBlindSpots)
            .expect("SharedBlindSpots verdict must be present");
        assert!(
            blind.fired,
            "shared blind spots should fire on consensus claim, detail={}",
            blind.detail
        );
        assert!(
            blind.detail.contains("everyone agrees"),
            "detail should name the matched token; got {}",
            blind.detail
        );

        // Variant token: "the consensus is".
        let verdicts_c = run_all_patterns(
            &[0.5, 0.6],
            5,
            "The consensus is that synchronous IO is wrong here.",
        );
        let blind_c = verdicts_c
            .iter()
            .find(|v| v.pattern == AdversaryPattern::SharedBlindSpots)
            .expect("SharedBlindSpots verdict must be present");
        assert!(blind_c.fired);

        // Without the consensus marker: does not fire.
        let verdicts_neutral = run_all_patterns(
            &[0.5, 0.6],
            5,
            "We chose this approach because it scores higher under the rubric.",
        );
        let blind_neutral = verdicts_neutral
            .iter()
            .find(|v| v.pattern == AdversaryPattern::SharedBlindSpots)
            .expect("SharedBlindSpots verdict must be present");
        assert!(
            !blind_neutral.fired,
            "no consensus claim must not fire, detail={}",
            blind_neutral.detail
        );
    }

    /// `UnanimousClaimsWithoutEvidence` fires when the proposal
    /// carries a high-confidence marker ("definitely", "clearly",
    /// ...) and the evidence count is below the threshold
    /// (`< 2`). Pins the D.12.5 add-on contract.
    #[test]
    fn run_all_patterns_fires_unanimous_claims_without_evidence() {
        // Confidence marker + thin evidence → fires.
        let verdicts = run_all_patterns(&[0.5, 0.6], 1, "This is definitely the right answer.");
        let unan = verdicts
            .iter()
            .find(|v| v.pattern == AdversaryPattern::UnanimousClaimsWithoutEvidence)
            .expect("UnanimousClaimsWithoutEvidence verdict must be present");
        assert!(
            unan.fired,
            "unanimous without evidence should fire, detail={}",
            unan.detail
        );
        assert!(unan.detail.contains("definitely"));

        // Same confidence marker + sufficient evidence → does NOT
        // fire: the proposal is allowed to sound confident when it
        // can back it up.
        let verdicts_evidence =
            run_all_patterns(&[0.5, 0.6], 5, "This is definitely the right answer.");
        let unan_e = verdicts_evidence
            .iter()
            .find(|v| v.pattern == AdversaryPattern::UnanimousClaimsWithoutEvidence)
            .expect("UnanimousClaimsWithoutEvidence verdict must be present");
        assert!(
            !unan_e.fired,
            "unanimous with evidence should NOT fire, detail={}",
            unan_e.detail
        );

        // No confidence marker + thin evidence → does NOT fire:
        // the helper only flags over-confident-thin proposals.
        let verdicts_no_conf = run_all_patterns(&[0.5, 0.6], 1, "Maybe this is fine.");
        let unan_nc = verdicts_no_conf
            .iter()
            .find(|v| v.pattern == AdversaryPattern::UnanimousClaimsWithoutEvidence)
            .expect("UnanimousClaimsWithoutEvidence verdict must be present");
        assert!(!unan_nc.fired);
    }

    /// `HiddenAssumptions` fires when the provenance carries an
    /// implicit assumption marker ("assuming ", "we assume",
    /// "presumably", ...). Pins the D.12.5 add-on contract.
    #[test]
    fn run_all_patterns_fires_hidden_assumptions() {
        let verdicts = run_all_patterns(
            &[0.5, 0.6],
            5,
            "Assuming the user has root, we can patch the kernel directly.",
        );
        let hidden = verdicts
            .iter()
            .find(|v| v.pattern == AdversaryPattern::HiddenAssumptions)
            .expect("HiddenAssumptions verdict must be present");
        assert!(
            hidden.fired,
            "hidden assumptions should fire on 'assuming' marker, detail={}",
            hidden.detail
        );
        assert!(
            hidden.detail.contains("assuming"),
            "detail should name the matched token; got {}",
            hidden.detail
        );

        // Variant token: "presumably".
        let verdicts_p =
            run_all_patterns(&[0.5, 0.6], 5, "Presumably the network will be reachable.");
        let hidden_p = verdicts_p
            .iter()
            .find(|v| v.pattern == AdversaryPattern::HiddenAssumptions)
            .expect("HiddenAssumptions verdict must be present");
        assert!(hidden_p.fired);

        // Neutral provenance does not fire.
        let verdicts_neutral = run_all_patterns(
            &[0.5, 0.6],
            5,
            "The proposal cites RFC 8259 and the IANA registry.",
        );
        let hidden_neutral = verdicts_neutral
            .iter()
            .find(|v| v.pattern == AdversaryPattern::HiddenAssumptions)
            .expect("HiddenAssumptions verdict must be present");
        assert!(!hidden_neutral.fired);
    }

    /// `OmittedRisks` fires when the provenance is non-trivial
    /// (above the 200-char threshold) AND contains no risk
    /// vocabulary. Pins the D.12.5 add-on contract.
    #[test]
    fn run_all_patterns_fires_omitted_risks() {
        // Long proposal with no risk language → fires.
        let long_neutral = "a".repeat(300);
        let verdicts = run_all_patterns(&[0.5, 0.6], 5, &long_neutral);
        let omitted = verdicts
            .iter()
            .find(|v| v.pattern == AdversaryPattern::OmittedRisks)
            .expect("OmittedRisks verdict must be present");
        assert!(
            omitted.fired,
            "omitted risks should fire on long provenance with no risk tokens, detail={}",
            omitted.detail
        );
        assert!(omitted.detail.contains("provenance_len=300"));

        // Long proposal that DOES mention risk → does NOT fire.
        let mut long_with_risk = "a".repeat(300);
        long_with_risk.push_str(" The main risk is data loss if the migration fails.");
        let verdicts_r = run_all_patterns(&[0.5, 0.6], 5, &long_with_risk);
        let omitted_r = verdicts_r
            .iter()
            .find(|v| v.pattern == AdversaryPattern::OmittedRisks)
            .expect("OmittedRisks verdict must be present");
        assert!(
            !omitted_r.fired,
            "omitted risks should NOT fire when risk tokens are present, detail={}",
            omitted_r.detail
        );

        // Short proposal with no risk language → does NOT fire:
        // short justifications legitimately have no risk section.
        let short = "too short to matter";
        let verdicts_s = run_all_patterns(&[0.5, 0.6], 5, short);
        let omitted_s = verdicts_s
            .iter()
            .find(|v| v.pattern == AdversaryPattern::OmittedRisks)
            .expect("OmittedRisks verdict must be present");
        assert!(
            !omitted_s.fired,
            "omitted risks should NOT fire on short provenance, detail={}",
            omitted_s.detail
        );
    }

    /// `UnverifiedClaims` fires when the provenance carries an
    /// attribution marker ("studies show", "experts say", ...)
    /// without naming a source. Pins the D.12.5 add-on contract.
    #[test]
    fn run_all_patterns_fires_unverified_claims() {
        let verdicts = run_all_patterns(
            &[0.5, 0.6],
            5,
            "Studies show that the failure rate is below 1%.",
        );
        let unverified = verdicts
            .iter()
            .find(|v| v.pattern == AdversaryPattern::UnverifiedClaims)
            .expect("UnverifiedClaims verdict must be present");
        assert!(
            unverified.fired,
            "unverified claims should fire on attribution marker, detail={}",
            unverified.detail
        );
        assert!(
            unverified.detail.contains("studies show"),
            "detail should name the matched token; got {}",
            unverified.detail
        );

        // Variant token: "reportedly".
        let verdicts_r = run_all_patterns(
            &[0.5, 0.6],
            5,
            "Reportedly, the upstream maintainer is unresponsive.",
        );
        let unverified_r = verdicts_r
            .iter()
            .find(|v| v.pattern == AdversaryPattern::UnverifiedClaims)
            .expect("UnverifiedClaims verdict must be present");
        assert!(unverified_r.fired);

        // Neutral provenance does not fire.
        let verdicts_neutral =
            run_all_patterns(&[0.5, 0.6], 5, "RFC 8259 mandates UTF-8 for JSON text.");
        let unverified_n = verdicts_neutral
            .iter()
            .find(|v| v.pattern == AdversaryPattern::UnverifiedClaims)
            .expect("UnverifiedClaims verdict must be present");
        assert!(!unverified_n.fired);
    }
}
