//! Redaction policies and the helper that applies them to text.
//!
//! A `RedactPolicy` decides which surfaces are redacted. `apply` walks
//! the active pattern list and replaces matches with `[REDACTED:id]`.
//!
//! The categorised variant `apply_with_categories` (D.8.2) returns
//! both the redacted text and the per-`PatternKind` match counts so
//! the caller can persist them in the `redact_audit` SQLite table
//! (D.8.5 / D.5.1).
//!
//! ## Tracing policy
//!
//! `apply` is the hot path of `RedactWriter`, which IS the tracing
//! subscriber's writer. Any `tracing::xxx!` event whose level passes
//! the active filter re-enters `RedactWriter::write` and recurses
//! infinitely (stack overflow). This module therefore emits **no
//! tracing events at all**. Debugging happens via `RUST_BACKTRACE`
//! and a debugger, not via the tracing macros.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::error::Result;

use super::patterns::{PATTERNS, Pattern, PatternKind, kind_for_pattern_id, substitute};

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
    pub fn active_patterns(&self) -> Vec<&'static Pattern> {
        let all: Vec<&'static Pattern> = PATTERNS.iter().collect();
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
    let active = policy.active_patterns();
    let mut owned: Option<String> = None;
    for p in &active {
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

/// Result of the categorised redaction pass. Carries the redacted
/// text plus the per-`PatternKind` match counts so the caller can
/// persist a `redact_audit` row (D.8.5 / D.5.1) without
/// re-running the regexes.
///
/// `kinds` is a vector of `(kind, count)` pairs in the order the
/// matches were observed. Each entry corresponds to at least one
/// substitution in `text`. `Unknown` is the only kind that does
/// NOT map to a built-in pattern id — it is emitted when the
/// legacy `[REDACTED:<id>]` marker survives a downstream rewrite
/// but the id is not in the categorised catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactResult {
    /// The redacted text. Equivalent to what `apply()` would have
    /// produced but with the categorised substitute marker.
    pub text: String,
    /// Per-kind match counts in the order the substitutions
    /// happened.
    pub kinds: Vec<(PatternKind, usize)>,
}

/// Apply the categorised redaction pass (D.8.2). Walks the active
/// pattern list and, for each match, replaces the original with
/// `substitute(kind_for_pattern_id(p.id))` (or
/// `substitute(PatternKind::Unknown)` when no mapping exists).
///
/// The pass returns both the rewritten text and the per-kind
/// match counts so the caller can write a single `redact_audit`
/// row with the breakdown. `Unknown` entries are skipped from the
/// audit count (they represent patterns that were not part of
/// the categorised catalog — they still redact, but the audit
/// row stays clean).
pub fn apply_with_categories(
    policy: &RedactPolicy,
    surface: Surface,
    text: &str,
) -> Result<RedactResult> {
    if !policy.is_enabled(surface) || text.is_empty() {
        return Ok(RedactResult {
            text: text.to_string(),
            kinds: Vec::new(),
        });
    }
    let mut owned: String = text.to_owned();
    let mut kinds: Vec<(PatternKind, usize)> = Vec::new();
    for p in policy.active_patterns() {
        let kind = match kind_for_pattern_id(p.id) {
            Some(k) => k,
            None => continue,
        };
        let replacement = substitute(kind);
        // `find_iter` lets us count matches without doing the
        // replace twice. The replace uses the same `re` so the
        // two are guaranteed to agree.
        let count = p.re.find_iter(&owned).count();
        if count == 0 {
            continue;
        }
        let replaced = p.re.replace_all(&owned, replacement);
        if let Cow::Owned(s) = replaced {
            owned = s;
        }
        kinds.push((kind, count));
    }
    Ok(RedactResult { text: owned, kinds })
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

    // -----------------------------------------------------------------
    // Categorised redaction (D.8.2)
    // -----------------------------------------------------------------

    use super::PatternKind;

    /// `sk-cp-...` keys collapse to the categorised substitute.
    #[test]
    fn apply_with_categories_redacts_sk_cp_api_key() {
        let p = RedactPolicy::default();
        let text = "API key is sk-cp-abc123def456ghi789jkl012mno345pqr678stu901vwx234";
        let r = apply_with_categories(&p, Surface::Telemetry, text).unwrap();
        assert!(r.text.contains("***REDACTED:api_key:sk-cp***"));
        assert!(!r.text.contains("sk-cp-abc123"));
        assert!(r.kinds.iter().any(|(k, _)| *k == PatternKind::SkCpApiKey));
    }

    /// Email addresses collapse to the categorised substitute.
    #[test]
    fn apply_with_categories_redacts_email() {
        let p = RedactPolicy::default();
        let text = "Contact: alice@example.com about issue #42";
        let r = apply_with_categories(&p, Surface::Export, text).unwrap();
        assert!(r.text.contains("***REDACTED:email***"));
        assert!(!r.text.contains("alice@example.com"));
        assert!(r.kinds.iter().any(|(k, _)| *k == PatternKind::Email));
    }

    /// JWTs collapse to the categorised substitute. The bearer
    /// pattern would otherwise catch the whole `Bearer <jwt>`
    /// pair first, so this test uses a JWT that does not appear
    /// in a header to exercise the JWT regex in isolation.
    #[test]
    fn apply_with_categories_redacts_jwt() {
        let p = RedactPolicy::default();
        let text =
            "token=eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNry";
        let r = apply_with_categories(&p, Surface::Telemetry, text).unwrap();
        assert!(r.text.contains("***REDACTED:jwt***"));
        assert!(!r.text.contains("eyJhbGciOiJIUzI1Ni"));
        assert!(r.kinds.iter().any(|(k, _)| *k == PatternKind::Jwt));
    }

    /// GitHub PATs collapse to the categorised substitute.
    #[test]
    fn apply_with_categories_redacts_github_pat() {
        let p = RedactPolicy::default();
        let text = "token=ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let r = apply_with_categories(&p, Surface::Telemetry, text).unwrap();
        assert!(r.text.contains("***REDACTED:github_pat***"));
        assert!(!r.text.contains("ghp_abcdef"));
        assert!(r.kinds.iter().any(|(k, _)| *k == PatternKind::GithubPat));
    }

    /// The bearer header collapses to `Bearer ***REDACTED***`
    /// (not the value-style marker).
    #[test]
    fn apply_with_categories_redacts_bearer() {
        let p = RedactPolicy::default();
        let text = "Authorization: bearer abcdef0123456789abcdef0123";
        let r = apply_with_categories(&p, Surface::Telemetry, text).unwrap();
        assert!(r.text.contains("Bearer ***REDACTED***"));
        assert!(r.kinds.iter().any(|(k, _)| *k == PatternKind::BearerHeader));
    }

    /// The `kinds` vector is non-empty when at least one match
    /// happened and the counts are >= 1. The same text scanned
    /// twice must report the same per-kind counts (idempotent).
    #[test]
    fn apply_with_categories_returns_kind_counts() {
        let p = RedactPolicy::default();
        let text = "k1=sk-cp-aaaaaaaaaaaaaaaaaaaa k2=sk-cp-bbbbbbbbbbbbbbbbbbbb";
        let r = apply_with_categories(&p, Surface::Telemetry, text).unwrap();
        assert!(!r.kinds.is_empty());
        let sk_cp_count: usize = r
            .kinds
            .iter()
            .filter(|(k, _)| *k == PatternKind::SkCpApiKey)
            .map(|(_, c)| *c)
            .sum();
        assert_eq!(sk_cp_count, 2);
    }

    /// Patterns that are not in the categorised catalog (e.g.
    /// `aws_secret_key`) are silently skipped — the categorised
    /// apply pass does not redact them at all and the audit
    /// `kinds` vector stays clean. This is the spec's "skip
    /// silently" contract (D.8.2): only the 14 categorised
    /// kinds are recognised; everything else is left to the
    /// legacy `apply()` pass.
    #[test]
    fn apply_with_categories_skips_unknown_patterns() {
        let p = RedactPolicy::default().with_patterns(["aws_secret_key"]);
        let text = "AWS_SECRET_ACCESS_KEY=abcdef0123456789abcdef0123456789abcdef01";
        let r = apply_with_categories(&p, Surface::Telemetry, text).unwrap();
        // The text was NOT redacted (the categorised pass
        // skipped the aws_secret_key pattern).
        assert!(r.text.contains("AWS_SECRET_ACCESS_KEY"));
        // And the audit kinds list is empty because no
        // categorised pattern matched.
        assert!(r.kinds.is_empty());
    }
}
