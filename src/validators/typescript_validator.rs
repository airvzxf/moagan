//! TypeScript validator — runs `tsc --noEmit` in the sandbox.
//!
//! Strategy: lay out a minimal `tsconfig.json` + the artifact's
//! source as `check.ts` inside a scratch dir and invoke
//! `tsc --noEmit`. The `--noEmit` flag instructs TypeScript to
//! type-check without producing JS output, which is the cheapest
//! way to surface real type errors.
//!
//! Verdict mapping:
//! - `tsc` missing on disk → `Skipped`
//! - exit `0` → `Pass`
//! - non-zero exit → `Fail` (type error reported on stdout)
//!
//! Compliance: `proposal-02-rust.md` §5.7.

use std::fs;

use crate::error::Result;
use crate::sandbox::{Sandbox, SandboxResult, SandboxStatus};

use super::rust_validator::tail;
use super::{CodeArtifact, ValidationEvidence, ValidationStatus, Validator};

/// TypeScript validator. Stateless; reuse freely.
#[derive(Debug, Default, Clone, Copy)]
pub struct TypeScriptValidator;

impl TypeScriptValidator {
    /// Build a new instance.
    pub fn new() -> Self {
        Self
    }

    /// Language id this validator claims.
    pub const LANGUAGE: &'static str = "typescript";

    /// Run `tsc --noEmit` against the artifact inside the sandbox's
    /// scratch dir.
    pub async fn check(artifact: &CodeArtifact, sandbox: &Sandbox) -> Result<ValidationEvidence> {
        if !looks_like_typescript_source(&artifact.source) {
            return Ok(ValidationEvidence::skipped(
                "typescript",
                "no function/const/let declaration found; artifact looks non-executable",
            ));
        }

        let work = sandbox.new_workdir()?;
        fs::write(work.path().join("check.ts"), &artifact.source)?;
        fs::write(work.path().join("tsconfig.json"), minimal_tsconfig())?;

        let result = sandbox
            .run_in(work.path(), "tsc", &["--noEmit", "check.ts"])
            .await?;

        Ok(evidence_from_result(result))
    }
}

impl Validator for TypeScriptValidator {
    fn name(&self) -> &'static str {
        "typescript"
    }

    fn validate(
        &self,
        _proposal: &crate::domain::Proposal,
        _sandbox: Option<&Sandbox>,
    ) -> Result<ValidationEvidence> {
        Ok(ValidationEvidence::skipped(
            "typescript",
            "no source code attached; check called per-artifact",
        ))
    }
}

/// Minimal `tsconfig.json` for a single-file check. We disable
/// `strict` so the model can ship loose code; the proposal-level
/// validator only cares that the syntax and basic types resolve.
fn minimal_tsconfig() -> &'static str {
    "{\n  \"compilerOptions\": {\n    \"target\": \"es2022\",\n    \"module\": \"esnext\",\n    \"strict\": false,\n    \"noEmit\": true,\n    \"skipLibCheck\": true\n  },\n  \"include\": [\"check.ts\"]\n}\n"
}

/// Heuristic check: does the source look like TypeScript with at
/// least one declaration (`function`, `const`, `let`, `var`,
/// `interface`, `type`, `class`)? Avoids running `tsc` on prose.
fn looks_like_typescript_source(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut i = 0;
    let keywords: &[&[u8]] = &[
        b"function ",
        b"const ",
        b"let ",
        b"var ",
        b"interface ",
        b"type ",
        b"class ",
        b"export ",
        b"import ",
    ];
    while i < bytes.len() {
        for kw in keywords {
            if i + kw.len() <= bytes.len() && &bytes[i..i + kw.len()] == *kw {
                // Accept: a keyword is present, so the source
                // looks TS-shaped. We do not require an open paren
                // here because `const x = 1;` is a perfectly valid
                // declaration that we still want to type-check.
                return true;
            }
        }
        i += 1;
    }
    false
}

fn evidence_from_result(result: SandboxResult) -> ValidationEvidence {
    let mut evidence = ValidationEvidence {
        validator: "typescript".into(),
        status: status_from_sandbox(result.status),
        command: Some(result.command.clone()),
        exit_code: Some(result.exit_code),
        stdout_summary: tail(&result.stdout, 2_000),
        stderr_summary: tail(&result.stderr, 2_000),
        ..ValidationEvidence::default()
    };
    evidence.checks_run.push("tsc --noEmit".into());
    if result.status == SandboxStatus::Fail {
        evidence
            .failed_checks
            .push("tsc returned non-zero exit".into());
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

    fn good_ts() -> CodeArtifact {
        CodeArtifact::new(
            "src/check.ts",
            "typescript",
            "export function add(a: number, b: number): number {\n  return a + b;\n}\n",
        )
    }

    fn broken_ts() -> CodeArtifact {
        CodeArtifact::new(
            "src/check.ts",
            "typescript",
            "export function add(a: number, b: number): number {\n  return a + ;\n}\n",
        )
    }

    fn non_executable() -> CodeArtifact {
        CodeArtifact::new("notes.md", "typescript", "just a note, no declarations")
    }

    #[test]
    fn non_executable_source_is_skipped() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt
            .block_on(TypeScriptValidator::check(&non_executable(), &sandbox()))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Skipped);
        assert!(ev.skipped_checks[0].contains("non-executable"));
    }

    #[test]
    fn missing_tsc_returns_skipped() {
        let cfg =
            SandboxConfig::new().with_allowlist(Allowlist::from_slice(["definitely-not-tsc-xyz"]));
        let sb = Sandbox::new(cfg).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt
            .block_on(TypeScriptValidator::check(&good_ts(), &sb))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Skipped);
        assert_eq!(ev.validator, "typescript");
    }

    #[test]
    fn good_ts_passes_when_tsc_present() {
        if std::process::Command::new("tsc")
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
            .block_on(TypeScriptValidator::check(&good_ts(), &sandbox()))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Pass);
        assert_eq!(ev.exit_code, Some(0));
    }

    #[test]
    fn broken_ts_fails_when_tsc_present() {
        if std::process::Command::new("tsc")
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
            .block_on(TypeScriptValidator::check(&broken_ts(), &sandbox()))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Fail);
        // tsc reports the error on stdout.
        assert!(
            ev.stdout_summary.contains("error")
                || ev.stderr_summary.contains("error")
                || ev.stdout_summary.contains("TS")
                || ev.stderr_summary.contains("TS"),
        );
    }

    #[test]
    fn validator_trait_returns_skipped() {
        let v = TypeScriptValidator::new();
        let p = crate::domain::Proposal::default();
        let e = v.validate(&p, None).unwrap();
        assert_eq!(e.status, ValidationStatus::Skipped);
    }

    #[test]
    fn looks_like_typescript_source_matches_keywords() {
        assert!(looks_like_typescript_source("function f() {}"));
        assert!(looks_like_typescript_source("const x = 1;"));
        assert!(looks_like_typescript_source("interface I {}"));
        assert!(looks_like_typescript_source("type T = number;"));
        assert!(looks_like_typescript_source("export function f() {}"));
        assert!(!looks_like_typescript_source("just a note"));
    }
}
