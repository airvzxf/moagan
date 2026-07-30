//! Structural validator — checks that a proposal satisfies the
//! minimum contract every mode requires (T01-06 §5.7 + §5.8).
//!
//! The structural check is intentionally cheap: it never spawns a
//! process and never calls an LLM. Anything that depends on a tool
//! (cargo, python, tsc) belongs in a language-specific validator.

use crate::domain::Proposal;
use crate::error::Result;
use crate::sandbox::Sandbox;

use super::{ValidationEvidence, ValidationStatus, Validator};

/// Structural validator. Stateless; reuse freely.
#[derive(Debug, Default, Clone, Copy)]
pub struct StructuralValidator;

impl StructuralValidator {
    /// Build a new instance.
    pub fn new() -> Self {
        Self
    }

    /// Inspect a proposal and return the verdict. Exposed so the
    /// pipeline can run the structural check first and short-circuit
    /// the expensive validators on `Fail`.
    pub fn check(proposal: &Proposal) -> ValidationEvidence {
        let mut evidence = ValidationEvidence {
            validator: "structural".into(),
            status: ValidationStatus::Pass,
            ..ValidationEvidence::default()
        };

        if proposal.id.trim().is_empty() {
            evidence.status = ValidationStatus::Fail;
            evidence.failed_checks.push("missing id".into());
        } else {
            evidence.checks_run.push("id".into());
        }

        if proposal.summary.trim().is_empty() {
            evidence.status = ValidationStatus::Fail;
            evidence.failed_checks.push("missing summary".into());
        } else if proposal.summary.len() < 20 {
            if evidence.status == ValidationStatus::Pass {
                evidence.status = ValidationStatus::Warn;
            }
            evidence
                .failed_checks
                .push("summary shorter than 20 chars".into());
        } else {
            evidence.checks_run.push("summary".into());
        }

        if proposal.approach.trim().is_empty() {
            evidence.status = ValidationStatus::Fail;
            evidence.failed_checks.push("missing approach".into());
        } else {
            evidence.checks_run.push("approach".into());
        }

        if proposal.evidence.is_empty() {
            if evidence.status == ValidationStatus::Pass {
                evidence.status = ValidationStatus::Warn;
            }
            evidence.failed_checks.push("no evidence provided".into());
        } else {
            evidence.checks_run.push("evidence".into());
        }

        if proposal.tradeoffs.is_empty() {
            if evidence.status == ValidationStatus::Pass {
                evidence.status = ValidationStatus::Warn;
            }
            evidence.failed_checks.push("no tradeoffs listed".into());
        } else {
            evidence.checks_run.push("tradeoffs".into());
        }

        evidence
    }
}

impl Validator for StructuralValidator {
    fn name(&self) -> &'static str {
        "structural"
    }

    fn validate(
        &self,
        proposal: &Proposal,
        _sandbox: Option<&Sandbox>,
    ) -> Result<ValidationEvidence> {
        Ok(Self::check(proposal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal() -> Proposal {
        Proposal {
            id: "p_001".into(),
            summary: "A reasonably long summary that satisfies the length floor.".into(),
            approach: "Use a Rust crate with tokio and rusqlite.".into(),
            evidence: vec!["measured 124 ms p99 in a synthetic run".into()],
            tradeoffs: vec!["single-process model".into()],
            source_sketch: "sk_001".into(),
            artifacts: vec![],
            replaced_by: None,
        }
    }

    #[test]
    fn full_proposal_passes() {
        let e = StructuralValidator::check(&proposal());
        assert_eq!(e.status, ValidationStatus::Pass);
        assert!(e.failed_checks.is_empty());
        assert_eq!(e.checks_run.len(), 5);
    }

    #[test]
    fn missing_id_is_fail() {
        let mut p = proposal();
        p.id.clear();
        let e = StructuralValidator::check(&p);
        assert_eq!(e.status, ValidationStatus::Fail);
        assert!(e.failed_checks.iter().any(|f| f.contains("id")));
    }

    #[test]
    fn short_summary_is_warn() {
        let mut p = proposal();
        p.summary = "short".into();
        let e = StructuralValidator::check(&p);
        assert_eq!(e.status, ValidationStatus::Warn);
        assert!(e.failed_checks.iter().any(|f| f.contains("summary")));
    }

    #[test]
    fn missing_evidence_is_warn() {
        let mut p = proposal();
        p.evidence.clear();
        let e = StructuralValidator::check(&p);
        assert_eq!(e.status, ValidationStatus::Warn);
        assert!(e.failed_checks.iter().any(|f| f.contains("evidence")));
    }

    #[test]
    fn missing_approach_is_fail() {
        let mut p = proposal();
        p.approach.clear();
        let e = StructuralValidator::check(&p);
        assert_eq!(e.status, ValidationStatus::Fail);
        assert!(e.failed_checks.iter().any(|f| f.contains("approach")));
    }

    #[test]
    fn validator_trait_returns_same_as_check() {
        let v = StructuralValidator::new();
        let e = v.validate(&proposal(), None).unwrap();
        assert_eq!(e.status, ValidationStatus::Pass);
    }
}
