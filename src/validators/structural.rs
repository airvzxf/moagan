//! Structural validator — checks that a proposal satisfies the
//! minimum contract every mode requires (T01-06 §5.7 + §5.8).
//!
//! The structural check is intentionally cheap: it never spawns a
//! process and never calls an LLM. Anything that depends on a tool
//! (cargo, python, tsc) belongs in a language-specific validator.
//!
//! D.11.14: every failure the structural validator emits carries a
//! typed [`FailureKind`]. The legacy `failed_checks` strings are
//! derived from the typed list and stay byte-identical so the
//! existing pipeline tests keep passing.

use crate::domain::Proposal;
use crate::error::Result;
use crate::sandbox::Sandbox;

use super::{FailureKind, ValidationEvidence, ValidationFailure, ValidationStatus, Validator};

/// Tokens that, when found in the proposal summary or approach,
/// trigger a [`FailureKind::ForbiddenTech`] failure. The list is
/// deliberately small and conservative — only technologies that are
/// universally out of scope for the moagan pipeline (cluster
/// orchestrators the tool itself does not support).
const FORBIDDEN_TECH_TOKENS: &[&str] = &["kubernetes", " k8s ", "docker swarm", "cassandra"];

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
            evidence.record_failure(
                ValidationFailure::new(FailureKind::MissingField, "missing id").with_field("id"),
            );
        } else {
            evidence.checks_run.push("id".into());
        }

        if proposal.summary.trim().is_empty() {
            evidence.status = ValidationStatus::Fail;
            evidence.record_failure(
                ValidationFailure::new(FailureKind::MissingField, "missing summary")
                    .with_field("summary"),
            );
        } else if proposal.summary.len() < 20 {
            if evidence.status == ValidationStatus::Pass {
                evidence.status = ValidationStatus::Warn;
            }
            evidence.record_failure(
                ValidationFailure::new(
                    FailureKind::LengthOutOfRange,
                    "summary shorter than 20 chars",
                )
                .with_field("summary"),
            );
        } else {
            evidence.checks_run.push("summary".into());
        }

        if proposal.approach.trim().is_empty() {
            evidence.status = ValidationStatus::Fail;
            evidence.record_failure(
                ValidationFailure::new(FailureKind::MissingField, "missing approach")
                    .with_field("approach"),
            );
        } else {
            evidence.checks_run.push("approach".into());
        }

        if proposal.evidence.is_empty() {
            if evidence.status == ValidationStatus::Pass {
                evidence.status = ValidationStatus::Warn;
            }
            evidence.record_failure(
                ValidationFailure::new(FailureKind::SoftWarning, "no evidence provided")
                    .with_field("evidence"),
            );
        } else {
            evidence.checks_run.push("evidence".into());
        }

        if proposal.tradeoffs.is_empty() {
            if evidence.status == ValidationStatus::Pass {
                evidence.status = ValidationStatus::Warn;
            }
            evidence.record_failure(
                ValidationFailure::new(FailureKind::SoftWarning, "no tradeoffs listed")
                    .with_field("tradeoffs"),
            );
        } else {
            evidence.checks_run.push("tradeoffs".into());
        }

        // Forbidden-tech scan over the proposal text. A hit turns
        // the verdict into Fail and emits a typed
        // `ForbiddenTech` failure pointing at the `approach`
        // field (or `summary` if the hit only appears there).
        let scan = format!(
            " {} {} ",
            proposal.summary.to_lowercase(),
            proposal.approach.to_lowercase()
        );
        for token in FORBIDDEN_TECH_TOKENS {
            let needle = token.to_lowercase();
            if scan.contains(&needle) {
                evidence.status = ValidationStatus::Fail;
                let field = if proposal.approach.to_lowercase().contains(&needle) {
                    "approach"
                } else {
                    "summary"
                };
                evidence.record_failure(
                    ValidationFailure::new(
                        FailureKind::ForbiddenTech,
                        format!("proposal mentions forbidden technology '{token}'"),
                    )
                    .with_field(field)
                    .with_hint("remove the technology mention or scope the proposal tighter"),
                );
            }
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
            source_nodes: Vec::new(),
        }
    }

    #[test]
    fn full_proposal_passes() {
        let e = StructuralValidator::check(&proposal());
        assert_eq!(e.status, ValidationStatus::Pass);
        assert!(e.failures.is_empty());
        assert_eq!(e.checks_run.len(), 5);
    }

    #[test]
    fn missing_id_is_fail() {
        let mut p = proposal();
        p.id.clear();
        let e = StructuralValidator::check(&p);
        assert_eq!(e.status, ValidationStatus::Fail);
        let legacy = e.legacy_failed_checks();
        assert!(legacy.iter().any(|f| f.contains("id")));
    }

    #[test]
    fn short_summary_is_warn() {
        let mut p = proposal();
        p.summary = "short".into();
        let e = StructuralValidator::check(&p);
        assert_eq!(e.status, ValidationStatus::Warn);
        let legacy = e.legacy_failed_checks();
        assert!(legacy.iter().any(|f| f.contains("summary")));
    }

    #[test]
    fn missing_evidence_is_warn() {
        let mut p = proposal();
        p.evidence.clear();
        let e = StructuralValidator::check(&p);
        assert_eq!(e.status, ValidationStatus::Warn);
        let legacy = e.legacy_failed_checks();
        assert!(legacy.iter().any(|f| f.contains("evidence")));
    }

    #[test]
    fn missing_approach_is_fail() {
        let mut p = proposal();
        p.approach.clear();
        let e = StructuralValidator::check(&p);
        assert_eq!(e.status, ValidationStatus::Fail);
        let legacy = e.legacy_failed_checks();
        assert!(legacy.iter().any(|f| f.contains("approach")));
    }

    #[test]
    fn validator_trait_returns_same_as_check() {
        let v = StructuralValidator::new();
        let e = v.validate(&proposal(), None).unwrap();
        assert_eq!(e.status, ValidationStatus::Pass);
    }
}
