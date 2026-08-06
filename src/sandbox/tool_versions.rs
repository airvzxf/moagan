//! D.11.12: capture tool versions for reproducibility.
//!
//! Runs `rustc --version` and `cargo --version` once at startup
//! and caches the output. Results are serializable to
//! `tool_versions.json` for export.

use serde::{Deserialize, Serialize};
use std::process::Command;

/// Snapshot of the toolchain that produced a given run.
///
/// All three fields are populated at startup via [`ToolVersions::capture`].
/// `rustc` and `cargo` are best-effort: if the binary is missing
/// (e.g. a minimal container without rustup) the corresponding field
/// is `None` and the export layer marks the run as having
/// `ReproducibilityMissing` evidence rather than failing the run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolVersions {
    /// `rustc --version` output, trimmed. `None` if the binary is
    /// missing or returned a non-zero exit code.
    pub rustc: Option<String>,
    /// `cargo --version` output, trimmed. `None` if the binary is
    /// missing or returned a non-zero exit code.
    pub cargo: Option<String>,
    /// `moagan` itself, taken from `CARGO_PKG_VERSION` at compile
    /// time. Always populated; never `None`.
    pub moagan: String,
}

impl ToolVersions {
    /// Run `rustc --version` and `cargo --version` and bundle the
    /// results with the current `moagan` version. Never panics on
    /// a missing tool — see [`ToolVersions`] for the rationale.
    pub fn capture() -> Self {
        let rustc = run_capture("rustc", &["--version"]);
        let cargo = run_capture("cargo", &["--version"]);
        let moagan = env!("CARGO_PKG_VERSION").to_string();
        Self {
            rustc,
            cargo,
            moagan,
        }
    }
}

fn run_capture(bin: &str, args: &[&str]) -> Option<String> {
    Command::new(bin).args(args).output().ok().and_then(|o| {
        if o.status.success() {
            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `moagan` is the crate version, not a probe — it must always
    /// be populated from `CARGO_PKG_VERSION` regardless of whether
    /// `rustc` or `cargo` are reachable.
    #[test]
    fn tool_versions_captures_moagan_from_env() {
        let v = ToolVersions::capture();
        assert_eq!(v.moagan, env!("CARGO_PKG_VERSION"));
        assert!(!v.moagan.is_empty(), "CARGO_PKG_VERSION must be set");
    }

    /// Probing a non-existent binary must return `None` instead of
    /// panicking. The struct must still be constructible and
    /// serializable with both optional fields unset.
    #[test]
    fn tool_versions_returns_none_for_missing_binary() {
        // A name that is guaranteed not to exist on PATH; even on a
        // pathological host with such a binary the helper still
        // returns `Option<String>` and the struct stays valid.
        let got = run_capture("moagan-nonexistent-binary-xyz-9876", &["--version"]);
        assert!(got.is_none());
        let v = ToolVersions {
            rustc: None,
            cargo: None,
            moagan: "0.0.0-test".to_string(),
        };
        assert!(v.rustc.is_none());
        assert!(v.cargo.is_none());
    }

    /// The struct is the wire form for `tool_versions.json`; round
    /// trips through `serde_json` without losing any field.
    #[test]
    fn tool_versions_serializes_to_json() {
        let v = ToolVersions {
            rustc: Some("rustc 1.97.1 (abcdef 2026-01-01)".to_string()),
            cargo: Some("cargo 1.97.1 (abcdef 2026-01-01)".to_string()),
            moagan: "0.4.0".to_string(),
        };
        let json = serde_json::to_string(&v).expect("serialize");
        assert!(json.contains("\"rustc\""));
        assert!(json.contains("\"cargo\""));
        assert!(json.contains("\"moagan\":\"0.4.0\""));
        let back: ToolVersions = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.moagan, v.moagan);
        assert_eq!(back.rustc, v.rustc);
        assert_eq!(back.cargo, v.cargo);
    }
}
