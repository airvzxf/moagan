//! Discovery facet derivation.
//!
//! Pure helpers; the phase lives in `src/phases/discover_facet.rs`.

use sha2::{Digest, Sha256};

use crate::domain::{Facet, FacetList};

/// Stable id for a facet name. The slug is kebab-cased and trimmed
/// to 64 chars.
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    let mut prev_dash = false;
    for ch in name.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if !prev_dash && !s.is_empty() {
                s.push('-');
            }
            prev_dash = true;
        } else {
            s.push(mapped);
            prev_dash = false;
        }
    }
    let trimmed = s.trim_matches('-').to_string();
    if trimmed.len() > 64 {
        trimmed[..64].to_string()
    } else {
        trimmed
    }
}

/// Compute the cache key for a `(brief, category_id)` pair. Used by
/// the facet phase to memoize the derived list across runs.
pub fn cache_key(brief: &str, category_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(brief.as_bytes());
    h.update([0x1f]);
    h.update(category_id.as_bytes());
    hex::encode(h.finalize())
}

impl FacetList {
    /// Build a `FacetList` from raw `(name, description, required)`
    /// triples. The phase calls this after the LLM returns the
    /// facet list as a JSON array.
    pub fn from_triples(
        category_id: &str,
        cluster_id: &str,
        brief: &str,
        now_unix: i64,
        triples: Vec<(String, String, bool)>,
    ) -> Self {
        let facets: Vec<Facet> = triples
            .into_iter()
            .map(|(name, description, required)| Facet {
                id: slug(&name),
                description,
                required,
            })
            .collect();
        Self {
            category_id: category_id.into(),
            cluster_id: cluster_id.into(),
            facets,
            cache_key: cache_key(brief, category_id),
            created_unix: now_unix,
            schema_version: "v1".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_lowercases_and_dashes() {
        assert_eq!(slug("Data Flows"), "data-flows");
        assert_eq!(slug("Auth  Strategy"), "auth-strategy");
    }

    #[test]
    fn slug_strips_leading_trailing_dashes() {
        assert_eq!(slug("  -Foo Bar-  "), "foo-bar");
    }

    #[test]
    fn slug_collapses_runs_of_dashes() {
        assert_eq!(slug("foo___bar"), "foo-bar");
    }

    #[test]
    fn slug_truncates_at_64_chars() {
        let s = "a".repeat(100);
        assert_eq!(slug(&s).len(), 64);
    }

    #[test]
    fn cache_key_deterministic() {
        let k1 = cache_key("brief", "cat_01");
        let k2 = cache_key("brief", "cat_01");
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64);
    }

    #[test]
    fn cache_key_differs_by_inputs() {
        assert_ne!(cache_key("brief", "cat_01"), cache_key("brief", "cat_02"));
        assert_ne!(cache_key("brief", "cat_01"), cache_key("other", "cat_01"));
    }

    #[test]
    fn facet_list_from_triples_builds_slugs() {
        let fl = FacetList::from_triples(
            "cat_01",
            "cluster_01",
            "brief",
            1_700_000_000,
            vec![
                ("Data Flows".into(), "Sequence of data".into(), true),
                ("Constraints".into(), "Hard constraints".into(), false),
            ],
        );
        assert_eq!(fl.facets.len(), 2);
        assert_eq!(fl.facets[0].id, "data-flows");
        assert!(fl.facets[0].required);
        assert_eq!(fl.cache_key.len(), 64);
    }
}
