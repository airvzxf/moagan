//! Hard tag incompatibilities (proposal-03 §D.13.15).
//!
//! Some tags describe architectural choices that are mutually exclusive:
//! a cluster of proposals that mixes them is incoherent, so the
//! `SynthesizePhase` must skip it instead of asking the LLM to merge
//! contradictory decisions. The list is a constant so a tool (or a
//! human reviewing the JSON sidecar) can audit every pair we treat as
//! mutually exclusive.
//!
//! Compliance: proposal-03 §D.13.15 (10 pairs from T02-09; T19-09;
//! T03-01; T18-04 §11.1; T05-10 §11.1; T08-06 §11.2; T08-08 §11.2).
//!
//! This module is deliberately small: a `const` list and a pure
//! helper. The synthesis phase is the only consumer; we expose both
//! the constant and the helper so the integration test can pin the
//! pairs directly without depending on the `SynthesizePhase`.

/// Pairs of tag values that are mutually exclusive. Order within each
/// pair is irrelevant — `is_incompatible("a", "b")` and
/// `is_incompatible("b", "a")` both return `true`.
pub const HARD_INCOMPATIBILITIES: &[(&str, &str)] = &[
    ("monolith", "microservices"),
    ("sync_rpc", "event_driven"),
    ("strong_consistency", "eventual_consistency"),
    ("sql", "nosql"),
    ("self_hosted", "serverless"),
    ("rust", "non_permitted_runtime"),
    ("single_tenant", "multi_tenant"),
    ("monolith_db", "polyglot_persistence"),
    ("pull_based", "push_based"),
    ("custom_protocol", "standard_protocol"),
];

/// Symmetric incompatibility check. Returns `true` when `(a, b)`
/// (in either order) appears in `HARD_INCOMPATIBILITIES`.
pub fn is_incompatible(a: &str, b: &str) -> bool {
    HARD_INCOMPATIBILITIES
        .iter()
        .any(|(x, y)| (a == *x && b == *y) || (a == *y && b == *x))
}

/// Iterate every unique pair (a, b) where a and b are members of
/// `tags`, returning the pairs whose components are mutually
/// exclusive. The first component is always the one that appears
/// earlier in `HARD_INCOMPATIBILITIES`. Returns the pair as a
/// 2-tuple of borrowed strings.
pub fn find_conflicts<'a>(tags: &[&'a str]) -> Vec<(&'a str, &'a str)> {
    let mut out: Vec<(&'a str, &'a str)> = Vec::new();
    for i in 0..tags.len() {
        for j in (i + 1)..tags.len() {
            if is_incompatible(tags[i], tags[j]) {
                out.push((tags[i], tags[j]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tag compared with itself must never be flagged — the matrix
    /// is "different architectural choices", not "self-conflict".
    #[test]
    fn identical_tags_are_not_incompatible() {
        assert!(!is_incompatible("monolith", "monolith"));
        assert!(!is_incompatible("sql", "sql"));
    }

    /// Both orderings of a known incompatible pair must report true
    /// (the matrix is symmetric).
    #[test]
    fn known_pair_is_incompatible_both_orderings() {
        assert!(is_incompatible("monolith", "microservices"));
        assert!(is_incompatible("microservices", "monolith"));
        assert!(is_incompatible("sql", "nosql"));
        assert!(is_incompatible("nosql", "sql"));
    }

    /// A pair that does not appear in the matrix must report false.
    /// Picking tags that are unrelated on purpose (sql + pull_based
    /// is fine; sql + self_hosted is also fine; only sql + nosql is
    /// a hard incompatibility).
    #[test]
    fn unknown_pair_is_not_incompatible() {
        assert!(!is_incompatible("sql", "self_hosted"));
        assert!(!is_incompatible("rust", "event_driven"));
        assert!(!is_incompatible("foo", "bar"));
    }

    /// Empty input cannot contain any pair. `is_incompatible("", "")`
    /// is `false` because the empty string is not in the matrix.
    #[test]
    fn empty_input_is_not_incompatible() {
        assert!(!is_incompatible("", ""));
        assert!(!is_incompatible("", "monolith"));
    }

    /// `find_conflicts` returns every pair exactly once (no
    /// duplicates) and skips unrelated tags.
    #[test]
    fn find_conflicts_returns_only_conflicting_pairs() {
        let tags = vec!["monolith", "microservices", "sql", "self_hosted"];
        let conflicts = find_conflicts(&tags);
        assert_eq!(conflicts.len(), 1);
        let (a, b) = conflicts[0];
        assert!(
            (a == "monolith" && b == "microservices")
                || (a == "microservices" && b == "monolith")
        );
    }
}
