//! Validators — evidence-based checks for proposals.
//!
//! A validator inspects a [`Proposal`] and returns a
//! [`ValidationEvidence`] with a verdict plus the evidence used to
//! reach it. Validators compose: the language-specific validators
//! (`RustValidator`, `PythonValidator`, `TypeScriptValidator`,
//! `SchemaValidator`) execute code in a [`Sandbox`] and inherit
//! from the structural / constraints checks defined here.
//!
//! Compliance: `proposal-02-rust.md` §5.7 + §5.8.

pub mod constraints;
pub mod rust_validator;
pub mod structural;

pub use constraints::ConstraintsValidator;
pub use rust_validator::RustValidator;
pub use structural::StructuralValidator;

/// A single piece of code attached to a proposal. The pipeline uses
/// `language` to pick the right `CodeValidator`; `kind` is a free-form
/// label (`"src/lib.rs"`, `"tests/smoke.py"`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeArtifact {
    /// Logical name of the artifact (file path relative to the
    /// proposal, or any human-readable identifier).
    pub kind: String,
    /// Programming language (`"rust"`, `"python"`, `"typescript"`,
    /// `"sql"`, ...). Used to dispatch to the right validator.
    pub language: String,
    /// The actual source code. Validators feed this into the
    /// sandboxed compiler / linter.
    pub source: String,
}

impl CodeArtifact {
    /// Build a new artifact from its three components.
    pub fn new(
        kind: impl Into<String>,
        language: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            kind: kind.into(),
            language: language.into(),
            source: source.into(),
        }
    }
}

use crate::domain::Proposal;
use crate::error::Result;
use crate::sandbox::Sandbox;

/// Verdict a validator returns for a single proposal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ValidationStatus {
    /// All hard checks passed. Default variant so a freshly
    /// constructed empty evidence never reads as `Error`.
    #[default]
    Pass,
    /// Soft checks flagged but no hard failure.
    Warn,
    /// At least one hard check failed.
    Fail,
    /// The check was skipped (e.g. tool missing, not applicable).
    Skipped,
    /// The validator could not run (e.g. I/O error, parse failure).
    Error,
}

impl ValidationStatus {
    /// Default verdict is `Pass` so a fresh empty evidence struct
    /// never accidentally reads as `Error`.
    pub const DEFAULT: ValidationStatus = ValidationStatus::Pass;

    /// String form used in JSON serialisation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
            Self::Error => "error",
        }
    }
}

/// Aggregated evidence returned by every validator.
#[derive(Debug, Clone, Default)]
pub struct ValidationEvidence {
    /// Validator name (e.g. `"structural"`, `"rust"`).
    pub validator: String,
    /// Verdict.
    pub status: ValidationStatus,
    /// Names of checks that were run.
    pub checks_run: Vec<String>,
    /// Names of checks that were skipped and the reason.
    pub skipped_checks: Vec<String>,
    /// Failures with one-line explanations.
    pub failed_checks: Vec<String>,
    /// The command that produced the evidence (language validators
    /// only). `None` for pure structural / constraints checks.
    pub command: Option<String>,
    /// Process exit code (language validators only).
    pub exit_code: Option<i32>,
    /// Last lines of stdout (truncated to fit).
    pub stdout_summary: String,
    /// Last lines of stderr (truncated to fit).
    pub stderr_summary: String,
    /// Free-form reproducibility data (tool version, hashes, etc.).
    pub reproducibility: Vec<(String, String)>,
}

impl ValidationEvidence {
    /// Build a `Pass` evidence with a single check name.
    pub fn pass(validator: impl Into<String>, check: impl Into<String>) -> Self {
        Self {
            validator: validator.into(),
            status: ValidationStatus::Pass,
            checks_run: vec![check.into()],
            ..Self::default()
        }
    }

    /// Build a `Fail` evidence with a failure message.
    pub fn fail(validator: impl Into<String>, failure: impl Into<String>) -> Self {
        Self {
            validator: validator.into(),
            status: ValidationStatus::Fail,
            failed_checks: vec![failure.into()],
            ..Self::default()
        }
    }

    /// Build a `Skipped` evidence with a reason.
    pub fn skipped(validator: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            validator: validator.into(),
            status: ValidationStatus::Skipped,
            skipped_checks: vec![reason.into()],
            ..Self::default()
        }
    }
}

/// Common interface every validator implements.
pub trait Validator: Send + Sync {
    /// Stable name (used in `manifest.json` and as `evidence.validator`).
    fn name(&self) -> &'static str;

    /// Run the validator against `proposal`. The sandbox is optional
    /// because pure structural / constraints validators never spawn
    /// processes; language validators must request it.
    fn validate(
        &self,
        proposal: &Proposal,
        sandbox: Option<&Sandbox>,
    ) -> Result<ValidationEvidence>;
}

/// Compose several validators into one. The aggregated status follows
/// T01-06 §5.7 rules: any `Fail` collapses to `Fail`, otherwise any
/// `Warn` becomes `Warn`, otherwise `Pass`. `Skipped` and `Error`
/// votes are recorded but do not dominate a `Pass` / `Warn` (the
/// caller inspects `skipped_checks` / `failed_checks` for detail).
pub struct CompositeValidator {
    validators: Vec<Box<dyn Validator>>,
}

impl CompositeValidator {
    /// Build an empty composite.
    pub fn new() -> Self {
        Self {
            validators: Vec::new(),
        }
    }

    /// Append a validator.
    pub fn with<V: Validator + 'static>(mut self, v: V) -> Self {
        self.validators.push(Box::new(v));
        self
    }

    /// Run every validator sequentially and aggregate.
    pub fn run(
        &self,
        proposal: &Proposal,
        sandbox: Option<&Sandbox>,
    ) -> Result<Vec<ValidationEvidence>> {
        let mut out = Vec::with_capacity(self.validators.len());
        for v in &self.validators {
            out.push(v.validate(proposal, sandbox)?);
        }
        Ok(out)
    }

    /// Run every validator and reduce to the worst verdict.
    pub fn aggregate(
        &self,
        proposal: &Proposal,
        sandbox: Option<&Sandbox>,
    ) -> Result<ValidationEvidence> {
        let evidences = self.run(proposal, sandbox)?;
        let mut aggregate = ValidationEvidence {
            validator: "composite".into(),
            status: ValidationStatus::Pass,
            checks_run: Vec::new(),
            skipped_checks: Vec::new(),
            failed_checks: Vec::new(),
            ..ValidationEvidence::default()
        };
        for ev in evidences {
            aggregate
                .checks_run
                .push(format!("{}:{}", ev.validator, ev.status.as_str()));
            aggregate
                .skipped_checks
                .extend(ev.skipped_checks.iter().cloned());
            aggregate
                .failed_checks
                .extend(ev.failed_checks.iter().cloned());
            aggregate.status = match (aggregate.status, ev.status) {
                (_, ValidationStatus::Fail) | (ValidationStatus::Fail, _) => ValidationStatus::Fail,
                (ValidationStatus::Pass, ValidationStatus::Warn)
                | (ValidationStatus::Warn, ValidationStatus::Warn) => ValidationStatus::Warn,
                (cur, ValidationStatus::Pass) => cur,
                (cur, other) => match cur {
                    ValidationStatus::Pass => other,
                    _ => cur,
                },
            };
        }
        Ok(aggregate)
    }
}

impl Default for CompositeValidator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p_with_summary(s: &str) -> Proposal {
        Proposal {
            summary: s.into(),
            approach: "approach".into(),
            evidence: vec!["e".into()],
            tradeoffs: vec!["t".into()],
            ..Proposal::default()
        }
    }

    #[test]
    fn validation_status_as_str() {
        assert_eq!(ValidationStatus::Pass.as_str(), "pass");
        assert_eq!(ValidationStatus::Warn.as_str(), "warn");
        assert_eq!(ValidationStatus::Fail.as_str(), "fail");
        assert_eq!(ValidationStatus::Skipped.as_str(), "skipped");
        assert_eq!(ValidationStatus::Error.as_str(), "error");
    }

    #[test]
    fn evidence_pass_skips_everything() {
        let e = ValidationEvidence::pass("x", "y");
        assert_eq!(e.status, ValidationStatus::Pass);
        assert_eq!(e.checks_run, vec!["y"]);
        assert!(e.failed_checks.is_empty());
    }

    #[test]
    fn evidence_fail_records_failure() {
        let e = ValidationEvidence::fail("x", "boom");
        assert_eq!(e.status, ValidationStatus::Fail);
        assert_eq!(e.failed_checks, vec!["boom"]);
    }

    #[test]
    fn evidence_skipped_records_reason() {
        let e = ValidationEvidence::skipped("x", "no binary");
        assert_eq!(e.status, ValidationStatus::Skipped);
        assert_eq!(e.skipped_checks, vec!["no binary"]);
    }

    /// A composite must demote to Fail whenever any child reports
    /// Fail, regardless of order.
    #[test]
    fn composite_demotes_to_fail() {
        struct Fail;
        impl Validator for Fail {
            fn name(&self) -> &'static str {
                "fail"
            }
            fn validate(&self, _: &Proposal, _: Option<&Sandbox>) -> Result<ValidationEvidence> {
                Ok(ValidationEvidence::fail("fail", "x"))
            }
        }
        struct Warn;
        impl Validator for Warn {
            fn name(&self) -> &'static str {
                "warn"
            }
            fn validate(&self, _: &Proposal, _: Option<&Sandbox>) -> Result<ValidationEvidence> {
                let mut e = ValidationEvidence::pass("warn", "ok");
                e.status = ValidationStatus::Warn;
                e.failed_checks.push("soft".into());
                Ok(e)
            }
        }
        let c = CompositeValidator::new().with(Warn).with(Fail);
        let agg = c.aggregate(&p_with_summary("x"), None).unwrap();
        assert_eq!(agg.status, ValidationStatus::Fail);
    }

    /// Warn-only composite must report Warn and keep the failure
    /// messages visible.
    #[test]
    fn composite_warn_only_is_warn() {
        struct Warn;
        impl Validator for Warn {
            fn name(&self) -> &'static str {
                "warn"
            }
            fn validate(&self, _: &Proposal, _: Option<&Sandbox>) -> Result<ValidationEvidence> {
                let mut e = ValidationEvidence::pass("warn", "ok");
                e.status = ValidationStatus::Warn;
                e.failed_checks.push("soft".into());
                Ok(e)
            }
        }
        let c = CompositeValidator::new().with(Warn);
        let agg = c.aggregate(&p_with_summary("x"), None).unwrap();
        assert_eq!(agg.status, ValidationStatus::Warn);
        assert_eq!(agg.failed_checks, vec!["soft"]);
    }
}
