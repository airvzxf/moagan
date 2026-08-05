//! Constraints validator — checks that the brief's hard constraints
//! are reflected in the proposal.
//!
//! The validator scans the brief constraints (carried on every
//! [`Proposal`] as a list of strings via the intake + clarify pipeline)
//! and looks for evidence in the proposal's `approach`, `tradeoffs`,
//! and `evidence` fields. A constraint that appears nowhere produces
//! a `Warn` (the proposal might still be valid — the model just did
//! not echo the literal string back).
//!
//! Compliance: T01-06 §5.6 ("Restricciones duras no se compensan con
//! scores altos"). The proposal does not need to repeat the
//! constraint verbatim; the validator is a soft signal, not a gate.
//!
//! D.11.14: every constraint miss is emitted as a typed
//! [`FailureKind::HardConstraintMissing`] failure (with `Warn`
//! status so the deliver phase can still surface the proposal).

use crate::domain::Proposal;
use crate::error::Result;
use crate::sandbox::Sandbox;

use super::{FailureKind, ValidationEvidence, ValidationFailure, ValidationStatus, Validator};

/// Constraints validator. Stateless; reuse freely.
#[derive(Debug, Default, Clone, Copy)]
pub struct ConstraintsValidator;

impl ConstraintsValidator {
    /// Build a new instance.
    pub fn new() -> Self {
        Self
    }

    /// Inspect the proposal against the hard constraints advertised
    /// by the brief.
    pub fn check(proposal: &Proposal, brief_constraints: &[String]) -> ValidationEvidence {
        Self::check_inner(proposal, brief_constraints)
    }

    /// Same as [`Self::check`] but reads the constraint slice from the
    /// canonical [`Brief`](crate::domain::Brief). The validate phase
    /// uses this entry point so the brief constraints are wired into
    /// the real check instead of the no-op trait path.
    pub fn check_with_brief(
        proposal: &Proposal,
        brief: &crate::domain::Brief,
    ) -> ValidationEvidence {
        Self::check_inner(proposal, &brief.constraints)
    }

    fn check_inner(proposal: &Proposal, brief_constraints: &[String]) -> ValidationEvidence {
        let mut evidence = ValidationEvidence {
            validator: "constraints".into(),
            status: ValidationStatus::Pass,
            ..ValidationEvidence::default()
        };

        if brief_constraints.is_empty() {
            evidence.checks_run.push("no_constraints_in_brief".into());
            return evidence;
        }

        let haystack = build_haystack(proposal);
        for constraint in brief_constraints {
            let needle = constraint.trim();
            if needle.is_empty() {
                continue;
            }
            let label = format!("constraint: {needle}");
            if haystack.contains(&needle.to_lowercase()) {
                evidence.checks_run.push(label);
            } else {
                if evidence.status == ValidationStatus::Pass {
                    evidence.status = ValidationStatus::Warn;
                }
                evidence.record_failure(
                    ValidationFailure::new(
                        FailureKind::HardConstraintMissing,
                        format!("{label} not echoed in proposal"),
                    )
                    .with_field("approach"),
                );
            }
        }

        evidence
    }
}

/// Build the lower-cased haystack against which constraints are
/// matched. Concatenating avoids the borrow conflict of building a
/// single mega-`String`.
fn build_haystack(proposal: &Proposal) -> String {
    let mut buf = String::new();
    buf.push_str(&proposal.summary.to_lowercase());
    buf.push('\n');
    buf.push_str(&proposal.approach.to_lowercase());
    buf.push('\n');
    for t in &proposal.tradeoffs {
        buf.push_str(&t.to_lowercase());
        buf.push('\n');
    }
    for e in &proposal.evidence {
        buf.push_str(&e.to_lowercase());
        buf.push('\n');
    }
    buf
}

impl Validator for ConstraintsValidator {
    fn name(&self) -> &'static str {
        "constraints"
    }

    fn validate(
        &self,
        proposal: &Proposal,
        _sandbox: Option<&Sandbox>,
    ) -> Result<ValidationEvidence> {
        // The trait path stays as a no-op Pass; the validate phase
        // calls `check_with_brief` directly so the real brief
        // constraints reach the checker.
        Ok(Self::check(proposal, &[]))
    }
}

impl ConstraintsValidator {
    /// Static form of [`Validator::name`] for call sites that need
    /// the literal without instantiating the validator (e.g. the
    /// validate phase dispatches on it to choose the brief-aware
    /// entry point).
    pub fn name_static() -> &'static str {
        "constraints"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal_with(text: &str) -> Proposal {
        Proposal {
            id: "p_001".into(),
            summary: "summary".into(),
            approach: text.into(),
            evidence: vec![],
            tradeoffs: vec![],
            ..Proposal::default()
        }
    }

    #[test]
    fn empty_constraints_short_circuits() {
        let p = proposal_with("anything");
        let e = ConstraintsValidator::check(&p, &[]);
        assert_eq!(e.status, ValidationStatus::Pass);
        assert_eq!(e.checks_run, vec!["no_constraints_in_brief"]);
    }

    #[test]
    fn matched_constraint_is_recorded_as_run() {
        let p = proposal_with("Rust + tokio, no serverless");
        let e = ConstraintsValidator::check(&p, &["no serverless".into()]);
        assert_eq!(e.status, ValidationStatus::Pass);
        assert_eq!(e.checks_run.len(), 1);
        assert!(e.checks_run[0].contains("no serverless"));
    }

    #[test]
    fn unmatched_constraint_is_warn() {
        let p = proposal_with("Single Rust binary with SQLite.");
        let e = ConstraintsValidator::check(&p, &["no serverless".into()]);
        assert_eq!(e.status, ValidationStatus::Warn);
        assert!(e.failed_checks[0].contains("no serverless"));
    }

    #[test]
    fn case_insensitive_match() {
        let p = proposal_with("NO SERVERLESS deployment");
        let e = ConstraintsValidator::check(&p, &["no serverless".into()]);
        assert_eq!(e.status, ValidationStatus::Pass);
    }

    #[test]
    fn empty_constraint_strings_are_skipped() {
        let p = proposal_with("anything");
        let e = ConstraintsValidator::check(&p, &["".into(), "   ".into()]);
        assert_eq!(e.status, ValidationStatus::Pass);
        assert!(e.checks_run.is_empty());
    }

    #[test]
    fn check_with_brief_propagates_constraints() {
        use crate::domain::Brief;
        let p = proposal_with("Rust + tokio, no serverless");
        let brief = Brief {
            constraints: vec!["no serverless".into(), "Rust".into()],
            ..Brief::default()
        };
        let e = ConstraintsValidator::check_with_brief(&p, &brief);
        assert_eq!(e.status, ValidationStatus::Pass);
        assert_eq!(e.checks_run.len(), 2);
        assert!(e.checks_run.iter().any(|c| c.contains("no serverless")));
        assert!(e.checks_run.iter().any(|c| c.contains("Rust")));
    }

    #[test]
    fn check_with_brief_records_missing_constraint_as_warn() {
        use crate::domain::Brief;
        let p = proposal_with("Single Rust binary with SQLite.");
        let brief = Brief {
            constraints: vec!["no serverless".into()],
            ..Brief::default()
        };
        let e = ConstraintsValidator::check_with_brief(&p, &brief);
        assert_eq!(e.status, ValidationStatus::Warn);
        assert!(e.failed_checks[0].contains("no serverless"));
    }

    #[test]
    fn name_static_matches_trait_name() {
        assert_eq!(ConstraintsValidator::name_static(), "constraints");
        assert_eq!(ConstraintsValidator::new().name(), "constraints");
    }
}
