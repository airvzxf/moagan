//! Discovery tagger — classify a sketch into a primary category.
//!
//! This module is the pure helper. The phase that wires it into the
//! pipeline lives in `src/phases/discover_tag.rs`.

use crate::domain::SketchTags;

/// Categorise a sketch. The actual LLM call lives in the phase;
/// here we only own the schema and the validation rules.
///
/// `primary` is the canonical category. If the similarity score is
/// below [`UNCATEGORIZED_THRESHOLD`] the snippet is forced into
/// `"uncategorized"` regardless of `primary`.
pub const UNCATEGORIZED_THRESHOLD: f32 = 0.6;

/// Hard enum of the difficulty values the tagger contract accepts.
pub const DIFFICULTY_VALUES: &[&str] = &["low", "medium", "high"];

/// Sanitise a tagger response. Mutates the input in place:
/// - If `similarity_to_category < UNCATEGORIZED_THRESHOLD`, set
///   `primary` to `"uncategorized"`.
/// - If `difficulty` is not one of `DIFFICULTY_VALUES`, default to
///   `"medium"`.
pub fn sanitise(tags: &mut SketchTags) {
    if tags.similarity_to_category < UNCATEGORIZED_THRESHOLD {
        tags.primary = "uncategorized".into();
    }
    if !DIFFICULTY_VALUES.contains(&tags.difficulty.as_str()) {
        tags.difficulty = "medium".into();
    }
}

/// Count the number of `uncategorized` tags in a slice. Useful to
/// decide whether to emit a warning (V4 §6.5: "if the mode of
/// uncategorized exceeds `uncategorized_threshold` (default 0.3),
/// emit a warning").
pub fn uncategorized_ratio(tags: &[SketchTags]) -> f32 {
    if tags.is_empty() {
        return 0.0;
    }
    let n = tags.iter().filter(|t| t.primary == "uncategorized").count();
    n as f32 / tags.len() as f32
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

    #[test]
    fn sanitise_demotes_low_similarity_to_uncategorized() {
        let mut t = mk_tags("auth", 0.4, "low");
        sanitise(&mut t);
        assert_eq!(t.primary, "uncategorized");
    }

    #[test]
    fn sanitise_keeps_high_similarity() {
        let mut t = mk_tags("auth", 0.92, "high");
        sanitise(&mut t);
        assert_eq!(t.primary, "auth");
    }

    #[test]
    fn sanitise_fixes_unknown_difficulty() {
        let mut t = mk_tags("auth", 0.9, "impossible");
        sanitise(&mut t);
        assert_eq!(t.difficulty, "medium");
    }

    #[test]
    fn sanitise_keeps_known_difficulty() {
        let mut t = mk_tags("auth", 0.9, "low");
        sanitise(&mut t);
        assert_eq!(t.difficulty, "low");
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
    fn uncategorized_threshold_is_the_documented_default() {
        assert!((UNCATEGORIZED_THRESHOLD - 0.6).abs() < 1e-6);
    }
}
