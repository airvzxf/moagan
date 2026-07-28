//! Python validator — runs `python -m py_compile` in the sandbox.
//!
//! Strategy: drop the artifact's source into a scratch dir as
//! `check.py` and invoke `python3 -m py_compile check.py`. The
//! `-m py_compile` mode parses every module top-level without
//! executing it, which is the cheapest meaningful signal that
//! the source is at least syntactically valid Python.
//!
//! Verdict mapping:
//! - `python3` missing on disk → `Skipped`
//! - exit `0` → `Pass`
//! - non-zero exit → `Fail` (syntax error reported on stderr)
//!
//! Compliance: `proposal-02-rust.md` §5.7.

use std::fs;

use crate::error::Result;
use crate::sandbox::{Sandbox, SandboxResult, SandboxStatus};

use super::rust_validator::tail;
use super::{CodeArtifact, ValidationEvidence, ValidationStatus, Validator};

/// Python validator. Stateless; reuse freely.
#[derive(Debug, Default, Clone, Copy)]
pub struct PythonValidator;

impl PythonValidator {
    /// Build a new instance.
    pub fn new() -> Self {
        Self
    }

    /// Language id this validator claims.
    pub const LANGUAGE: &'static str = "python";

    /// Run `python3 -m py_compile` against the artifact inside the
    /// sandbox's scratch dir.
    pub async fn check(artifact: &CodeArtifact, sandbox: &Sandbox) -> Result<ValidationEvidence> {
        if !looks_like_python_source(&artifact.source) {
            return Ok(ValidationEvidence::skipped(
                "python",
                "no Python identifier followed by `(` in source; artifact looks non-executable",
            ));
        }

        let work = sandbox.new_workdir()?;
        let script = work.path().join("check.py");
        fs::write(&script, &artifact.source)?;

        let result = sandbox
            .run_in(work.path(), "python3", &["-m", "py_compile", "check.py"])
            .await?;

        Ok(evidence_from_result(result))
    }
}

impl Validator for PythonValidator {
    fn name(&self) -> &'static str {
        "python"
    }

    fn validate(
        &self,
        _proposal: &crate::domain::Proposal,
        _sandbox: Option<&Sandbox>,
    ) -> Result<ValidationEvidence> {
        Ok(ValidationEvidence::skipped(
            "python",
            "no source code attached; check called per-artifact",
        ))
    }
}
/// Heuristic check: does the source look like Python with at least
/// one callable statement (`def <ident>(` or `class <ident>`)? Avoids
/// running `py_compile` on prose such as "no def here".
fn looks_like_python_source(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut i = 0;
    while i + 8 < bytes.len() {
        let is_def = i + 4 <= bytes.len() && &bytes[i..i + 4] == b"def ";
        let is_class = i + 6 <= bytes.len() && &bytes[i..i + 6] == b"class ";
        if is_def || is_class {
            let mut j = if is_def { i + 4 } else { i + 6 };
            // Walk the identifier characters.
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            // Skip whitespace (including newlines).
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            // `def foo(`, `class Foo:` or `class Foo(` next.
            if j < bytes.len() && (bytes[j] == b'(' || bytes[j] == b':') {
                return true;
            }
            // Advance past the keyword we just inspected so we do
            // not loop on the same `def ` over and over.
            i = j;
            continue;
        }
        i += 1;
    }
    false
}
fn evidence_from_result(result: SandboxResult) -> ValidationEvidence {
    let mut evidence = ValidationEvidence {
        validator: "python".into(),
        status: status_from_sandbox(result.status),
        command: Some(result.command.clone()),
        exit_code: Some(result.exit_code),
        stdout_summary: tail(&result.stdout, 2_000),
        stderr_summary: tail(&result.stderr, 2_000),
        ..ValidationEvidence::default()
    };
    evidence.checks_run.push("python3 -m py_compile".into());
    if result.status == SandboxStatus::Fail {
        evidence
            .failed_checks
            .push("py_compile returned non-zero exit".into());
    }
    evidence
}

fn status_from_sandbox(status: SandboxStatus) -> ValidationStatus {
    match status {
        SandboxStatus::Pass => ValidationStatus::Pass,
        SandboxStatus::Fail => ValidationStatus::Fail,
        SandboxStatus::Timeout => ValidationStatus::Fail,
        SandboxStatus::NotAllowed => ValidationStatus::Skipped,
        SandboxStatus::NotFound => ValidationStatus::Skipped,
        SandboxStatus::Error => ValidationStatus::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::sandbox::{Allowlist, Sandbox, SandboxConfig};

    fn sandbox() -> Sandbox {
        Sandbox::new(SandboxConfig::new()).unwrap()
    }

    fn good_python() -> CodeArtifact {
        CodeArtifact::new(
            "src/check.py",
            "python",
            "def add(a: int, b: int) -> int:\n    return a + b\n",
        )
    }

    fn broken_python() -> CodeArtifact {
        CodeArtifact::new(
            "src/check.py",
            "python",
            "def add(a: int, b: int) -> int\n    return a + b\n",
        )
    }

    fn non_executable() -> CodeArtifact {
        CodeArtifact::new("notes.md", "python", "just a note, no def here")
    }

    #[test]
    fn non_executable_source_is_skipped() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt
            .block_on(PythonValidator::check(&non_executable(), &sandbox()))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Skipped);
        assert!(ev.skipped_checks[0].contains("non-executable"));
    }

    #[test]
    fn missing_python_returns_skipped() {
        // Allowlist with a name that does not match a real python
        // binary so we exercise the NotFound branch.
        let cfg = SandboxConfig::new()
            .with_allowlist(Allowlist::from_slice(["definitely-not-python-xyz"]));
        let sb = Sandbox::new(cfg).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt
            .block_on(PythonValidator::check(&good_python(), &sb))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Skipped);
        assert_eq!(ev.validator, "python");
    }

    #[test]
    fn good_python_passes_when_python_present() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt
            .block_on(PythonValidator::check(&good_python(), &sandbox()))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Pass);
        assert_eq!(ev.exit_code, Some(0));
    }

    #[test]
    fn broken_python_fails_when_python_present() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt
            .block_on(PythonValidator::check(&broken_python(), &sandbox()))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Fail);
        assert!(ev.stderr_summary.contains("SyntaxError") || ev.stderr_summary.contains("invalid"));
    }

    #[test]
    fn validator_trait_returns_skipped() {
        let v = PythonValidator::new();
        let p = crate::domain::Proposal::default();
        let e = v.validate(&p, None).unwrap();
        assert_eq!(e.status, ValidationStatus::Skipped);
    }

    #[test]
    fn looks_like_python_source_matches_def_and_class() {
        assert!(looks_like_python_source("def f(): pass"));
        assert!(looks_like_python_source("class Foo: pass"));
        assert!(!looks_like_python_source("just a note"));
    }
}
