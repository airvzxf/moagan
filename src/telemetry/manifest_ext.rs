//! D.33.1-4 + D.33.7: Manifest extensions.
//!
//! These are additive wrappers that don't modify the existing
//! `Manifest` struct. Instead, they live as sibling types that
//! get composed at export time.

use serde::Serialize;

/// Hash algorithm selector for manifest digests.
#[derive(Debug, Clone, Copy, Serialize)]
pub enum HashAlgo {
    /// SHA-256.
    Sha256,
    /// BLAKE3.
    Blake3,
}

/// One entry in the manifest's provider-history log.
#[derive(Debug, Clone, Serialize)]
pub struct ManifestHistory {
    /// Unix seconds when the transition happened.
    pub at_unix: i64,
    /// Provider name being switched away from.
    pub from_provider: String,
    /// Provider name being switched to.
    pub to_provider: String,
    /// Free-text reason for the transition.
    pub reason: String,
}

/// One alert attached to the manifest export.
#[derive(Debug, Clone, Serialize)]
pub struct ManifestAlert {
    /// Unix seconds when the alert was raised.
    pub at_unix: i64,
    /// Severity tag (e.g. `"warn"`, `"error"`).
    pub severity: String,
    /// Human-readable alert body.
    pub message: String,
}

/// Snapshot of tool versions captured at manifest-export time.
#[derive(Debug, Clone, Serialize)]
pub struct ToolVersionsSummary {
    /// `rustc --version` output, when available.
    pub rustc: Option<String>,
    /// `cargo --version` output, when available.
    pub cargo: Option<String>,
    /// `moagan` package version (always populated from
    /// `CARGO_PKG_VERSION`).
    pub moagan: String,
}

/// Additive manifest extension; composed with the canonical
/// manifest at export time without modifying its schema.
#[derive(Debug, Clone, Serialize)]
pub struct ManifestExtension {
    /// Hash algorithm used for the manifest digests.
    pub hash_algo: HashAlgo,
    /// Provider-switch history, oldest first.
    pub history: Vec<ManifestHistory>,
    /// Alerts emitted during the run.
    pub alerts: Vec<ManifestAlert>,
    /// Tool versions used to produce the run.
    pub tool_versions: ToolVersionsSummary,
}

impl ManifestExtension {
    /// Empty extension with SHA-256 default and `moagan`
    /// version populated from `CARGO_PKG_VERSION`.
    pub fn empty() -> Self {
        Self {
            hash_algo: HashAlgo::Sha256,
            history: Vec::new(),
            alerts: Vec::new(),
            tool_versions: ToolVersionsSummary {
                rustc: None,
                cargo: None,
                moagan: env!("CARGO_PKG_VERSION").to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_extension_hash_algo_default_is_sha256() {
        let e = ManifestExtension::empty();
        assert!(matches!(e.hash_algo, HashAlgo::Sha256));
    }

    #[test]
    fn manifest_extension_moagan_from_env() {
        let e = ManifestExtension::empty();
        assert!(!e.tool_versions.moagan.is_empty());
    }

    #[test]
    fn manifest_extension_serializes_to_json() {
        let e = ManifestExtension::empty();
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("hash_algo"));
        assert!(json.contains("tool_versions"));
    }
}
