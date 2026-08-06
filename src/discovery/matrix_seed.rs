//! D.13.19: helper for reproducible matrix sampling. The current
//! `MatrixCell` struct doesn't carry a `seed` field (to avoid
//! breaking 20+ literal constructors). This helper derives a
//! deterministic seed from (run_id, dimension_id, facet_id).

/// Derive a deterministic seed for one `MatrixCell`. The current
/// `MatrixCell` struct doesn't carry a `seed` field on purpose (the
/// catalog spec intentionally avoided touching the struct to keep
/// existing tests green). This helper computes the same value
/// outside the struct so the sampler can rebuild the seed from
/// `(run_id, dimension_id, facet_id)` whenever it needs one.
pub fn derive_cell_seed(run_id: &str, dimension_id: &str, facet_id: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    run_id.hash(&mut h);
    dimension_id.hash(&mut h);
    facet_id.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_cell_seed_is_deterministic() {
        let a = derive_cell_seed("run-1", "dim-a", "facet-x");
        let b = derive_cell_seed("run-1", "dim-a", "facet-x");
        assert_eq!(a, b);
    }

    #[test]
    fn derive_cell_seed_differs_for_different_inputs() {
        let base = derive_cell_seed("run-1", "dim-a", "facet-x");
        assert_ne!(base, derive_cell_seed("run-2", "dim-a", "facet-x"));
        assert_ne!(base, derive_cell_seed("run-1", "dim-b", "facet-x"));
        assert_ne!(base, derive_cell_seed("run-1", "dim-a", "facet-y"));
    }
}
