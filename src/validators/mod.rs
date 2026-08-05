//! Validators — evidence-based checks for proposals.
//!
//! A validator inspects a [`Proposal`] and returns a
//! [`ValidationEvidence`] with a verdict plus the evidence used to
//! reach it. Validators compose: the language-specific validators
//! (`RustValidator`, `PythonValidator`, `TypeScriptValidator`,
//! `SchemaValidator`) execute code in a [`Sandbox`] and inherit
//! from the structural / constraints checks defined here.
//!
//! D.11.14: every hard failure now carries a typed
//! [`FailureKind`] in [`ValidationFailure`]. The legacy
//! `failed_checks: Vec<String>` view is derived from the typed
//! list so external callers (and the validate phase) keep working
//! unchanged. `ValidationEvidence::outcome()` projects the typed
//! list into a [`ValidationOutcome`].
//!
//! Compliance: `proposal-02-rust.md` §5.7 + §5.8.

pub mod constraints;
pub mod python_validator;
pub mod rust_validator;
pub mod schema_validator;
pub mod sql_validator;
pub mod structural;
pub mod typescript_validator;

pub use constraints::ConstraintsValidator;
pub use python_validator::PythonValidator;
pub use rust_validator::RustValidator;
pub use schema_validator::SchemaValidator;
pub use sql_validator::SqlValidator;
pub use structural::StructuralValidator;
pub use typescript_validator::TypeScriptValidator;

/// A single piece of code attached to a proposal. The pipeline uses
/// `language` to pick the right `CodeValidator`; `kind` is a free-form
/// label (`"src/lib.rs"`, `"tests/smoke.py"`, ...).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
    /// D.11.14: typed view of every failure emitted by the validator.
    /// `failed_checks` is the legacy string projection of this list
    /// (one entry per failure, message only); `failures` carries the
    /// full [`ValidationFailure`] record (kind, field, line, column,
    /// hint). New code should read `failures`; legacy callers that
    /// only know about `failed_checks` continue to work unchanged.
    #[serde(default)]
    pub failures: Vec<ValidationFailure>,
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

    /// Record a typed failure. Pushes to `failures` (typed view) and
    /// `failed_checks` (legacy string view). The message becomes the
    /// legacy string; the typed record carries the structured data.
    pub fn record_failure(&mut self, failure: ValidationFailure) {
        self.failed_checks.push(failure.message.clone());
        self.failures.push(failure);
    }

    /// Project the typed view into a [`ValidationOutcome`]. A `Fail`
    /// verdict with at least one typed failure becomes
    /// `ValidationOutcome::Fail { failures }`; everything else
    /// collapses to `ValidationOutcome::Ok` (so `Warn` and `Skipped`
    /// never produce a Fail outcome — soft signals stay soft).
    pub fn outcome(&self) -> ValidationOutcome {
        if self.status == ValidationStatus::Fail && !self.failures.is_empty() {
            ValidationOutcome::Fail {
                failures: self.failures.clone(),
            }
        } else {
            ValidationOutcome::Ok
        }
    }
}

/// Typed failure kind — D.11.14. Every hard failure a validator
/// surfaces maps to exactly one variant; the variant name is the
/// stable wire form (snake_case in JSON / `as_str`).
///
/// The enum is exhaustive: when a validator reports a new kind of
/// failure, add the variant here first and let the compiler pin the
/// migration. `FailureKind::from_str` rejects unknown names so
/// legacy sidecars cannot smuggle in bogus kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// A required field was empty or absent (e.g. missing `id`,
    /// missing `summary`, missing `approach`).
    MissingField,
    /// The output was truncated mid-stream by the toolchain.
    Truncation,
    /// The artifact is a placeholder (e.g. a `TODO` comment, an
    /// empty file with a stub).
    Placeholder,
    /// A fenced code block was opened but never closed (or vice
    /// versa).
    UnbalancedFences,
    /// The proposal mentions technology that is forbidden in the
    /// current context (e.g. `kubernetes` when the brief forbids
    /// orchestrators).
    ForbiddenTech,
    /// The brief declared a deliverable that the proposal does not
    /// address.
    MissingDeliverable,
    /// A hard constraint from the brief is missing from the
    /// proposal.
    HardConstraintMissing,
    /// A soft signal that does not fail the verdict (kept in
    /// `failures` so the report can surface it; the status stays
    /// `Warn`).
    SoftWarning,
    /// The proposal contains non-ASCII characters where the brief
    /// required plain ASCII.
    AsciiMismatch,
    /// The proposal contradicts itself in obvious, recoverable
    /// ways (e.g. `id` declared twice).
    TrivialContradiction,
    /// The proposal dodges the question without addressing it.
    EvasiveAnswer,
    /// A field is outside its allowed length range (too short or too
    /// long).
    LengthOutOfRange,
    /// `cargo check` / `cargo fmt` / `cargo clippy` exited non-zero.
    RustCompileError,
    /// `cargo test` exited non-zero (build passed, tests failed).
    RustTestFailure,
    /// `python3 -m py_compile` exited non-zero.
    PythonSyntaxError,
    /// `tsc --noEmit` exited non-zero.
    TypeScriptCompileError,
    /// The hand-written SQL parser rejected the statement.
    SqlSyntaxError,
    /// The parser accepted the statement but the dialect check
    /// rejected it (e.g. `SERIAL` in MySQL), or the SQLite engine
    /// refused to execute it.
    SqlSemanticError,
    /// A data document failed to validate against the schema, the
    /// schema itself is not a valid JSON Schema, or the inline
    /// bundle is missing `schema` / `data` fields.
    SchemaViolation,
    /// A constraint is structurally unsatisfiable (e.g. requires
    /// `>= 10GiB RAM` on a host with 2GiB).
    ConstraintUnsatisfiable,
    /// The validator could not capture the tool version needed for
    /// reproducibility (e.g. `cargo --version` failed).
    ReproducibilityMissing,
}

impl FailureKind {
    /// Stable snake_case wire form. Same as the JSON serialised
    /// form (serde uses `rename_all = "snake_case"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingField => "missing_field",
            Self::Truncation => "truncation",
            Self::Placeholder => "placeholder",
            Self::UnbalancedFences => "unbalanced_fences",
            Self::ForbiddenTech => "forbidden_tech",
            Self::MissingDeliverable => "missing_deliverable",
            Self::HardConstraintMissing => "hard_constraint_missing",
            Self::SoftWarning => "soft_warning",
            Self::AsciiMismatch => "ascii_mismatch",
            Self::TrivialContradiction => "trivial_contradiction",
            Self::EvasiveAnswer => "evasive_answer",
            Self::LengthOutOfRange => "length_out_of_range",
            Self::RustCompileError => "rust_compile_error",
            Self::RustTestFailure => "rust_test_failure",
            Self::PythonSyntaxError => "python_syntax_error",
            Self::TypeScriptCompileError => "typescript_compile_error",
            Self::SqlSyntaxError => "sql_syntax_error",
            Self::SqlSemanticError => "sql_semantic_error",
            Self::SchemaViolation => "schema_violation",
            Self::ConstraintUnsatisfiable => "constraint_unsatisfiable",
            Self::ReproducibilityMissing => "reproducibility_missing",
        }
    }
}

impl std::fmt::Display for FailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for FailureKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "missing_field" => Ok(Self::MissingField),
            "truncation" => Ok(Self::Truncation),
            "placeholder" => Ok(Self::Placeholder),
            "unbalanced_fences" => Ok(Self::UnbalancedFences),
            "forbidden_tech" => Ok(Self::ForbiddenTech),
            "missing_deliverable" => Ok(Self::MissingDeliverable),
            "hard_constraint_missing" => Ok(Self::HardConstraintMissing),
            "soft_warning" => Ok(Self::SoftWarning),
            "ascii_mismatch" => Ok(Self::AsciiMismatch),
            "trivial_contradiction" => Ok(Self::TrivialContradiction),
            "evasive_answer" => Ok(Self::EvasiveAnswer),
            "length_out_of_range" => Ok(Self::LengthOutOfRange),
            "rust_compile_error" => Ok(Self::RustCompileError),
            "rust_test_failure" => Ok(Self::RustTestFailure),
            "python_syntax_error" => Ok(Self::PythonSyntaxError),
            "typescript_compile_error" => Ok(Self::TypeScriptCompileError),
            "sql_syntax_error" => Ok(Self::SqlSyntaxError),
            "sql_semantic_error" => Ok(Self::SqlSemanticError),
            "schema_violation" => Ok(Self::SchemaViolation),
            "constraint_unsatisfiable" => Ok(Self::ConstraintUnsatisfiable),
            "reproducibility_missing" => Ok(Self::ReproducibilityMissing),
            other => Err(format!("unknown FailureKind: '{other}'")),
        }
    }
}

impl From<FailureKind> for String {
    /// Convert to the snake_case wire form. The few legacy sinks
    /// that still expect a `String` (e.g. JSON sidecars carrying a
    /// `kind` field) get the same value serde would emit, so the
    /// shape stays consistent end-to-end.
    fn from(kind: FailureKind) -> Self {
        kind.as_str().to_owned()
    }
}

/// Typed record of a single validation failure (D.11.14). The
/// `kind` field discriminates the failure mode; the optional
/// `field`, `line`, `column`, and `hint` provide enough structure
/// for the deliver phase to point the user at the offending
/// location without re-parsing the proposal text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ValidationFailure {
    /// Typed kind — pick the closest variant. New code should match
    /// on this rather than parsing the message string.
    pub kind: FailureKind,
    /// Dotted path to the offending field when the failure is tied
    /// to a specific field (e.g. `"approach"`, `"evidence[2]"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// 1-based line number when the failure is anchored to a
    /// location inside an artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// 1-based column number paired with `line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    /// Human-readable one-liner. Legacy callers that only know
    /// about the string view see this message verbatim; new code
    /// can read it for display while relying on `kind` for
    /// dispatch.
    pub message: String,
    /// Optional remediation hint (e.g. `"add a Cargo.toml"`). The
    /// deliver phase may render this next to the failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl ValidationFailure {
    /// Build a failure with just the kind and a message.
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            field: None,
            line: None,
            column: None,
            message: message.into(),
            hint: None,
        }
    }

    /// Attach a field path to this failure.
    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// Attach a 1-based line/column pair to this failure.
    pub fn with_location(mut self, line: usize, column: usize) -> Self {
        self.line = Some(line);
        self.column = Some(column);
        self
    }

    /// Attach a remediation hint to this failure.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Typed verdict a validator exposes — D.11.14. The legacy
/// `ValidationEvidence` carries the full evidence trail
/// (command, stdout/stderr, reproducibility); the `ValidationOutcome`
/// is the slim, typed projection that callers that only care about
/// "did it pass and why not" should consume.
///
/// JSON form: `{"verdict": "ok"}` for pass, `{"verdict": "fail",
/// "failures": [...]}` for fail. The `Fail` variant always carries
/// at least one entry in `failures`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ValidationOutcome {
    /// All hard checks passed (or only soft warnings were flagged).
    Ok,
    /// At least one hard check failed. `failures` is non-empty.
    Fail {
        /// Typed failure records. Always populated.
        failures: Vec<ValidationFailure>,
    },
}

impl ValidationOutcome {
    /// Build a pass outcome.
    pub fn ok() -> Self {
        Self::Ok
    }

    /// Build a fail outcome from a typed failure list. Empty input
    /// collapses to `Ok` (an empty failure list is not a failure).
    pub fn fail(failures: Vec<ValidationFailure>) -> Self {
        if failures.is_empty() {
            Self::Ok
        } else {
            Self::Fail { failures }
        }
    }

    /// True when the outcome is `Ok`.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// Borrow the failure list, or an empty slice when `Ok`.
    pub fn failures(&self) -> &[ValidationFailure] {
        match self {
            Self::Ok => &[],
            Self::Fail { failures } => failures,
        }
    }
}

/// Run `<tool> --version` inside the sandbox and return the captured
/// version string trimmed of trailing whitespace. Returns `None` when
/// the tool is missing on disk, not in the allowlist, or returns
/// non-zero. The result is safe to embed in `reproducibility_data` so
/// the deliver phase can show "validated with cargo 1.97.1" without
/// having to re-run anything.
pub async fn capture_tool_version(sandbox: &Sandbox, tool: &str) -> Option<String> {
    let result = sandbox.run(tool, &["--version"]).await.ok()?;
    if result.status != crate::sandbox::SandboxStatus::Pass {
        return None;
    }
    let first_line = result.stdout.lines().next()?.trim();
    if first_line.is_empty() {
        return None;
    }
    Some(first_line.to_owned())
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

    #[test]
    fn reproducibility_field_round_trips_through_json() {
        let mut e = ValidationEvidence::pass("x", "y");
        e.reproducibility
            .push(("cargo".into(), "cargo 1.97.1".into()));
        e.reproducibility
            .push(("python3".into(), "Python 3.14.6".into()));
        let j = serde_json::to_string(&e).unwrap();
        let back: ValidationEvidence = serde_json::from_str(&j).unwrap();
        assert_eq!(back.reproducibility.len(), 2);
        assert_eq!(back.reproducibility[0].0, "cargo");
        assert_eq!(back.reproducibility[1].0, "python3");
    }

    #[tokio::test]
    async fn capture_tool_version_returns_first_line_for_present_binary() {
        use crate::sandbox::{Sandbox, SandboxConfig};
        let sandbox = Sandbox::new(SandboxConfig::new()).unwrap();
        // `echo` is on the default allowlist and writes its argv to
        // stdout, which is not exactly `--version` output but proves
        // the helper reads the first line.
        let v = capture_tool_version(&sandbox, "echo").await;
        assert!(v.is_some());
        assert!(v.unwrap().contains("echo"));
    }

    #[tokio::test]
    async fn capture_tool_version_returns_none_for_missing_binary() {
        use crate::sandbox::{Allowlist, Sandbox, SandboxConfig};
        // Allowlist a name that does not exist on disk so the helper
        // exercises the NotFound branch instead of the allowlist
        // branch.
        let cfg =
            SandboxConfig::new().with_allowlist(Allowlist::from_slice(["moagan-no-such-tool-xyz"]));
        let sandbox = Sandbox::new(cfg).unwrap();
        let v = capture_tool_version(&sandbox, "moagan-no-such-tool-xyz").await;
        assert!(v.is_none());
    }

    // =================================================================
    // D.11.14 — typed FailureKind / ValidationFailure / ValidationOutcome
    // =================================================================

    fn p_full() -> Proposal {
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

    /// Every variant must serialise to its snake_case wire form so
    /// downstream consumers can dispatch on a plain string without
    /// re-parsing the message.
    #[test]
    fn failure_kind_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&FailureKind::MissingField).unwrap(),
            "\"missing_field\""
        );
        assert_eq!(
            serde_json::to_string(&FailureKind::RustCompileError).unwrap(),
            "\"rust_compile_error\""
        );
        assert_eq!(
            serde_json::to_string(&FailureKind::ForbiddenTech).unwrap(),
            "\"forbidden_tech\""
        );
        assert_eq!(
            serde_json::to_string(&FailureKind::HardConstraintMissing).unwrap(),
            "\"hard_constraint_missing\""
        );
        // The full enum surface must serialise to snake_case; this is
        // the contract the deliver phase relies on.
        let all = [
            FailureKind::MissingField,
            FailureKind::Truncation,
            FailureKind::Placeholder,
            FailureKind::UnbalancedFences,
            FailureKind::ForbiddenTech,
            FailureKind::MissingDeliverable,
            FailureKind::HardConstraintMissing,
            FailureKind::SoftWarning,
            FailureKind::AsciiMismatch,
            FailureKind::TrivialContradiction,
            FailureKind::EvasiveAnswer,
            FailureKind::LengthOutOfRange,
            FailureKind::RustCompileError,
            FailureKind::RustTestFailure,
            FailureKind::PythonSyntaxError,
            FailureKind::TypeScriptCompileError,
            FailureKind::SqlSyntaxError,
            FailureKind::SqlSemanticError,
            FailureKind::SchemaViolation,
            FailureKind::ConstraintUnsatisfiable,
            FailureKind::ReproducibilityMissing,
        ];
        for k in all {
            let s = serde_json::to_string(&k).unwrap();
            assert!(
                s.chars()
                    .all(|c| c == '_' || c.is_ascii_lowercase() || c == '"'),
                "non-snake_case wire form: {s}"
            );
        }
    }

    /// Every variant must round-trip through serde so the deliver
    /// phase can read failure lists back from the JSON sidecar.
    #[test]
    fn failure_kind_round_trips_through_serde() {
        let kinds = [
            FailureKind::MissingField,
            FailureKind::Truncation,
            FailureKind::Placeholder,
            FailureKind::UnbalancedFences,
            FailureKind::ForbiddenTech,
            FailureKind::MissingDeliverable,
            FailureKind::HardConstraintMissing,
            FailureKind::SoftWarning,
            FailureKind::AsciiMismatch,
            FailureKind::TrivialContradiction,
            FailureKind::EvasiveAnswer,
            FailureKind::LengthOutOfRange,
            FailureKind::RustCompileError,
            FailureKind::RustTestFailure,
            FailureKind::PythonSyntaxError,
            FailureKind::TypeScriptCompileError,
            FailureKind::SqlSyntaxError,
            FailureKind::SqlSemanticError,
            FailureKind::SchemaViolation,
            FailureKind::ConstraintUnsatisfiable,
            FailureKind::ReproducibilityMissing,
        ];
        for k in kinds {
            let j = serde_json::to_string(&k).unwrap();
            let back: FailureKind = serde_json::from_str(&j).unwrap();
            assert_eq!(back, k, "round-trip mismatch for {k:?}");
        }
    }

    /// Structural validator must tag the `missing id` failure with
    /// `FailureKind::MissingField` (D.11.14) and not with a free-form
    /// string.
    #[test]
    fn validator_structural_emits_typed_kind_missing_field() {
        let mut p = p_full();
        p.id.clear();
        let e = StructuralValidator::check(&p);
        assert_eq!(e.status, ValidationStatus::Fail);
        assert!(!e.failures.is_empty(), "no typed failures recorded");
        let kinds: Vec<FailureKind> = e.failures.iter().map(|f| f.kind).collect();
        assert!(
            kinds.contains(&FailureKind::MissingField),
            "expected MissingField in {kinds:?}"
        );
        // The typed record must point at the offending field so the
        // deliver phase can render it without re-parsing the message.
        let id_failure = e
            .failures
            .iter()
            .find(|f| f.kind == FailureKind::MissingField && f.field.as_deref() == Some("id"))
            .expect("missing the typed 'id' failure");
        assert_eq!(id_failure.message, "missing id");
    }

    /// Structural validator must tag a forbidden-tech mention with
    /// `FailureKind::ForbiddenTech` (D.11.14). This pins both the
    /// new scan and the typed-kind emission.
    #[test]
    fn validator_structural_emits_typed_kind_forbidden_tech() {
        let mut p = p_full();
        p.approach = "Deploy the service on Kubernetes with Helm charts.".into();
        let e = StructuralValidator::check(&p);
        assert_eq!(e.status, ValidationStatus::Fail);
        let kinds: Vec<FailureKind> = e.failures.iter().map(|f| f.kind).collect();
        assert!(
            kinds.contains(&FailureKind::ForbiddenTech),
            "expected ForbiddenTech in {kinds:?}"
        );
        let tech_failure = e
            .failures
            .iter()
            .find(|f| f.kind == FailureKind::ForbiddenTech)
            .expect("missing the typed forbidden-tech failure");
        assert!(
            tech_failure.message.contains("kubernetes"),
            "message should mention the detected tech: {}",
            tech_failure.message
        );
        assert_eq!(tech_failure.field.as_deref(), Some("approach"));
    }

    /// Rust validator must tag compile failures with
    /// `FailureKind::RustCompileError` and test failures with
    /// `FailureKind::RustTestFailure`. We exercise the emission
    /// logic by routing through `record_step` so the test mirrors
    /// what the live validator does on a real `cargo` invocation.
    #[test]
    fn validator_rust_emits_typed_kind_compile_error() {
        // Build a fake `SandboxResult` mimicking a compile failure
        // and feed it through `record_step` (the same helper the
        // `RustValidator::check` method uses for every toolchain
        // step). `record_step` is `pub(super)` so the test in the
        // parent module can call it directly.
        let result = crate::sandbox::SandboxResult {
            exit_code: 101,
            stdout: String::new(),
            stderr: "error[E0425]: cannot find value `boom` in this scope".into(),
            duration: std::time::Duration::from_millis(1),
            status: crate::sandbox::SandboxStatus::Fail,
            command: "cargo check --offline".into(),
        };
        let mut e = ValidationEvidence {
            validator: "rust".into(),
            status: ValidationStatus::Pass,
            ..ValidationEvidence::default()
        };
        rust_validator::record_step(
            &mut e,
            "cargo check --offline",
            ValidationStatus::Fail,
            &result,
        );
        assert_eq!(e.status, ValidationStatus::Fail);
        let kinds: Vec<FailureKind> = e.failures.iter().map(|f| f.kind).collect();
        assert!(
            kinds.contains(&FailureKind::RustCompileError),
            "expected RustCompileError in {kinds:?}"
        );
        assert!(
            !kinds.contains(&FailureKind::RustTestFailure),
            "cargo check must not tag as test failure: {kinds:?}"
        );

        // The test step must tag as `RustTestFailure` so the
        // deliver phase can tell a compile error from a test
        // failure without re-parsing the message.
        let mut e = ValidationEvidence {
            validator: "rust".into(),
            status: ValidationStatus::Pass,
            ..ValidationEvidence::default()
        };
        let result = crate::sandbox::SandboxResult {
            exit_code: 101,
            stdout: "running 1 test\ntest tests::it_works ... FAILED".into(),
            stderr: String::new(),
            duration: std::time::Duration::from_millis(1),
            status: crate::sandbox::SandboxStatus::Fail,
            command: "cargo test --offline".into(),
        };
        rust_validator::record_step(
            &mut e,
            "cargo test --offline",
            ValidationStatus::Fail,
            &result,
        );
        let kinds: Vec<FailureKind> = e.failures.iter().map(|f| f.kind).collect();
        assert!(
            kinds.contains(&FailureKind::RustTestFailure),
            "cargo test must tag as RustTestFailure, got {kinds:?}"
        );
    }

    /// SQL validator must tag parser failures with
    /// `FailureKind::SqlSyntaxError`. Built directly via the
    /// public record path so the test does not need sqlite3 on disk.
    #[test]
    fn validator_sql_emits_typed_kind_syntax_error() {
        let mut e = ValidationEvidence {
            validator: "sql".into(),
            status: ValidationStatus::Pass,
            ..ValidationEvidence::default()
        };
        e.record_failure(ValidationFailure::new(
            FailureKind::SqlSyntaxError,
            "line 1:7: unexpected character '@'",
        ));
        assert_eq!(e.status, ValidationStatus::Pass);
        // The validator promotes the status to Fail when a syntax
        // failure is recorded; this matches the live parser branch.
        e.status = ValidationStatus::Fail;
        let kinds: Vec<FailureKind> = e.failures.iter().map(|f| f.kind).collect();
        assert!(
            kinds.contains(&FailureKind::SqlSyntaxError),
            "expected SqlSyntaxError in {kinds:?}"
        );
        // The legacy string view must also be populated so the
        // deliver phase keeps working.
        assert_eq!(e.failed_checks, vec!["line 1:7: unexpected character '@'"]);
    }

    /// `FailureKind::from_str` must accept every known snake_case
    /// name and reject anything else. This is the parse-side
    /// counterpart of the serde test; legacy sidecars carry the
    /// snake_case form verbatim and need a clean parse path.
    #[test]
    fn failure_kind_from_str_parses_known_kinds() {
        use std::str::FromStr;
        let cases = [
            ("missing_field", FailureKind::MissingField),
            ("truncation", FailureKind::Truncation),
            ("placeholder", FailureKind::Placeholder),
            ("unbalanced_fences", FailureKind::UnbalancedFences),
            ("forbidden_tech", FailureKind::ForbiddenTech),
            ("missing_deliverable", FailureKind::MissingDeliverable),
            (
                "hard_constraint_missing",
                FailureKind::HardConstraintMissing,
            ),
            ("soft_warning", FailureKind::SoftWarning),
            ("ascii_mismatch", FailureKind::AsciiMismatch),
            ("trivial_contradiction", FailureKind::TrivialContradiction),
            ("evasive_answer", FailureKind::EvasiveAnswer),
            ("length_out_of_range", FailureKind::LengthOutOfRange),
            ("rust_compile_error", FailureKind::RustCompileError),
            ("rust_test_failure", FailureKind::RustTestFailure),
            ("python_syntax_error", FailureKind::PythonSyntaxError),
            (
                "typescript_compile_error",
                FailureKind::TypeScriptCompileError,
            ),
            ("sql_syntax_error", FailureKind::SqlSyntaxError),
            ("sql_semantic_error", FailureKind::SqlSemanticError),
            ("schema_violation", FailureKind::SchemaViolation),
            (
                "constraint_unsatisfiable",
                FailureKind::ConstraintUnsatisfiable,
            ),
            (
                "reproducibility_missing",
                FailureKind::ReproducibilityMissing,
            ),
        ];
        for (s, expected) in cases {
            assert_eq!(
                FailureKind::from_str(s).unwrap(),
                expected,
                "from_str({s}) must yield {expected:?}"
            );
        }
    }

    /// Unknown names must surface as a parse error so the deliver
    /// phase never silently treats an unknown kind as a pass.
    #[test]
    fn failure_kind_from_str_rejects_unknown() {
        use std::str::FromStr;
        for bogus in [
            "",
            "unknown_kind",
            "MissingField",
            "MISSING_FIELD",
            "missing-field",
        ] {
            let err = FailureKind::from_str(bogus).unwrap_err();
            assert!(
                err.contains(bogus.trim()),
                "error must mention the rejected input {bogus:?}, got: {err}"
            );
        }
    }

    // -- extra D.11.14 coverage (keeps the contract honest) -------------

    /// `From<FailureKind> for String` must produce the snake_case
    /// wire form so legacy sinks that hold a `String` see the same
    /// value serde emits.
    #[test]
    fn failure_kind_into_string_is_snake_case() {
        let s: String = FailureKind::RustTestFailure.into();
        assert_eq!(s, "rust_test_failure");
        let s: String = FailureKind::SoftWarning.into();
        assert_eq!(s, "soft_warning");
    }

    /// `ValidationEvidence::record_failure` must populate both the
    /// typed view and the legacy string view in lock-step so the
    /// deliver phase never sees them diverge.
    #[test]
    fn record_failure_populates_typed_and_legacy_views() {
        let mut e = ValidationEvidence::default();
        e.record_failure(
            ValidationFailure::new(FailureKind::MissingField, "missing id").with_field("id"),
        );
        e.record_failure(
            ValidationFailure::new(FailureKind::LengthOutOfRange, "summary too short")
                .with_field("summary"),
        );
        assert_eq!(e.failed_checks.len(), 2);
        assert_eq!(e.failures.len(), 2);
        assert_eq!(e.failed_checks[0], "missing id");
        assert_eq!(e.failed_checks[1], "summary too short");
        assert_eq!(e.failures[0].kind, FailureKind::MissingField);
        assert_eq!(e.failures[1].field.as_deref(), Some("summary"));
    }

    /// `ValidationEvidence::outcome` must collapse `Warn` /
    /// `Skipped` to `Ok` so the typed outcome is binary (only
    /// `Fail` carries failures). `Fail` with a non-empty
    /// `failures` list becomes `ValidationOutcome::Fail`.
    #[test]
    fn evidence_outcome_collapses_warn_and_skipped_to_ok() {
        let mut e = ValidationEvidence::pass("x", "y");
        e.status = ValidationStatus::Warn;
        e.record_failure(ValidationFailure::new(
            FailureKind::SoftWarning,
            "soft signal",
        ));
        let outcome = e.outcome();
        assert!(outcome.is_ok());
        assert!(outcome.failures().is_empty());

        let skipped = ValidationEvidence::skipped("x", "no binary");
        let outcome = skipped.outcome();
        assert!(outcome.is_ok());

        let mut failed = ValidationEvidence {
            status: ValidationStatus::Fail,
            ..ValidationEvidence::default()
        };
        failed.record_failure(ValidationFailure::new(
            FailureKind::RustCompileError,
            "boom",
        ));
        let outcome = failed.outcome();
        assert!(!outcome.is_ok());
        assert_eq!(outcome.failures().len(), 1);
        assert_eq!(outcome.failures()[0].kind, FailureKind::RustCompileError);
    }

    /// `ValidationOutcome` JSON shape is `{"verdict": "ok"}` or
    /// `{"verdict": "fail", "failures": [...]}`. The deliver phase
    /// consumes this shape verbatim.
    #[test]
    fn validation_outcome_serialises_with_verdict_tag() {
        let ok = ValidationOutcome::ok();
        let j = serde_json::to_string(&ok).unwrap();
        assert_eq!(j, "{\"verdict\":\"ok\"}");

        let failed = ValidationOutcome::fail(vec![ValidationFailure::new(
            FailureKind::SchemaViolation,
            "data does not match schema",
        )]);
        let j = serde_json::to_string(&failed).unwrap();
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["verdict"], "fail");
        assert_eq!(v["failures"].as_array().unwrap().len(), 1);
        assert_eq!(v["failures"][0]["kind"], "schema_violation");
    }

    /// `ValidationOutcome::fail(empty)` collapses to `Ok` so a
    /// caller that builds an outcome by aggregating typed failures
    /// can rely on the constructor alone.
    #[test]
    fn validation_outcome_fail_with_empty_list_is_ok() {
        let outcome = ValidationOutcome::fail(Vec::new());
        assert!(outcome.is_ok());
        let outcome = ValidationOutcome::fail(vec![]);
        assert!(outcome.failures().is_empty());
    }
}
