//! Rust validator — runs `cargo check` in the sandbox.
//!
//! Strategy: drop the artifact's source into a freshly generated
//! `Cargo.toml` + `src/main.rs` (or `src/lib.rs`) project inside the
//! sandbox's temp dir and invoke `cargo check --offline`. The
//! `--offline` flag prevents the validator from downloading crates
//! from the network, keeping the run hermetic.
//!
//! Verdict mapping:
//! - `cargo` missing on disk → `Skipped`
//! - `exit_code == 0` → `Pass`
//! - non-zero exit → `Fail` (compile error)
//!
//! The validator reports `command`, `exit_code`, `stdout_summary`,
//! and `stderr_summary` so the deliver phase can surface the actual
//! error to the user.
//!
//! Compliance: `proposal-02-rust.md` §5.7 + §7.

use std::fs;

use crate::error::Result;
use crate::sandbox::{Sandbox, SandboxResult, SandboxStatus};

use super::{CodeArtifact, ValidationEvidence, ValidationStatus, Validator, capture_tool_version};

/// Rust validator. Stateless; reuse freely.
#[derive(Debug, Default, Clone, Copy)]
pub struct RustValidator;

impl RustValidator {
    /// Build a new instance.
    pub fn new() -> Self {
        Self
    }

    /// Language id this validator claims; used by the validate phase
    /// to dispatch artifacts.
    pub const LANGUAGE: &'static str = "rust";

    /// Run `cargo check --offline` against the artifact inside the
    /// sandbox's scratch dir.
    pub async fn check(artifact: &CodeArtifact, sandbox: &Sandbox) -> Result<ValidationEvidence> {
        if !looks_like_rust_source(&artifact.source) {
            // Very loose sanity check: an empty / placeholder source
            // almost certainly indicates a non-executable artefact
            // (e.g. a config snippet labelled "rust"). Treat as
            // Skipped rather than Fail so the pipeline does not
            // punish proposals that include non-compilable notes.
            return Ok(ValidationEvidence::skipped(
                "rust",
                "no `fn` body in source; artifact looks non-executable",
            ));
        }

        let work = sandbox.new_workdir()?;
        write_minimal_crate(work.path(), artifact)?;

        let result = sandbox
            .run_in(work.path(), "cargo", &["check", "--offline"])
            .await?;

        let mut evidence = evidence_from_result(result);
        if let Some(v) = capture_tool_version(sandbox, "cargo").await {
            evidence.reproducibility.push(("cargo".into(), v));
        }
        Ok(evidence)
    }
}

impl Validator for RustValidator {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn validate(
        &self,
        _proposal: &crate::domain::Proposal,
        _sandbox: Option<&Sandbox>,
    ) -> Result<ValidationEvidence> {
        // The base trait cannot carry source text; the validate
        // phase will call `RustValidator::check` directly per
        // artifact. Reporting Skipped here keeps the composite happy
        // when a proposal has no Rust code attached.
        Ok(ValidationEvidence::skipped(
            "rust",
            "no source code attached; check called per-artifact",
        ))
    }
}

/// Heuristic check: does the source look like Rust with at least
/// one function definition? Matches `fn <ident>(...` so that "no fn
/// bodies" prose does not trip the check.
fn looks_like_rust_source(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut i = 0;
    while i + 4 < bytes.len() {
        if &bytes[i..i + 3] == b"fn "
            && (bytes[i + 3].is_ascii_alphabetic() || bytes[i + 3] == b'_')
        {
            // Walk the identifier characters.
            let mut j = i + 3;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            // Skip whitespace (including newlines).
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            // A function signature must have an open paren next (possibly
            // preceded by generic params, but the open paren is required).
            if j < bytes.len() && bytes[j] == b'(' {
                return true;
            }
            // Otherwise keep scanning; advance i past this `fn `.
            i = j;
            continue;
        }
        i += 1;
    }
    false
}

/// Lay out a minimal Cargo project around the artifact's source. The
/// source is written to `src/lib.rs` so `cargo check` finds it via
/// the library target. If the artifact's source happens to contain
/// a `fn main()`, the model almost certainly meant a binary crate;
/// in that case the source goes to `src/main.rs` instead.
fn write_minimal_crate(root: &std::path::Path, artifact: &CodeArtifact) -> Result<()> {
    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir)?;
    let has_main = artifact.source.contains("fn main(");
    if has_main {
        fs::write(src_dir.join("main.rs"), &artifact.source)?;
        let manifest = "[package]\nname = \"validator_snippet\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n";
        fs::write(root.join("Cargo.toml"), manifest)?;
    } else {
        fs::write(src_dir.join("lib.rs"), &artifact.source)?;
        let manifest = "[package]\nname = \"validator_snippet\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[lib]\npath = \"src/lib.rs\"\n";
        fs::write(root.join("Cargo.toml"), manifest)?;
    }
    Ok(())
}

fn evidence_from_result(result: SandboxResult) -> ValidationEvidence {
    let mut evidence = ValidationEvidence {
        validator: "rust".into(),
        status: status_from_sandbox(result.status),
        command: Some(result.command.clone()),
        exit_code: Some(result.exit_code),
        stdout_summary: tail(&result.stdout, 2_000),
        stderr_summary: tail(&result.stderr, 2_000),
        ..ValidationEvidence::default()
    };
    evidence.checks_run.push("cargo check --offline".into());
    if result.status == SandboxStatus::Fail {
        evidence
            .failed_checks
            .push("cargo check returned non-zero exit".into());
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

/// Keep the trailing N bytes of `text` so we never blow up the
/// evidence payload.
pub(super) fn tail(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let start = text.len() - cap;
    text[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::sandbox::{Sandbox, SandboxConfig};

    fn sandbox() -> Sandbox {
        Sandbox::new(SandboxConfig::new()).unwrap()
    }

    fn good_rust() -> CodeArtifact {
        CodeArtifact::new(
            "src/lib.rs",
            "rust",
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
    }

    fn broken_rust() -> CodeArtifact {
        CodeArtifact::new(
            "src/lib.rs",
            "rust",
            "pub fn add(a: i32, b: i32) -> i32 { a + ; }\n",
        )
    }

    fn non_executable() -> CodeArtifact {
        CodeArtifact::new("notes.md", "rust", "this is just a note, no fn bodies")
    }

    #[test]
    fn non_executable_source_is_skipped() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt
            .block_on(RustValidator::check(&non_executable(), &sandbox()))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Skipped);
        assert!(ev.skipped_checks[0].contains("non-executable"));
    }

    #[test]
    fn missing_cargo_returns_skipped() {
        // Strip cargo from the allowlist so we can prove the binary
        // missing branch is reachable in environments without rustc.
        let cfg = SandboxConfig::new().with_allowlist(crate::sandbox::Allowlist::from_slice([
            "definitely-not-cargo-xyz",
        ]));
        let sb = Sandbox::new(cfg).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt
            .block_on(RustValidator::check(&good_rust(), &sb))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Skipped);
        assert_eq!(ev.validator, "rust");
    }

    #[test]
    fn good_rust_passes_when_cargo_present() {
        if std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .is_err()
        {
            // Skip silently: cargo not installed on this host.
            return;
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt
            .block_on(RustValidator::check(&good_rust(), &sandbox()))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Pass);
        assert!(ev.command.is_some());
        assert_eq!(ev.exit_code, Some(0));
        // The reproducibility field records the cargo version that
        // produced the verdict. Skip the assertion when capture
        // returned nothing (e.g. an isolated sandbox without
        // --version output).
        if let Some((tool, _)) = ev.reproducibility.first() {
            assert_eq!(tool, "cargo");
        }
    }

    #[test]
    fn broken_rust_fails_when_cargo_present() {
        if std::process::Command::new("cargo")
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
            .block_on(RustValidator::check(&broken_rust(), &sandbox()))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Fail);
        assert!(ev.stderr_summary.contains("error"));
    }

    #[test]
    fn validator_trait_returns_skipped() {
        let v = RustValidator::new();
        let p = crate::domain::Proposal::default();
        let e = v.validate(&p, None).unwrap();
        assert_eq!(e.status, ValidationStatus::Skipped);
    }

    #[test]
    fn write_minimal_crate_layout_lib() {
        let work = tempfile::tempdir().unwrap();
        write_minimal_crate(work.path(), &good_rust()).unwrap();
        assert!(work.path().join("Cargo.toml").exists());
        assert!(work.path().join("src/lib.rs").exists());
        let manifest = std::fs::read_to_string(work.path().join("Cargo.toml")).unwrap();
        assert!(manifest.contains("[lib]"));
    }

    #[test]
    fn write_minimal_crate_layout_bin() {
        let work = tempfile::tempdir().unwrap();
        let bin = CodeArtifact::new("src/main.rs", "rust", "fn main() { println!(\"hi\"); }\n");
        write_minimal_crate(work.path(), &bin).unwrap();
        let manifest = std::fs::read_to_string(work.path().join("Cargo.toml")).unwrap();
        assert!(!manifest.contains("[lib]"));
        assert!(work.path().join("src/main.rs").exists());
    }

    #[test]
    fn tail_truncates_from_the_front() {
        let s = "x".repeat(5_000);
        let t = tail(&s, 100);
        assert_eq!(t.len(), 100);
        assert!(t.chars().all(|c| c == 'x'));
    }

    #[test]
    fn evidence_from_result_maps_status() {
        let r = SandboxResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration: std::time::Duration::from_millis(1),
            status: SandboxStatus::Pass,
            command: "cargo check".into(),
        };
        let e = evidence_from_result(r);
        assert_eq!(e.status, ValidationStatus::Pass);
        assert_eq!(e.exit_code, Some(0));
    }

    #[test]
    fn evidence_from_result_maps_fail() {
        let r = SandboxResult {
            exit_code: 101,
            stdout: String::new(),
            stderr: "error[E0425]: ...".into(),
            duration: std::time::Duration::from_millis(1),
            status: SandboxStatus::Fail,
            command: "cargo check".into(),
        };
        let e = evidence_from_result(r);
        assert_eq!(e.status, ValidationStatus::Fail);
        assert!(e.stderr_summary.contains("E0425"));
    }
}
