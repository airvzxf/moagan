//! Discovery tagger — classify a sketch into a primary category.
//!
//! This module is the pure helper. The phase that wires it into the
//! pipeline lives in `src/phases/discover_tag.rs`.

use crate::discovery::tagger_threshold::TaggerThreshold;
use crate::domain::SketchTags;

/// Hard enum of the difficulty values the tagger contract accepts.
pub const DIFFICULTY_VALUES: &[&str] = &["low", "medium", "high"];

/// Sanitise a tagger response. Mutates the input in place:
/// - If `similarity_to_category < threshold.value`, set `primary` to
///   `"uncategorized"`.
/// - If `difficulty` is not one of `DIFFICULTY_VALUES`, default to
///   `"medium"`.
pub fn sanitise(tags: &mut SketchTags, threshold: &TaggerThreshold) {
    tracing::debug!(
        sketch_id = %tags.sketch_id,
        primary = %tags.primary,
        similarity = tags.similarity_to_category,
        threshold = threshold.value,
        difficulty = %tags.difficulty,
        "tagger: sanitise enter"
    );
    let mut demoted = false;
    let mut difficulty_reset = false;
    if tags.similarity_to_category < threshold.value {
        tracing::trace!(
            sketch_id = %tags.sketch_id,
            similarity = tags.similarity_to_category,
            threshold = threshold.value,
            "tagger: similarity below threshold; demoting primary"
        );
        tags.primary = "uncategorized".into();
        demoted = true;
    }
    if !DIFFICULTY_VALUES.contains(&tags.difficulty.as_str()) {
        tracing::trace!(
            sketch_id = %tags.sketch_id,
            difficulty = %tags.difficulty,
            "tagger: difficulty not in whitelist; resetting to medium"
        );
        tags.difficulty = "medium".into();
        difficulty_reset = true;
    }
    if demoted || difficulty_reset {
        tracing::debug!(
            sketch_id = %tags.sketch_id,
            demoted,
            difficulty_reset,
            "tagger: sanitise applied"
        );
    }
}

/// Count the number of `uncategorized` tags in a slice. Useful to
/// decide whether to emit a warning (V4 §6.5: "if the mode of
/// uncategorized exceeds `uncategorized_threshold` (default 0.3),
/// emit a warning").
pub fn uncategorized_ratio(tags: &[SketchTags]) -> f32 {
    if tags.is_empty() {
        tracing::trace!(count = 0, "tagger: uncategorized_ratio on empty slice");
        return 0.0;
    }
    let n = tags.iter().filter(|t| t.primary == "uncategorized").count();
    let ratio = n as f32 / tags.len() as f32;
    tracing::debug!(
        total = tags.len(),
        uncategorized = n,
        ratio = ratio,
        "tagger: uncategorized_ratio"
    );
    if ratio > 0.3 {
        tracing::warn!(
            ratio = ratio,
            threshold = 0.3,
            "tagger: uncategorized ratio exceeds documented threshold"
        );
    }
    ratio
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_tags(primary: &str, sim: f32, difficulty: &str) -> SketchTags {
        SketchTags {
            sketch_id: "sk_x".into(),
            primary: primary.into(),
            secondary: vec![],
            subcategory: String::new(),
            difficulty: difficulty.into(),
            similarity_to_category: sim,
            notes: String::new(),
            schema_version: "v1".into(),
        }
    }

    fn default_threshold() -> TaggerThreshold {
        TaggerThreshold::default()
    }

    #[test]
    fn sanitise_demotes_low_similarity_to_uncategorized() {
        let mut t = mk_tags("auth", 0.4, "low");
        sanitise(&mut t, &default_threshold());
        assert_eq!(t.primary, "uncategorized");
    }

    #[test]
    fn sanitise_keeps_high_similarity() {
        let mut t = mk_tags("auth", 0.92, "high");
        sanitise(&mut t, &default_threshold());
        assert_eq!(t.primary, "auth");
    }

    #[test]
    fn sanitise_fixes_unknown_difficulty() {
        let mut t = mk_tags("auth", 0.9, "impossible");
        sanitise(&mut t, &default_threshold());
        assert_eq!(t.difficulty, "medium");
    }

    #[test]
    fn sanitise_keeps_known_difficulty() {
        let mut t = mk_tags("auth", 0.9, "low");
        sanitise(&mut t, &default_threshold());
        assert_eq!(t.difficulty, "low");
    }

    #[test]
    fn sanitise_respects_configured_threshold() {
        let threshold = TaggerThreshold::from_config_value(Some(0.42));
        let mut kept = mk_tags("auth", 0.5, "low");
        sanitise(&mut kept, &threshold);
        assert_eq!(kept.primary, "auth", "0.5 >= 0.42 must keep the tag");
        let mut demoted = mk_tags("auth", 0.4, "low");
        sanitise(&mut demoted, &threshold);
        assert_eq!(
            demoted.primary, "uncategorized",
            "0.4 < 0.42 must demote the tag"
        );
    }

    #[test]
    fn uncategorized_ratio_returns_zero_for_empty() {
        assert_eq!(uncategorized_ratio(&[]), 0.0);
    }

    #[test]
    fn uncategorized_ratio_is_proportion() {
        let tags = vec![
            mk_tags("auth", 0.9, "low"),
            mk_tags("uncategorized", 0.3, "low"),
            mk_tags("storage", 0.9, "low"),
            mk_tags("uncategorized", 0.2, "low"),
        ];
        let r = uncategorized_ratio(&tags);
        assert!((r - 0.5).abs() < 1e-6);
    }

    #[test]
    fn tagger_threshold_default_matches_documented_default() {
        assert!((TaggerThreshold::default().value - 0.6).abs() < 1e-6);
    }
}
