//! D.33.1-4 + D.33.7: Manifest extensions.
//!
//! These are additive wrappers that don't modify the existing
//! `Manifest` struct. Instead, they live as sibling types that
//! get composed at export time.

use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
pub enum HashAlgo {
    Sha256,
    Blake3,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManifestHistory {
    pub at_unix: i64,
    pub from_provider: String,
    pub to_provider: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManifestAlert {
    pub at_unix: i64,
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolVersionsSummary {
    pub rustc: Option<String>,
    pub cargo: Option<String>,
    pub moagan: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManifestExtension {
    pub hash_algo: HashAlgo,
    pub history: Vec<ManifestHistory>,
    pub alerts: Vec<ManifestAlert>,
    pub tool_versions: ToolVersionsSummary,
}

impl ManifestExtension {
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
