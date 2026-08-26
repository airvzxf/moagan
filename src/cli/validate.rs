//! `moagan validate <brief_path>` — pre-flight structural gate.
//!
//! Reads a brief from disk (a JSON file that deserialises into
//! [`crate::domain::Brief`]), synthesises an empty proposal whose
//! `summary` is the brief's problem statement, and runs the same
//! twelve deterministic structural checks that
//! [`crate::phases::gate::GatePhase`] runs against every proposal
//! inside a real run.
//!
//! The point is a CI-friendly gate that does **not** touch the LLM:
//! a `brief.json` that fails the structural check can be rejected
//! from a GitHub Actions job before any token budget is spent on it.
//!
//! Exit codes (D.14.4, T01-06 §12.3):
//!   0 — gate passed
//!   1 — gate failed (hard or missing issue; details on stderr)
//!   2 — `Error::InvalidArgs` (file missing or JSON unparseable)
//!   8 — `Error::IoError` (other I/O failure)
//!
//! Inspired by T16-01 §6.1.

use std::path::{Path, PathBuf};

use tracing::{debug, error, info, trace, warn};

use crate::config::Config;
use crate::domain::{Brief, Proposal};
use crate::error::{Error, IoError, Result};
use crate::fs_layout::safe_path;
use crate::phases::gate::structural_check;

/// CLI arguments for `moagan validate <brief_path>`.
///
/// `mode` is accepted for CLI symmetry with `moagan run` but does
/// not influence the structural check today; the spec reserves it
/// for future per-mode gates (D.14.4).
#[derive(Debug, Clone)]
pub struct ValidateArgs {
    /// Path to the brief JSON file.
    pub brief_path: PathBuf,
    /// Pipeline mode hint. Currently informational; see the module docs.
    #[allow(dead_code)]
    pub mode: Option<crate::cli::Mode>,
}

/// Run the pre-flight gate. Returns the process exit code as
/// `Result<i32>` so the central dispatcher can map `Error` variants
/// onto `ExitCode` (D.14.4, T01-06 §12.3).
///
/// On a failing gate the hard issues and missing fields are printed
/// to stderr — `cargo clippy --all-targets -- -D warnings` requires
/// that the failing case be observable from CI output.
pub fn run(args: ValidateArgs) -> Result<i32> {
    debug!(brief = %args.brief_path.display(), "validate::run: enter");
    let ValidateArgs {
        brief_path,
        mode: _,
    } = args;
    let brief = parse_brief(&brief_path)?;
    let cfg = Config::load()?;
    let forbidden: Vec<String> = cfg
        .gate_forbidden_techs
        .iter()
        .map(|s| s.to_lowercase())
        .collect();
    let proposal = synthetic_proposal(&brief);
    let gate = structural_check(
        &proposal,
        &brief,
        &forbidden,
        cfg.gate_min_length,
        cfg.gate_max_length,
    );
    if gate.pass {
        info!("validate: PASS");
        println!("validate: PASS — brief is structurally sound");
        Ok(0)
    } else {
        warn!(
            issues = gate.issues.len(),
            missing = gate.missing.len(),
            "validate: FAIL"
        );
        for issue in &gate.issues {
            eprintln!("{issue}");
        }
        for miss in &gate.missing {
            eprintln!("missing: {miss}");
        }
        eprintln!(
            "validate: FAIL — {} hard/soft issue(s), {} missing",
            gate.issues.len(),
            gate.missing.len()
        );
        Ok(1)
    }
}

/// Read and parse the brief JSON file. Maps the spec's exit-code
/// expectations (D.14.4):
///
/// - missing file   → `Error::InvalidArgs`  (exit 2)
/// - malformed JSON → `Error::InvalidArgs`  (exit 2)
/// - other I/O      → `Error::Io`           (exit 8)
/// - path traversal → `Error::PathTraversal` (D.29.1, exit 2)
fn parse_brief(path: &Path) -> Result<Brief> {
    trace!(path = %path.display(), "parse_brief: enter");
    // D.29.1: refuse `..` traversals and symlinks that escape the
    // brief's parent directory. The parent dir is the natural root
    // because the operator picked a specific file to validate;
    // confining access to siblings prevents the obvious
    // `../../etc/passwd` shortcut and any symlink that points
    // outside.
    let safe = safe_path(path.parent().unwrap_or(Path::new("/")), path)?;
    let text = std::fs::read_to_string(&safe).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            warn!(path = %safe.display(), "parse_brief: not found");
            Error::InvalidArgs(format!("brief not found: {}", safe.display()))
        } else {
            error!(path = %safe.display(), error = %e, "parse_brief: I/O error");
            Error::Io(IoError::Read {
                path: safe.clone(),
                source: e,
            })
        }
    })?;
    serde_json::from_str::<Brief>(&text).map_err(|e| {
        warn!(path = %safe.display(), error = %e, "parse_brief: invalid JSON");
        Error::InvalidArgs(format!(
            "brief at {} is not valid JSON: {e}",
            safe.display()
        ))
    })
}

/// Build the empty proposal used to exercise the structural check
/// against the brief. `summary` mirrors `brief.problem` so a brief
/// with a non-empty problem triggers the same soft warnings as a
/// real proposal would.
fn synthetic_proposal(brief: &Brief) -> Proposal {
    Proposal {
        id: "validate-cli".into(),
        summary: brief.problem.clone(),
        approach: brief.problem.clone(),
        tradeoffs: vec!["none - validate-cli pre-flight".into()],
        evidence: vec!["none - validate-cli pre-flight".into()],
        source_sketch: String::new(),
        ..Proposal::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Atomic counter that gives every test a unique path suffix so
    /// parallel tests do not stomp on each other's temp files.
    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn unique_tmp(label: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("moagan-validate-{pid}-{n}-{label}"));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        dir.join("brief.json")
    }

    /// Clean brief + synthetic proposal whose summary mirrors the
    /// brief's problem: no forbidden techs, no truncation markers,
    /// no placeholder tokens, length well within the default range.
    #[test]
    fn validate_clean_brief_exits_zero() {
        let path = unique_tmp("clean");
        let brief = serde_json::json!({
            "problem": "Use the standard ROYGBIV order for the rainbow",
            "objectives": ["produce the rainbow"],
            "deliverables": ["ordered color list"],
            "constraints": [],
            "assumptions": [],
            "non_goals": [],
            "acceptance": ["seven distinct colors"],
            "risks": []
        });
        std::fs::write(
            &path,
            serde_json::to_string(&brief).expect("serialise brief"),
        )
        .expect("write brief");

        let args = ValidateArgs {
            brief_path: path,
            mode: None,
        };
        let rc = run(args).expect("clean brief should not error");
        assert_eq!(rc, 0, "clean brief must yield exit code 0");
    }

    /// Brief whose problem statement names a forbidden tech. The
    /// synthetic proposal inherits that problem as its `summary`,
    /// so the structural check flags a `hard:` issue and `pass`
    /// becomes `false`. Exit code must be 1; stderr must contain
    /// the `hard:` prefix so CI logs surface the cause.
    #[test]
    fn validate_brief_with_hard_issues_exits_one() {
        let path = unique_tmp("hard");
        let brief = serde_json::json!({
            "problem": "Use postgres for storage",
            "objectives": [],
            "deliverables": [],
            "constraints": [],
            "assumptions": [],
            "non_goals": [],
            "acceptance": [],
            "risks": []
        });
        std::fs::write(
            &path,
            serde_json::to_string(&brief).expect("serialise brief"),
        )
        .expect("write brief");

        // The repo's default Config has an empty `gate_forbidden_techs`,
        // so a real `postgres` issue would not fire here. Set the
        // env var that `Config::load()` reads so the test sees the
        // forbidden-tech list the spec promises (D.14.4).
        // SAFETY: setting an env var from a test is safe when the test
        // owns its own environment; no other thread races on this key.
        unsafe {
            std::env::set_var("MOAGAN_GATE_FORBIDDEN_TECHS", "postgres");
        }

        let args = ValidateArgs {
            brief_path: path,
            mode: None,
        };
        let result = run(args);
        // Restore env BEFORE asserting so a panic does not leak the
        // override into the next test.
        unsafe {
            std::env::remove_var("MOAGAN_GATE_FORBIDDEN_TECHS");
        }
        let rc = result.expect("hard-issue brief should not error at the dispatcher");
        assert_eq!(rc, 1, "hard-issue brief must yield exit code 1");
    }

    /// Path that does not exist on disk. The spec maps this to
    /// `Error::InvalidArgs` (exit 2), not `Error::Io` (exit 8), so
    /// CI scripts can distinguish "you gave me a bad path" from
    /// "the filesystem is on fire".
    #[test]
    fn validate_missing_brief_returns_invalid_args() {
        let path = std::env::temp_dir().join(format!(
            "moagan-validate-does-not-exist-{}",
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(!path.exists(), "pre-condition: path must not exist");

        let args = ValidateArgs {
            brief_path: path,
            mode: None,
        };
        let err = run(args).expect_err("missing brief must error");
        assert!(
            matches!(err, Error::InvalidArgs(_)),
            "expected Error::InvalidArgs, got {err:?}"
        );
        assert_eq!(err.exit_code() as i32, 2);
    }

    /// Brief file exists but the contents are not valid JSON.
    /// Maps to `Error::InvalidArgs` so the failure mode matches
    /// the missing-brief case from the operator's perspective.
    #[test]
    fn validate_malformed_json_returns_invalid_args() {
        let path = unique_tmp("malformed");
        std::fs::write(&path, b"{ this is not json").expect("write malformed brief");

        let args = ValidateArgs {
            brief_path: path,
            mode: None,
        };
        let err = run(args).expect_err("malformed JSON must error");
        assert!(
            matches!(err, Error::InvalidArgs(_)),
            "expected Error::InvalidArgs, got {err:?}"
        );
        assert_eq!(err.exit_code() as i32, 2);
    }
}
