//! Synthesis-replacement predicate (Phase F, V4 §5.13 + D.13.16).
//!
//! "Solo sustituye a sus fuentes si demuestra mejora sin perder
//!  coherencia." — V4 §5.13.
//!
//! The predicate is the one from `proposal-03 D.13.16` (catalog
//! additive, opt-in to T01-06), adapted to dimension-counting rather
//! than source-counting so that single-source clusters can still be
//! replaced when the synthesis is strictly better in enough criteria:
//!
//!   replace iff synthesis is the strict best across all sources in
//!             ≥2 of the 5 quality dimensions AND no source
//!             Pareto-dominates the synthesis.
//!
//! "Pareto-dominates" uses `crate::ranking::pareto::dominates` — every
//! dimension >= and strictly > in at least one. This keeps the
//! blocking semantics compatible with D.13.16 ("dominates_count == 0")
//! while making the threshold about criteria coverage (≥2) instead of
//! per-source coverage (≥2 sources). The threshold of 2 is the
//! "non-trivial improvement" floor called out in V4 §5.13.
//!
//! Pure: takes quality vectors, returns bool / indices. The caller
//! (`RankPhase`) handles I/O (sidecar metadata + ranking filter).

use crate::ranking::pareto::{QualityVector, dominates};

/// V4 §5.13 + D.13.16 predicate: should the synthesis replace its
/// sources in the final output?
///
/// Returns `true` iff:
/// - At least one source has a quality vector to compare against.
/// - The synthesis is the strict best (across all sources) in ≥2 of
///   the 5 quality dimensions.
/// - No source Pareto-dominates the synthesis.
pub fn should_replace_synthesis(synthesis_v: &QualityVector, source_vs: &[QualityVector]) -> bool {
    if source_vs.is_empty() {
        tracing::trace!("should_replace_synthesis: no sources, returns false");
        return false;
    }

    let s_dims = [
        synthesis_v.correctness,
        synthesis_v.completeness,
        synthesis_v.fit,
        synthesis_v.evidence,
        synthesis_v.clarity,
    ];

    let mut s_strict_best_dims: u32 = 0;
    for (i, &s_dim) in s_dims.iter().enumerate() {
        let best_src_in_dim = source_vs
            .iter()
            .map(|v| dim_at(v, i))
            .fold(f32::NEG_INFINITY, f32::max);
        if s_dim > best_src_in_dim {
            s_strict_best_dims += 1;
        }
    }

    let any_source_pareto_dominates = source_vs.iter().any(|sv| dominates(sv, synthesis_v));

    let result = s_strict_best_dims >= 2 && !any_source_pareto_dominates;
    tracing::debug!(
        strict_best_dims = s_strict_best_dims,
        any_source_pareto_dominates,
        replace = result,
        "should_replace_synthesis"
    );
    result
}

#[inline]
fn dim_at(v: &QualityVector, i: usize) -> f32 {
    match i {
        0 => v.correctness,
        1 => v.completeness,
        2 => v.fit,
        3 => v.evidence,
        4 => v.clarity,
        _ => f32::NEG_INFINITY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qv(corr: f32, comp: f32, fit: f32, evi: f32, cla: f32) -> QualityVector {
        QualityVector {
            correctness: corr,
            completeness: comp,
            fit,
            evidence: evi,
            clarity: cla,
        }
    }

    #[test]
    fn win_synthesis_dominates_two_dimensions_no_source_dominates() {
        let s = qv(9.0, 9.0, 7.0, 6.0, 5.0);
        let srcs = vec![qv(8.0, 8.0, 9.0, 8.0, 8.0), qv(7.0, 7.0, 8.0, 7.0, 7.0)];
        assert!(should_replace_synthesis(&s, &srcs));
    }

    #[test]
    fn lose_synthesis_dominates_zero_dimensions() {
        let s = qv(5.0, 5.0, 5.0, 5.0, 5.0);
        let srcs = vec![qv(8.0, 8.0, 9.0, 8.0, 8.0)];
        assert!(!should_replace_synthesis(&s, &srcs));
    }

    #[test]
    fn tie_synthesis_dominates_one_dimension() {
        let s = qv(9.0, 5.0, 5.0, 5.0, 5.0);
        let srcs = vec![qv(8.0, 8.0, 9.0, 8.0, 8.0)];
        assert!(!should_replace_synthesis(&s, &srcs));
    }

    #[test]
    fn single_source_dominant_synthesis_replaces() {
        let s = qv(9.0, 9.0, 9.0, 9.0, 9.0);
        let srcs = vec![qv(8.0, 8.0, 8.0, 8.0, 8.0)];
        assert!(should_replace_synthesis(&s, &srcs));
    }

    #[test]
    fn pareto_source_dominates_synthesis_blocks_replacement() {
        // src >= s in every dim AND strictly > in at least one →
        // Pareto-dominates s. Replacement must be blocked even though
        // s would otherwise look competitive.
        let s = qv(7.0, 7.0, 7.0, 7.0, 7.0);
        let srcs = vec![qv(8.0, 8.0, 8.0, 8.0, 8.0)];
        assert!(!should_replace_synthesis(&s, &srcs));
    }

    #[test]
    fn empty_sources_returns_false() {
        let s = qv(9.0, 9.0, 9.0, 9.0, 9.0);
        assert!(!should_replace_synthesis(&s, &[]));
    }

    #[test]
    fn dominated_source_does_not_block_replacement() {
        // A source that s Pareto-dominates should not block replacement.
        let s = qv(9.0, 9.0, 9.0, 9.0, 9.0);
        let srcs = vec![qv(5.0, 5.0, 5.0, 5.0, 5.0), qv(8.0, 8.0, 8.0, 8.0, 8.0)];
        assert!(should_replace_synthesis(&s, &srcs));
    }

    #[test]
    fn tied_dimension_does_not_count_as_strict_best() {
        let s = qv(9.0, 8.0, 5.0, 5.0, 5.0);
        let srcs = vec![qv(8.0, 8.0, 9.0, 8.0, 8.0)];
        assert!(!should_replace_synthesis(&s, &srcs));
    }
}
