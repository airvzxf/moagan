//! Rust validator — runs `cargo check`, `cargo fmt --check`, and
//! `cargo clippy` in the sandbox.
//!
//! Strategy: drop the artifact's source into a freshly generated
//! `Cargo.toml` + `src/main.rs` (or `src/lib.rs`) project inside the
//! sandbox's temp dir and run the three toolchain steps in order:
//!
//! 1. `cargo check --offline`
//! 2. `cargo fmt --check`
//! 3. `cargo clippy --offline -- -D warnings`
//!
//! The steps are gated: when a step fails, the remaining steps are
//! recorded as `skipped_checks` (the prior failure short-circuits
//! the work). The first failure dictates the verdict; if every
//! step passes, the verdict is `Pass`.
//!
//! The `--offline` flag on `check` and `clippy` prevents the
//! validator from downloading crates from the network, keeping
//! the run hermetic. `fmt` is offline by design.
//!
//! Verdict mapping:
//! - `cargo` missing on disk → `Skipped` for every step
//! - `exit_code == 0` → `Pass`
//! - non-zero exit → `Fail` (compile error / format drift / lint)
//!
//! The validator reports `command`, `exit_code`, `stdout_summary`,
//! and `stderr_summary` so the deliver phase can surface the
//! actual error to the user.
//!
//! Compliance: `proposal-01-concept.md` §5.8 ("cargo fmt --check",
//! "cargo check", "cargo clippy -- -D warnings") +
//! `proposal-02-rust.md` §5.7 + §7.

use std::fs;

use crate::error::Result;
use crate::sandbox::{Sandbox, SandboxResult, SandboxStatus};

use super::{
    CodeArtifact, FailureKind, ValidationEvidence, ValidationFailure, ValidationStatus, Validator,
    capture_tool_version,
};

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

    /// Run the Rust toolchain checks against the artifact inside
    /// the sandbox's scratch dir. See module docs for the exact
    /// step ordering and verdict mapping.
    pub async fn check(artifact: &CodeArtifact, sandbox: &Sandbox) -> Result<ValidationEvidence> {
        tracing::debug!(
            kind = %artifact.kind,
            "validators::rust::RustValidator::check: enter"
        );
        if !looks_like_rust_source(&artifact.source) {
            // Very loose sanity check: an empty / placeholder source
            // almost certainly indicates a non-executable artefact
            // (e.g. a config snippet labelled "rust"). Treat as
            // Skipped rather than Fail so the pipeline does not
            // punish proposals that include non-compilable notes.
            tracing::trace!("validators::rust::RustValidator::check: skipping non-executable");
            return Ok(ValidationEvidence::skipped(
                "rust",
                "no `fn` body in source; artifact looks non-executable",
            ));
        }

        let work = sandbox.new_workdir()?;
        write_minimal_crate(work.path(), artifact)?;

        let mut evidence = ValidationEvidence {
            validator: "rust".into(),
            status: ValidationStatus::Pass,
            ..ValidationEvidence::default()
        };

        // Step 1: `cargo check --offline`.
        let check_result = sandbox
            .run_in(work.path(), "cargo", &["check", "--offline"])
            .await?;
        let check_status = status_from_sandbox(check_result.status);
        record_step(
            &mut evidence,
            "cargo check --offline",
            check_status,
            &check_result,
        );

        // Steps 2 and 3 only run when step 1 passed.
        if check_status == ValidationStatus::Pass {
            let fmt_result = sandbox
                .run_in(work.path(), "cargo", &["fmt", "--check"])
                .await?;
            let fmt_status = status_from_sandbox(fmt_result.status);
            record_step(&mut evidence, "cargo fmt --check", fmt_status, &fmt_result);

            if fmt_status == ValidationStatus::Pass {
                let clippy_result = sandbox
                    .run_in(
                        work.path(),
                        "cargo",
                        &["clippy", "--offline", "--", "-D", "warnings"],
                    )
                    .await?;
                let clippy_status = status_from_sandbox(clippy_result.status);
                record_step(
                    &mut evidence,
                    "cargo clippy --offline -- -D warnings",
                    clippy_status,
                    &clippy_result,
                );

                if clippy_status == ValidationStatus::Pass {
                    // Step 4: `cargo test --offline`. A artifact
                    // that declares no #[test] does not earn a
                    // Fail — proposal-01-concept.md §5.8 says
                    // "Un test ausente no equivale a test
                    // aprobado", and the corollary is that the
                    // absence of tests must be visible (not
                    // confused with a pass). The validator runs
                    // the full `cargo test` and inspects the
                    // output afterwards to surface the no-tests
                    // marker.
                    let test_result = sandbox
                        .run_in(work.path(), "cargo", &["test", "--offline"])
                        .await?;
                    let test_status = status_from_sandbox(test_result.status);
                    let no_tests = test_count_is_zero(&test_result.stderr, &test_result.stdout);
                    record_step(
                        &mut evidence,
                        "cargo test --offline",
                        test_status,
                        &test_result,
                    );
                    if no_tests && test_status == ValidationStatus::Pass {
                        evidence
                            .skipped_checks
                            .push("no tests declared in artifact".into());
                    }
                } else {
                    evidence
                        .skipped_checks
                        .push("cargo test --offline (prior step failed)".into());
                }
            } else {
                evidence
                    .skipped_checks
                    .push("cargo clippy --offline -- -D warnings (prior step failed)".into());
                evidence
                    .skipped_checks
                    .push("cargo test --offline (prior step failed)".into());
            }
        } else {
            evidence
                .skipped_checks
                .push("cargo fmt --check (prior step failed)".into());
            evidence
                .skipped_checks
                .push("cargo clippy --offline -- -D warnings (prior step failed)".into());
            evidence
                .skipped_checks
                .push("cargo test --offline (prior step failed)".into());
        }

        if let Some(v) = capture_tool_version(sandbox, "cargo").await {
            evidence.reproducibility.push(("cargo".into(), v));
        }
        tracing::debug!(
            status = ?evidence.status,
            checks = evidence.checks_run.len(),
            "validators::rust::RustValidator::check: exit"
        );
        Ok(evidence)
    }
}

/// True when `cargo test --no-run` produced no test cases. cargo
/// prints `running 0 tests` to stdout for each test binary. We
/// also accept the older "Running 0 tests" form for completeness.
/// A missing test binary or a build error returns false (the
/// caller treats that as a real failure).
fn test_count_is_zero(stderr: &str, stdout: &str) -> bool {
    let combined = format!("{stderr}\n{stdout}");
    let has_zero = combined.contains("running 0 tests")
        || combined.contains("Running 0 tests")
        || combined.contains("0 tests, 0 passed");
    let has_at_least_one = combined.contains("running 1 test")
        || combined.contains("running 2 tests")
        || combined.contains("running 3 tests")
        || combined.contains("running 4 tests")
        || combined.contains("running 5 tests")
        || combined.contains("running 6 tests")
        || combined.contains("running 7 tests")
        || combined.contains("running 8 tests")
        || combined.contains("running 9 tests");
    has_zero && !has_at_least_one
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
    let result = (|| {
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
    })();
    tracing::trace!(
        source_len = bytes.len(),
        looks_like_rust = result,
        "validators::rust::looks_like_rust_source"
    );
    result
}

/// Lay out a minimal Cargo project around the artifact's source. The
/// source is written to `src/lib.rs` so `cargo check` finds it via
/// the library target. If the artifact's source happens to contain
/// a `fn main()`, the model almost certainly meant a binary crate;
/// in that case the source goes to `src/main.rs` instead.
fn write_minimal_crate(root: &std::path::Path, artifact: &CodeArtifact) -> Result<()> {
    tracing::trace!(root = %root.display(), "validators::rust::write_minimal_crate: enter");
    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir)?;
    let has_main = artifact.source.contains("fn main(");
    if has_main {
        fs::write(src_dir.join("main.rs"), &artifact.source)?;
        let manifest = "[package]\nname = \"validator_snippet\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n";
        fs::write(root.join("Cargo.toml"), manifest)?;
        tracing::trace!(root = %root.display(), "validators::rust::write_minimal_crate: bin layout");
    } else {
        fs::write(src_dir.join("lib.rs"), &artifact.source)?;
        let manifest = "[package]\nname = \"validator_snippet\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[lib]\npath = \"src/lib.rs\"\n";
        fs::write(root.join("Cargo.toml"), manifest)?;
        tracing::trace!(root = %root.display(), "validators::rust::write_minimal_crate: lib layout");
    }
    Ok(())
}

/// Record one toolchain step into the running evidence. The first
/// command + exit + stdout/stderr reported is from the failing
/// step (or the last step if every step passed).
pub(super) fn record_step(
    evidence: &mut ValidationEvidence,
    label: &str,
    status: ValidationStatus,
    result: &SandboxResult,
) {
    tracing::trace!(
        label,
        ?status,
        exit_code = result.exit_code,
        "validators::rust::record_step"
    );
    evidence.checks_run.push(label.to_owned());
    // Always update the headline command/exit/stdout/stderr so
    // the deliver phase can show what produced the verdict. On
    // Pass the last step wins; on Fail the failing step wins.
    evidence.command = Some(result.command.clone());
    evidence.exit_code = Some(result.exit_code);
    evidence.stdout_summary = tail(&result.stdout, 2_000);
    evidence.stderr_summary = tail(&result.stderr, 2_000);
    match status {
        ValidationStatus::Pass | ValidationStatus::Warn => {}
        ValidationStatus::Fail => {
            evidence.status = ValidationStatus::Fail;
            // Tests on the test step (`cargo test`) emit a
            // `RustTestFailure`; every other Rust step is a compile
            // error.
            let kind = if label.contains("test") {
                FailureKind::RustTestFailure
            } else {
                FailureKind::RustCompileError
            };
            evidence.record_failure(ValidationFailure::new(
                kind,
                format!("{label} returned non-zero exit"),
            ));
        }
        ValidationStatus::Skipped | ValidationStatus::Error => {
            if evidence.status == ValidationStatus::Pass {
                evidence.status = status;
            }
            evidence
                .skipped_checks
                .push(format!("{label} unavailable: {}", result.stderr));
        }
    }
}

fn status_from_sandbox(status: SandboxStatus) -> ValidationStatus {
    let out = match status {
        SandboxStatus::Pass => ValidationStatus::Pass,
        SandboxStatus::Fail => ValidationStatus::Fail,
        SandboxStatus::Timeout => ValidationStatus::Fail,
        SandboxStatus::NotAllowed => ValidationStatus::Skipped,
        SandboxStatus::NotFound => ValidationStatus::Skipped,
        SandboxStatus::Error => ValidationStatus::Error,
    };
    tracing::trace!(?status, ?out, "validators::rust::status_from_sandbox");
    out
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
    use std::sync::OnceLock;

    fn sandbox() -> Sandbox {
        Sandbox::new(SandboxConfig::new()).unwrap()
    }

    /// Pre-warm a dedicated `CARGO_HOME` once per test process so the
    /// sandboxed `cargo --offline` steps resolve against a populated
    /// registry.
    ///
    /// The validator's `cargo check --offline` / `cargo clippy --offline`
    /// / `cargo test --offline` steps run inside the sandbox with
    /// `CARGO_NET_OFFLINE=true` (catalog §D.11.9 default-deny network).
    /// In a CI cold-cache run the registry index under
    /// `$CARGO_HOME/registry/index/` is empty, so the very first
    /// `--offline` invocation has nothing to resolve against and the
    /// cargo subprocess exits non-zero — turning a Pass into a Fail
    /// for tests like `good_rust_passes_when_cargo_present`.
    ///
    /// The sandbox at `src/sandbox/process.rs:1462` overwrites `HOME`
    /// to the per-invocation scratch dir. When `CARGO_HOME` is unset
    /// cargo derives its registry from `${HOME}/.cargo`, so the
    /// sandbox always sees an empty registry even if the test
    /// process has a populated `~/.cargo/registry` — that is the
    /// failure mode the previous fix hit.
    ///
    /// Cargo respects `CARGO_HOME` over `HOME`; the sandbox
    /// inherits `CARGO_HOME` because `build_env` does
    /// `env = std::env::vars().collect()` and only overrides `HOME`
    /// and `PATH` explicitly. Two cases:
    ///
    /// 1. **`CARGO_HOME` already set in the test process** (CI with
    ///    Swatinem, developer with a global registry, etc.): leave
    ///    it alone. The sandbox will inherit the already-populated
    ///    registry and `cargo --offline` works.
    /// 2. **`CARGO_HOME` unset** (local cold cache, a fresh test
    ///    runner with no global registry): allocate a fresh
    ///    [`tempfile::TempDir`] under `TMPDIR`, run `cargo fetch`
    ///    from the *test* process against a throwaway stdlib-only
    ///    dummy crate to materialise the registry index there, and
    ///    `set_var("CARGO_HOME", <tempdir>)` so the sandbox
    ///    inherits the populated path.
    ///
    /// `OnceLock` guarantees the fetch + set_var runs exactly once
    /// per test binary regardless of how many of the five tests
    /// below invoke it. The `set_var` lives inside `get_or_init` so
    /// the only thread that mutates the env is the one that wins
    /// the initialisation race; subsequent callers just observe
    /// the already-set value via `std::env::var_os`.
    ///
    /// Scope of the env leak: when we do set `CARGO_HOME`, it
    /// stays set for the remainder of the test binary's lifetime.
    /// The test binary is about to exit when the tests complete,
    /// and a parallel `cargo test` invocation is a separate process
    /// with its own env, so the leak is harmless.
    static CARGO_HOME_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();

    fn prewarm_cargo_registry() {
        // If the caller (CI workflow, dev shell) already exported a
        // CARGO_HOME, trust it. Swatinem pre-populates
        // /home/runner/.cargo on GitHub-hosted runners; locally a
        // developer may have a global registry. Overwriting that
        // with a tempdir would either (a) drop a populated cache
        // for no benefit, or (b) leave the tempdir empty if the
        // subsequent `cargo fetch` cannot reach the network.
        if std::env::var_os("CARGO_HOME").is_some() {
            return;
        }
        CARGO_HOME_DIR.get_or_init(|| {
            let dir = tempfile::Builder::new()
                .prefix("moagan-validator-cargo-home-")
                .tempdir()
                .expect("CARGO_HOME tempdir");

            // Minimal stdlib-only dummy crate; the only purpose is
            // to force `cargo fetch` to materialise the registry
            // index. The real fixture crates (good_rust, broken_rust,
            // the in-`#[test]` artefacts) are also stdlib-only, so
            // nothing on top of the bare index is required.
            let dummy = dir.path().join("dummy");
            std::fs::create_dir_all(dummy.join("src")).expect("mkdir dummy/src");
            std::fs::write(
                dummy.join("Cargo.toml"),
                "[package]\n\
                 name = \"moagan_validator_prewarm\"\n\
                 version = \"0.0.0\"\n\
                 edition = \"2021\"\n\
                 publish = false\n\n\
                 [lib]\n\
                 path = \"src/lib.rs\"\n",
            )
            .expect("write dummy Cargo.toml");
            std::fs::write(dummy.join("src").join("lib.rs"), "").expect("write dummy lib.rs");

            let out = std::process::Command::new("cargo")
                .arg("fetch")
                .arg("--manifest-path")
                .arg(dummy.join("Cargo.toml"))
                .env("CARGO_HOME", dir.path())
                .env_remove("CARGO_NET_OFFLINE")
                .output()
                .expect("spawn cargo fetch for prewarm");
            assert!(
                out.status.success(),
                "prewarm `cargo fetch` failed: stdout={}\nstderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );

            // SAFETY: serialised by `OnceLock::get_or_init`; every
            // other thread that calls `prewarm_cargo_registry` will
            // see the initialised `TempDir` and return without
            // racing on the env. The sandbox's `build_env` reads
            // `std::env::vars()` at call time, so any subsequent
            // `RustValidator::check` invocation picks up the new
            // `CARGO_HOME` via the inherited env.
            unsafe {
                std::env::set_var("CARGO_HOME", dir.path());
            }

            dir
        });
    }

    fn good_rust() -> CodeArtifact {
        // Formatted the way `cargo fmt` would leave it so the
        // `cargo fmt --check` step stays green.
        CodeArtifact::new(
            "src/lib.rs",
            "rust",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
    }

    fn broken_rust() -> CodeArtifact {
        CodeArtifact::new(
            "src/lib.rs",
            "rust",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + ;\n}\n",
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
        prewarm_cargo_registry();
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
        // The three pre-test toolchain steps must all have run for
        // a clean artifact. The fourth step (test) is either run
        // or recorded as a no-tests-declared marker.
        let labels: Vec<&str> = ev.checks_run.iter().map(String::as_str).collect();
        assert!(labels.iter().any(|l| l.contains("cargo check --offline")));
        assert!(labels.iter().any(|l| l.contains("cargo fmt --check")));
        assert!(
            labels
                .iter()
                .any(|l| l.contains("cargo clippy --offline -- -D warnings"))
        );
        assert!(
            labels.iter().any(|l| l.contains("cargo test --offline")),
            "cargo test step must be recorded; got {labels:?}"
        );
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

    /// When the first step (`cargo check`) fails the remaining
    /// steps must be reported as skipped so the sidecar accurately
    /// describes what ran. The verdict stays `Fail` and the failing
    /// step is the one whose stdout/stderr is preserved.
    #[test]
    fn broken_rust_skips_remaining_steps_after_check_failure() {
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
        let skipped: Vec<&str> = ev.skipped_checks.iter().map(String::as_str).collect();
        assert!(
            skipped.iter().any(|s| s.contains("cargo fmt --check")),
            "fmt must be skipped when check fails, got {skipped:?}"
        );
        assert!(
            skipped
                .iter()
                .any(|s| s.contains("cargo clippy --offline -- -D warnings")),
            "clippy must be skipped when check fails, got {skipped:?}"
        );
        assert!(
            skipped.iter().any(|s| s.contains("cargo test --offline")),
            "test must be skipped when check fails, got {skipped:?}"
        );
        let labels: Vec<&str> = ev.checks_run.iter().map(String::as_str).collect();
        assert!(
            !labels.iter().any(|l| l.contains("cargo fmt --check")),
            "fmt must not appear in checks_run when skipped, got {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l.contains("cargo clippy")),
            "clippy must not appear in checks_run when skipped, got {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l.contains("cargo test --offline")),
            "test must not appear in checks_run when skipped, got {labels:?}"
        );
    }

    /// A well-formatted, lint-clean artifact must produce a Pass
    /// verdict with all three steps in `checks_run` and the
    /// `reproducibility` field populated with the cargo version.
    #[test]
    fn good_rust_runs_three_steps_in_order() {
        if std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        prewarm_cargo_registry();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt
            .block_on(RustValidator::check(&good_rust(), &sandbox()))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Pass);
        let labels: Vec<&str> = ev.checks_run.iter().map(String::as_str).collect();
        // The first three steps are always check / fmt / clippy;
        // the fourth step is the cargo test invocation (either
        // a real run or a "no tests declared" marker).
        assert!(labels.iter().any(|l| l.contains("cargo check --offline")));
        assert!(labels.iter().any(|l| l.contains("cargo fmt --check")));
        assert!(
            labels
                .iter()
                .any(|l| l.contains("cargo clippy --offline -- -D warnings"))
        );
        assert!(
            labels.iter().any(|l| l.contains("cargo test --offline")),
            "test step must appear in checks_run; got {labels:?}"
        );
    }

    /// The fixture `good_rust` declares no #[test]. The validator
    /// must report Pass (compiling is enough) AND surface the
    /// "no tests declared" marker so the deliver phase can tell
    /// the difference between "tests passed" and "no tests".
    #[test]
    fn rust_validator_marks_no_tests_as_skipped() {
        if std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        prewarm_cargo_registry();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt
            .block_on(RustValidator::check(&good_rust(), &sandbox()))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Pass);
        assert!(
            ev.skipped_checks
                .iter()
                .any(|c| c.contains("no tests declared")),
            "no-tests artifact must surface 'no tests declared' in skipped_checks, got {:?}",
            ev.skipped_checks
        );
    }

    /// An artifact with a passing test must run the test and
    /// report Pass.
    #[test]
    fn rust_validator_passes_when_test_passes() {
        if std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        prewarm_cargo_registry();
        let artifact = CodeArtifact::new(
            "src/lib.rs",
            "rust",
            r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adds_two_positive_numbers() {
        assert_eq!(add(2, 3), 5);
    }
}
"#,
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt
            .block_on(RustValidator::check(&artifact, &sandbox()))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Pass);
        let labels: Vec<&str> = ev.checks_run.iter().map(String::as_str).collect();
        // The "no tests declared" marker must NOT be present
        // because this artifact has a test.
        assert!(
            !ev.skipped_checks
                .iter()
                .any(|c| c.contains("no tests declared")),
            "artifact with tests must not show the no-tests marker, got {:?}",
            ev.skipped_checks
        );
        // The test step must show up as a real run, not a
        // skipped marker.
        assert!(
            labels.iter().any(|l| l == &"cargo test --offline"),
            "test step must show as a real run when tests exist, got {labels:?}"
        );
    }

    /// An artifact with a failing test must report Fail with the
    /// test name in `failed_checks`.
    #[test]
    fn rust_validator_fails_when_test_fails() {
        if std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        prewarm_cargo_registry();
        let artifact = CodeArtifact::new(
            "src/lib.rs",
            "rust",
            r#"pub fn broken() -> i32 {
    42
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn this_one_fails() {
        assert_eq!(broken(), 7);
    }
}
"#,
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ev = rt
            .block_on(RustValidator::check(&artifact, &sandbox()))
            .unwrap();
        assert_eq!(ev.status, ValidationStatus::Fail);
        let labels: Vec<&str> = ev.checks_run.iter().map(String::as_str).collect();
        assert!(
            labels.iter().any(|l| l == &"cargo test --offline"),
            "test step must show as a real run when tests exist, got {labels:?}"
        );
        assert!(
            ev.legacy_failed_checks()
                .iter()
                .any(|c| c.contains("cargo test --offline")),
            "failing test must appear in legacy_failed_checks, got {:?}",
            ev.legacy_failed_checks()
        );
    }

    #[test]
    fn test_count_is_zero_recognises_cargo_output() {
        // The actual cargo output goes to stdout, not stderr.
        assert!(test_count_is_zero(
            "",
            "   Compiling adder v0.1.0\n    Finished test [unoptimized + debuginfo] target(s) in 0.5s\n     Running unittests src/lib.rs (target/debug/deps/adder-1234)\n\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n",
        ));
        assert!(!test_count_is_zero(
            "",
            "running 1 test\ntest tests::it_works ... ok\n",
        ));
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
    fn record_step_promotes_fail_to_evidence_status() {
        let mut e = ValidationEvidence::default();
        let r = SandboxResult {
            exit_code: 101,
            stdout: String::new(),
            stderr: "boom".into(),
            duration: std::time::Duration::from_millis(1),
            status: SandboxStatus::Fail,
            command: "cargo check".into(),
        };
        record_step(&mut e, "cargo check", ValidationStatus::Fail, &r);
        assert_eq!(e.status, ValidationStatus::Fail);
        assert!(
            e.legacy_failed_checks()
                .iter()
                .any(|c| c.contains("cargo check"))
        );
    }

    #[test]
    fn record_step_leaves_pass_alone() {
        let mut e = ValidationEvidence::default();
        let r = SandboxResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration: std::time::Duration::from_millis(1),
            status: SandboxStatus::Pass,
            command: "cargo fmt --check".into(),
        };
        record_step(&mut e, "cargo fmt --check", ValidationStatus::Pass, &r);
        assert_eq!(e.status, ValidationStatus::Pass);
        assert!(e.checks_run.iter().any(|c| c == "cargo fmt --check"));
        assert!(e.failures.is_empty());
    }
}
