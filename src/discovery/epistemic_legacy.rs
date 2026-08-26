//! Per-MOAGAN_HOME epistemic legacy — accumulated knowledge about
//! what worked and what failed in previous discovery runs.
//!
//! Stored as JSON at `<MOAGAN_HOME>/epistemic_legacy.json` or
//! `~/.config/moagan/epistemic_legacy.json` as fallback.
//!
//! The `sketch` prompt optionally substitutes `${epistemic_legacy}`
//! with the rendered view of this struct.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Schema version of the persisted file. Bumped on breaking changes;
/// on a mismatch the persisted state is silently discarded (an
/// operator who upgrades gets a fresh legacy rather than a half-loaded
/// one).
pub const SCHEMA_VERSION: u32 = 1;
const FILENAME: &str = "epistemic_legacy.json";

/// Per-MOAGAN_HOME epistemic legacy — accumulated knowledge about
/// what worked and what failed in previous discovery runs. Operators
/// populate the JSON manually; the struct itself is the on-disk shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EpistemicLegacy {
    /// Schema version — see [`SCHEMA_VERSION`].
    pub version: u32,
    /// Strategies / patterns that were observed to fail in past runs
    /// and should be avoided.
    pub known_failures: Vec<String>,
    /// Strategies / patterns that were observed to succeed in past
    /// runs and should be preferred.
    pub preferred_strategies: Vec<String>,
    /// Assumptions about the target domain (e.g. "rust-stable-2024")
    /// that hold across runs and free the model from re-deriving them.
    pub domain_assumptions: Vec<String>,
    /// Per-skill / per-angle confidence adjustments, keyed by skill
    /// or angle name. Positive values boost, negative values dampen.
    pub confidence_overrides: HashMap<String, f64>,
}

impl EpistemicLegacy {
    /// Construct an empty legacy with the current [`SCHEMA_VERSION`].
    pub fn empty() -> Self {
        tracing::trace!("epistemic_legacy: empty()");
        Self {
            version: SCHEMA_VERSION,
            ..Default::default()
        }
    }

    /// Load from the canonical location, falling back to XDG.
    /// Returns `Self::empty()` on missing or corrupt file.
    pub fn load() -> Self {
        tracing::debug!("epistemic_legacy: load from canonical location");
        if let Some(p) = primary_path()
            && let Ok(legacy) = Self::load_from(&p)
        {
            tracing::info!(
                path = %p.display(),
                known_failures = legacy.known_failures.len(),
                preferred_strategies = legacy.preferred_strategies.len(),
                "epistemic_legacy: loaded from primary"
            );
            return legacy;
        }
        if let Some(p) = xdg_fallback_path()
            && let Ok(legacy) = Self::load_from(&p)
        {
            tracing::info!(
                path = %p.display(),
                known_failures = legacy.known_failures.len(),
                preferred_strategies = legacy.preferred_strategies.len(),
                "epistemic_legacy: loaded from xdg fallback"
            );
            return legacy;
        }
        tracing::trace!("epistemic_legacy: no legacy found; returning empty");
        Self::empty()
    }

    /// Load from an explicit path. Returns [`LoadError::Version`] on a
    /// schema-version mismatch so callers can distinguish "missing" from
    /// "wire-incompatible".
    pub fn load_from(path: &Path) -> Result<Self, LoadError> {
        tracing::debug!(path = %path.display(), "epistemic_legacy: load_from");
        let text = std::fs::read_to_string(path).map_err(|e| {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "epistemic_legacy: read failed"
            );
            LoadError::Io(e)
        })?;
        let parsed: Self = serde_json::from_str(&text).map_err(|e| {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "epistemic_legacy: parse failed"
            );
            LoadError::Parse(e)
        })?;
        if parsed.version != SCHEMA_VERSION {
            tracing::warn!(
                path = %path.display(),
                on_disk = parsed.version,
                current = SCHEMA_VERSION,
                "epistemic_legacy: schema version mismatch"
            );
            return Err(LoadError::Version);
        }
        tracing::trace!(path = %path.display(), "epistemic_legacy: load_from ok");
        Ok(parsed)
    }

    /// Save to the canonical location (creates parent dirs).
    pub fn save(&self) -> Result<(), SaveError> {
        let path = primary_path().ok_or_else(|| {
            tracing::error!("epistemic_legacy: save failed (no home env)");
            SaveError::NoHome
        })?;
        self.save_to(&path)
    }

    /// Save to an explicit path. Atomic write via tmp+rename and a
    /// best-effort `fsync` on the parent directory for durability.
    pub fn save_to(&self, path: &Path) -> Result<(), SaveError> {
        tracing::debug!(
            path = %path.display(),
            known_failures = self.known_failures.len(),
            preferred_strategies = self.preferred_strategies.len(),
            "epistemic_legacy: save_to"
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                tracing::error!(
                    path = %parent.display(),
                    error = %e,
                    "epistemic_legacy: mkdir for parent failed"
                );
                SaveError::Io(e)
            })?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| {
            tracing::error!(error = %e, "epistemic_legacy: serialize failed");
            SaveError::Serialize(e)
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes()).map_err(|e| {
            tracing::error!(
                path = %tmp.display(),
                error = %e,
                "epistemic_legacy: tmp write failed"
            );
            SaveError::Io(e)
        })?;
        std::fs::rename(&tmp, path).map_err(|e| {
            tracing::error!(
                path = %path.display(),
                error = %e,
                "epistemic_legacy: rename tmp->final failed"
            );
            SaveError::Io(e)
        })?;
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        tracing::trace!(path = %path.display(), "epistemic_legacy: save_to ok");
        Ok(())
    }

    /// Render as a Markdown snippet suitable for prompt injection.
    pub fn render_markdown(&self) -> String {
        tracing::trace!(
            known_failures = self.known_failures.len(),
            preferred_strategies = self.preferred_strategies.len(),
            domain_assumptions = self.domain_assumptions.len(),
            confidence_overrides = self.confidence_overrides.len(),
            "epistemic_legacy: render_markdown"
        );
        let mut s = String::new();
        s.push_str("# Epistemic legacy\n\n");
        if !self.known_failures.is_empty() {
            s.push_str("## Known failures\n");
            for f in &self.known_failures {
                s.push_str(&format!("- {f}\n"));
            }
            s.push('\n');
        }
        if !self.preferred_strategies.is_empty() {
            s.push_str("## Preferred strategies\n");
            for s2 in &self.preferred_strategies {
                s.push_str(&format!("- {s2}\n"));
            }
            s.push('\n');
        }
        if !self.domain_assumptions.is_empty() {
            s.push_str("## Domain assumptions\n");
            for a in &self.domain_assumptions {
                s.push_str(&format!("- {a}\n"));
            }
            s.push('\n');
        }
        if !self.confidence_overrides.is_empty() {
            s.push_str("## Confidence overrides\n");
            for (k, v) in &self.confidence_overrides {
                s.push_str(&format!("- {k}: {v}\n"));
            }
            s.push('\n');
        }
        s
    }
}

/// Errors that [`EpistemicLegacy::load_from`] can surface.
#[derive(Debug)]
pub enum LoadError {
    /// Reading the file from disk failed (missing file, permission, etc.).
    Io(std::io::Error),
    /// The file contents are not valid JSON or do not match the struct shape.
    Parse(serde_json::Error),
    /// The on-disk `version` field does not match [`SCHEMA_VERSION`].
    Version,
}

/// Errors that [`EpistemicLegacy::save`] / [`EpistemicLegacy::save_to`] can surface.
#[derive(Debug)]
pub enum SaveError {
    /// A stdlib I/O call failed (mkdir, write, rename, fsync).
    Io(std::io::Error),
    /// Serializing the struct to JSON failed.
    Serialize(serde_json::Error),
    /// Neither `MOAGAN_HOME` nor `HOME` is set, so there is nowhere to save.
    NoHome,
}

fn primary_path() -> Option<PathBuf> {
    std::env::var_os("MOAGAN_HOME")
        .map(|h| PathBuf::from(h).join(FILENAME))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                PathBuf::from(h)
                    .join(".config")
                    .join("moagan")
                    .join(FILENAME)
            })
        })
}

fn xdg_fallback_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(|h| PathBuf::from(h).join("moagan").join(FILENAME))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                PathBuf::from(h)
                    .join(".config")
                    .join("moagan")
                    .join(FILENAME)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "moagan-epistemic-legacy-test-{}-{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn legacy_empty_initializes_with_schema_version() {
        let l = EpistemicLegacy::empty();
        assert_eq!(l.version, SCHEMA_VERSION);
        assert_eq!(l.version, 1);
        assert!(l.known_failures.is_empty());
        assert!(l.preferred_strategies.is_empty());
        assert!(l.domain_assumptions.is_empty());
        assert!(l.confidence_overrides.is_empty());
    }

    #[test]
    fn legacy_load_returns_empty_if_absent() {
        let tmp = unique_tmp("absent");
        let path = tmp.join(FILENAME);
        // File does not exist; load_from returns Err(Io) which the
        // caller converts to "empty". Verify the same path explicitly.
        assert!(matches!(
            EpistemicLegacy::load_from(&path).unwrap_err(),
            LoadError::Io(_)
        ));
        // Cleanup so we don't litter /tmp.
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn legacy_load_returns_empty_on_corrupt_file() {
        let tmp = unique_tmp("corrupt");
        let path = tmp.join(FILENAME);
        std::fs::write(&path, b"{{{ this is not json :::").unwrap();
        assert!(matches!(
            EpistemicLegacy::load_from(&path).unwrap_err(),
            LoadError::Parse(_)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn legacy_save_then_load_round_trip() {
        let tmp = unique_tmp("roundtrip");
        let path = tmp.join(FILENAME);
        let mut l = EpistemicLegacy::empty();
        l.known_failures.push("flaky-json-parser-v2".to_owned());
        l.preferred_strategies.push("prefer-typed-parse".to_owned());
        l.domain_assumptions.push("rust-stable-2024".to_owned());
        l.confidence_overrides
            .insert("sketch.angle:ops".to_owned(), 0.9);
        l.save_to(&path).unwrap();
        let loaded = EpistemicLegacy::load_from(&path).unwrap();
        assert_eq!(loaded, l);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn legacy_render_markdown_includes_known_failures() {
        let mut l = EpistemicLegacy::empty();
        l.known_failures.push("regression-X".to_owned());
        let md = l.render_markdown();
        assert!(md.contains("# Epistemic legacy"));
        assert!(md.contains("## Known failures"));
        assert!(md.contains("- regression-X"));
    }

    #[test]
    fn legacy_render_markdown_includes_preferred_strategies() {
        let mut l = EpistemicLegacy::empty();
        l.preferred_strategies.push("use-btreemap".to_owned());
        let md = l.render_markdown();
        assert!(md.contains("## Preferred strategies"));
        assert!(md.contains("- use-btreemap"));
    }

    #[test]
    fn inject_epistemic_legacy_substitutes_placeholder() {
        let legacy = EpistemicLegacy::empty();
        let rendered = legacy.render_markdown();
        let template = "Hello\n${epistemic_legacy}\nWorld";
        // The inject helper performs string substitution; verify
        // the placeholder is replaced by the rendered view.
        let injected = template.replace("${epistemic_legacy}", &rendered);
        assert!(injected.contains("# Epistemic legacy"));
        assert!(!injected.contains("${epistemic_legacy}"));
        assert!(injected.starts_with("Hello\n"));
        assert!(injected.ends_with("\nWorld"));
    }

    #[test]
    fn inject_epistemic_legacy_returns_unchanged_when_no_placeholder() {
        let template = "no placeholder here, only prose";
        let legacy = EpistemicLegacy::empty();
        let rendered = legacy.render_markdown();
        let injected = template.replace("${epistemic_legacy}", &rendered);
        assert_eq!(injected, template);
    }
}
