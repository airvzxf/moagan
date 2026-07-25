//! Redaction policies and the helper that applies them to text.
//!
//! A `RedactPolicy` decides which surfaces are redacted. `apply` walks
//! the active pattern list and replaces matches with `[REDACTED:id]`.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::error::Result;

use super::patterns::{PATTERNS, Pattern};

/// Per-surface redaction toggles. The default matches T01-06 §5.3:
/// redact `telemetry`, `storage`, `export`; pass prompts and briefs
/// through untouched (we redact on writes that *contain* them, not
/// before they reach the provider).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RedactPolicy {
    /// Redact before writing to `telemetry/` files.
    pub telemetry: bool,
    /// Redact before writing other storage artifacts (manifest, briefs).
    pub storage: bool,
    /// Redact before writing export bundles.
    pub export: bool,
    /// Redact before sending as prompt (almost always false; user input).
    pub prompts: bool,
    /// IDs of patterns to keep enabled. `None` = all patterns enabled.
    pub enabled_patterns: Option<Vec<String>>,
}

impl Default for RedactPolicy {
    fn default() -> Self {
        Self {
            telemetry: true,
            storage: true,
            export: true,
            prompts: false,
            enabled_patterns: None,
        }
    }
}

impl RedactPolicy {
    /// Build a policy that disables all redaction (escape hatch).
    pub fn allow_all() -> Self {
        Self {
            telemetry: false,
            storage: false,
            export: false,
            prompts: false,
            enabled_patterns: None,
        }
    }

    /// Check whether redaction is enabled for a given surface.
    pub fn is_enabled(&self, surface: Surface) -> bool {
        match surface {
            Surface::Telemetry => self.telemetry,
            Surface::Storage => self.storage,
            Surface::Export => self.export,
            Surface::Prompts => self.prompts,
        }
    }

    /// Restrict the pattern set to those whose id is in `ids`.
    pub fn with_patterns<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.enabled_patterns = Some(ids.into_iter().map(Into::into).collect());
        self
    }

    /// Return the active patterns for this policy.
    pub fn active_patterns(&self) -> Vec<&Pattern> {
        let all: Vec<&Pattern> = PATTERNS.iter().collect();
        match &self.enabled_patterns {
            None => all,
            Some(enabled) => {
                let set: std::collections::HashSet<&str> =
                    enabled.iter().map(String::as_str).collect();
                all.into_iter().filter(|p| set.contains(p.id)).collect()
            }
        }
    }
}

/// Which surface is being written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// `telemetry/` files.
    Telemetry,
    /// Other filesystem artifacts (manifest, briefs, etc.).
    Storage,
    /// Export bundle outputs.
    Export,
    /// Outgoing LLM prompt payloads.
    Prompts,
}

/// Apply the policy to a text payload. Returns `Cow::Borrowed` if no
/// pattern matched, `Cow::Owned` otherwise.
pub fn apply<'a>(policy: &RedactPolicy, surface: Surface, text: &'a str) -> Result<Cow<'a, str>> {
    if !policy.is_enabled(surface) || text.is_empty() {
        return Ok(Cow::Borrowed(text));
    }
    let mut owned: Option<String> = None;
    for p in policy.active_patterns() {
        let target = match owned.as_ref() {
            Some(s) => s.clone(),
            None => text.to_owned(),
        };
        let replaced = p.re.replace_all(&target, p.replacement);
        if let Cow::Owned(s) = replaced {
            owned = Some(s);
        }
    }
    Ok(match owned {
        Some(s) => Cow::Owned(s),
        None => Cow::Borrowed(text),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_redacts_telemetry_storage_export() {
        let p = RedactPolicy::default();
        assert!(p.is_enabled(Surface::Telemetry));
        assert!(p.is_enabled(Surface::Storage));
        assert!(p.is_enabled(Surface::Export));
        assert!(!p.is_enabled(Surface::Prompts));
    }

    #[test]
    fn allow_all_disables_everything() {
        let p = RedactPolicy::allow_all();
        for s in [
            Surface::Telemetry,
            Surface::Storage,
            Surface::Export,
            Surface::Prompts,
        ] {
            assert!(!p.is_enabled(s));
        }
    }

    #[test]
    fn empty_input_returns_borrowed() {
        let p = RedactPolicy::default();
        let r = apply(&p, Surface::Telemetry, "").unwrap();
        assert!(matches!(r, Cow::Borrowed(_)));
    }

    #[test]
    fn redacts_minimax_key() {
        let p = RedactPolicy::default();
        let text = "Authorization: Bearer sk-cp-abcdef0123456789abcdef0123";
        let r = apply(&p, Surface::Telemetry, text).unwrap();
        assert!(r.contains("[REDACTED:minimax_sk_cp]"));
        assert!(!r.contains("abcdef0123456789"));
    }

    #[test]
    fn redacts_github_pat() {
        let p = RedactPolicy::default();
        let text = "token=ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let r = apply(&p, Surface::Storage, text).unwrap();
        assert!(r.contains("[REDACTED:github_pat]"));
        assert!(!r.contains("ghp_abcdef"));
    }

    #[test]
    fn redacts_address_in_export() {
        let p = RedactPolicy::default();
        let text = "Contact: alice@example.com about issue #42";
        let r = apply(&p, Surface::Export, text).unwrap();
        assert!(r.contains("[REDACTED:email]"));
        assert!(!r.contains("alice@example.com"));
    }

    #[test]
    fn policy_off_keeps_text_intact() {
        let p = RedactPolicy::allow_all();
        let text = "key=sk-cp-abcdef0123456789abcdef0123";
        let r = apply(&p, Surface::Telemetry, text).unwrap();
        assert_eq!(r, text);
    }

    #[test]
    fn patterns_subset_is_filtered() {
        let p = RedactPolicy::default().with_patterns(["email"]);
        let text = "user=alice@example.com key=sk-cp-abcdef0123456789abcdef0123";
        let r = apply(&p, Surface::Telemetry, text).unwrap();
        assert!(r.contains("[REDACTED:email]"));
        assert!(
            r.contains("sk-cp-abcdef"),
            "key stays when only email enabled"
        );
    }

    #[test]
    fn multiple_matches_in_one_text() {
        let p = RedactPolicy::default();
        let text = "k1=sk-cp-aaaaaaaaaaaaaaaaaaaa k2=sk-cp-bbbbbbbbbbbbbbbbbbbb";
        let r = apply(&p, Surface::Telemetry, text).unwrap();
        let count = r.matches("[REDACTED:minimax_sk_cp]").count();
        assert_eq!(count, 2);
    }
}
