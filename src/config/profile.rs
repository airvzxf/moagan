//! Domain-specific profile loaded from TOML.
//!
//! Profiles override defaults from `Config` for specific problem
//! domains (cryptography, ML, distributed systems, frontend).
//!
//! Schema:
//! ```toml
//! extends = "base"
//! gate_forbidden_techs = ["react", "redux"]
//! gate_min_length = 100
//! gate_max_length = 8000
//! temperature_overrides = { "rust_competitive" = 0.3 }
//! judge_quorum_overrides = { "fast" = 1 }
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoError, Result};

/// Domain-specific configuration profile, deserialised from a
/// TOML file under `$MOAGAN_HOME/profiles/` (with a
/// `~/.config/moagan/profiles/` fallback). The TOML schema is
/// documented in the module-level comment.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Optional parent profile name. When set, the parent's
    /// fields populate any unset fields on this profile during
    /// `Profile::load`. Cycle detection in the extends chain
    /// returns `Error::InvalidArgs` rather than infinite
    /// recursion.
    #[serde(default)]
    pub extends: Option<String>,
    /// Additional tech strings the gate rejects for this
    /// domain. Merged (unioned + deduped) with the parent's
    /// list when inheritance applies.
    #[serde(default)]
    pub gate_forbidden_techs: Vec<String>,
    /// Optional override for the gate's minimum proposal length.
    /// `None` means "inherit or use the config default".
    #[serde(default)]
    pub gate_min_length: Option<usize>,
    /// Optional override for the gate's maximum proposal length.
    /// `None` means "inherit or use the config default".
    #[serde(default)]
    pub gate_max_length: Option<usize>,
    /// Per-role temperature overrides keyed by role name.
    /// Consulted by phases that opt to read them; child entries
    /// win on key collision with the parent profile.
    #[serde(default)]
    pub temperature_overrides: HashMap<String, f32>,
    /// Per-mode judge quorum overrides keyed by mode name.
    /// Consulted by future per-mode judge count wiring; child
    /// entries win on key collision with the parent profile.
    #[serde(default)]
    pub judge_quorum_overrides: HashMap<String, usize>,
}

impl Profile {
    /// Construct an empty profile. Equivalent to `Profile::default()`
    /// but reads better at call sites.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load a profile by short name from the standard search
    /// paths (`$MOAGAN_HOME/profiles/<name>.toml` then
    /// `~/.config/moagan/profiles/<name>.toml`). Returns
    /// `Err(Error::InvalidArgs)` when no file matches or when
    /// the `extends` chain has a cycle.
    pub fn load(name: &str) -> Result<Self> {
        Self::load_with_history(name, &mut Vec::new())
    }

    /// Merge this profile (self) on top of a parent. List fields
    /// are unioned (`parent ++ self` then deduped); scalar
    /// `Option`s fall back to the parent when unset on self;
    /// the temperature / judge-quorum maps take the parent as
    /// a baseline and overlay self entries on top so self wins
    /// on key collision.
    pub fn merge_with(mut self, parent: &Profile) -> Self {
        let mut forbidden = parent.gate_forbidden_techs.clone();
        forbidden.extend(self.gate_forbidden_techs.iter().cloned());
        forbidden.sort();
        forbidden.dedup();
        self.gate_forbidden_techs = forbidden;
        if self.gate_min_length.is_none() {
            self.gate_min_length = parent.gate_min_length;
        }
        if self.gate_max_length.is_none() {
            self.gate_max_length = parent.gate_max_length;
        }
        let mut temps = parent.temperature_overrides.clone();
        temps.extend(self.temperature_overrides);
        self.temperature_overrides = temps;
        let mut quorums = parent.judge_quorum_overrides.clone();
        quorums.extend(self.judge_quorum_overrides);
        self.judge_quorum_overrides = quorums;
        self
    }

    /// True when every field is unset. Useful for the CLI path
    /// where `--profile ""` should be a no-op rather than load
    /// the literal profile named `""`.
    pub fn is_empty(&self) -> bool {
        self.extends.is_none()
            && self.gate_forbidden_techs.is_empty()
            && self.gate_min_length.is_none()
            && self.gate_max_length.is_none()
            && self.temperature_overrides.is_empty()
            && self.judge_quorum_overrides.is_empty()
    }

    /// Look up a per-role temperature override by role name (e.g.
    /// `"sketch"` or `"judge"`). Returns `None` when the profile
    /// does not override that role — callers should fall back to
    /// the hard-coded role default in `phases::phase`.
    pub fn temperature_for(&self, role: &str) -> Option<f32> {
        self.temperature_overrides.get(role).copied()
    }

    /// Look up a per-mode judge quorum override by mode name (e.g.
    /// `"fast"` or `"deep"`). Returns `None` when the profile does
    /// not override that mode — callers should fall back to
    /// [`crate::phases::cardinality::judge_quorum`].
    pub fn judge_quorum_for(&self, mode: &str) -> Option<usize> {
        self.judge_quorum_overrides.get(mode).copied()
    }

    fn load_with_history(name: &str, chain: &mut Vec<String>) -> Result<Self> {
        if chain.iter().any(|ancestor| ancestor == name) {
            return Err(Error::InvalidArgs(format!(
                "circular profile extends chain detected: {} -> {name}",
                chain.join(" -> ")
            )));
        }
        chain.push(name.to_owned());
        let path = match locate(name) {
            Some(p) => p,
            None => {
                chain.pop();
                return Err(Error::InvalidArgs(format!(
                    "profile '{name}' not found in MOAGAN_HOME or XDG paths"
                )));
            }
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                chain.pop();
                return Err(Error::Io(IoError::Read {
                    path: path.clone(),
                    source: e,
                }));
            }
        };
        let parsed = match toml::from_str::<Profile>(&text) {
            Ok(p) => p,
            Err(e) => {
                chain.pop();
                return Err(Error::Io(IoError::Parse {
                    path: path.clone(),
                    source: Box::new(e),
                }));
            }
        };
        let result = if let Some(parent_name) = &parsed.extends {
            let parent = Profile::load_with_history(parent_name, chain)?;
            Ok(parsed.merge_with(&parent))
        } else {
            Ok(parsed)
        };
        // Pop on the way out so siblings (e.g. when a profile
        // appears in multiple independent chains) don't see stale
        // ancestors. The cycle-detection loop above is a
        // membership check against the *current* path, not a
        // global set, so we keep our own frame's name on the
        // chain while we recurse into `parent_name` and clear it
        // before returning.
        chain.pop();
        result
    }
}

fn locate(name: &str) -> Option<PathBuf> {
    let filename = format!("{name}.toml");
    if let Ok(moagan_home) = std::env::var("MOAGAN_HOME") {
        let p = PathBuf::from(moagan_home).join("profiles").join(&filename);
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home)
            .join(".config")
            .join("moagan")
            .join("profiles")
            .join(&filename);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises tests that mutate the `MOAGAN_HOME` env var so a
    /// parallel run can't observe another test's empty/unset
    /// home mid-write.
    static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn profile_default_is_empty() {
        let p = Profile::default();
        assert!(p.is_empty());
        assert_eq!(p, Profile::empty());
        assert!(p.extends.is_none());
        assert!(p.gate_forbidden_techs.is_empty());
    }

    #[test]
    fn profile_load_returns_not_found_for_missing_name() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::remove_var("MOAGAN_HOME");
        }
        let result = Profile::load("nonexistent-profile-xyz-12345");
        assert!(result.is_err());
        let err = format!("{}", result.err().unwrap());
        assert!(
            err.contains("nonexistent-profile-xyz-12345"),
            "expected name in error: {err}"
        );
    }

    #[test]
    fn profile_load_round_trips_via_toml() {
        let toml = r#"
            gate_forbidden_techs = ["react", "redux"]
            gate_min_length = 100
            gate_max_length = 8000
            [temperature_overrides]
            rust_competitive = 0.3
            [judge_quorum_overrides]
            fast = 1
        "#;
        let parsed: Profile = toml::from_str(toml).expect("parses");
        assert_eq!(parsed.gate_forbidden_techs, vec!["react", "redux"]);
        assert_eq!(parsed.gate_min_length, Some(100));
        assert_eq!(parsed.gate_max_length, Some(8000));
        assert_eq!(
            parsed.temperature_overrides.get("rust_competitive"),
            Some(&0.3)
        );
        assert_eq!(parsed.judge_quorum_overrides.get("fast"), Some(&1));
    }

    #[test]
    fn profile_merge_unions_forbidden_techs() {
        let parent = Profile {
            gate_forbidden_techs: vec!["vue".to_owned(), "react".to_owned()],
            gate_min_length: Some(50),
            gate_max_length: None,
            temperature_overrides: HashMap::new(),
            judge_quorum_overrides: HashMap::new(),
            extends: None,
        };
        let child = Profile {
            gate_forbidden_techs: vec!["react".to_owned(), "redux".to_owned()],
            gate_min_length: None,
            gate_max_length: Some(9000),
            temperature_overrides: HashMap::new(),
            judge_quorum_overrides: HashMap::new(),
            extends: None,
        };
        let merged = child.merge_with(&parent);
        assert_eq!(
            merged.gate_forbidden_techs,
            vec!["react".to_owned(), "redux".to_owned(), "vue".to_owned()]
        );
        assert_eq!(merged.gate_min_length, Some(50));
        assert_eq!(merged.gate_max_length, Some(9000));
        assert!(!merged.is_empty());
    }

    #[test]
    fn profile_inherits_from_parent_via_extends() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        unsafe {
            std::env::set_var("MOAGAN_HOME", dir.path());
        }
        let parent_path = dir.path().join("profiles");
        std::fs::create_dir_all(&parent_path).expect("mkdir");
        std::fs::write(
            parent_path.join("base.toml"),
            r#"
                gate_forbidden_techs = ["react"]
                gate_min_length = 75
            "#,
        )
        .expect("write parent");
        std::fs::write(
            parent_path.join("child.toml"),
            r#"
                extends = "base"
                gate_forbidden_techs = ["redux"]
                gate_max_length = 4000
            "#,
        )
        .expect("write child");

        let loaded = Profile::load("child").expect("loads");
        assert_eq!(
            loaded.gate_forbidden_techs,
            vec!["react".to_owned(), "redux".to_owned()]
        );
        assert_eq!(loaded.gate_min_length, Some(75));
        assert_eq!(loaded.gate_max_length, Some(4000));
        unsafe {
            std::env::remove_var("MOAGAN_HOME");
        }
    }

    #[test]
    fn profile_detects_circular_extends_chain() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        unsafe {
            std::env::set_var("MOAGAN_HOME", dir.path());
        }
        let profiles = dir.path().join("profiles");
        std::fs::create_dir_all(&profiles).expect("mkdir");
        std::fs::write(profiles.join("a.toml"), r#"extends = "b""#).expect("write a");
        std::fs::write(profiles.join("b.toml"), r#"extends = "a""#).expect("write b");

        let result = Profile::load("a");
        assert!(result.is_err());
        let err = format!("{}", result.err().unwrap());
        assert!(
            err.contains("circular"),
            "expected circular chain error, got: {err}"
        );
        unsafe {
            std::env::remove_var("MOAGAN_HOME");
        }
    }
}
