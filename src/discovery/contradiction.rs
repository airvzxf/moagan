//! Discovery contradiction detector.
//!
//! Pure helpers; the phase that wires the LLM calls lives in
//! `src/phases/discover_contradict.rs`.

use serde::{Deserialize, Serialize};

/// One contradiction record before it is serialised to
/// `crate::domain::Contradiction`. The transformation is `into()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionRecord {
    /// Cluster id on the "a" side.
    pub cluster_a: String,
    /// Cluster id on the "b" side.
    pub cluster_b: String,
    /// Sketch ids that triggered the contradiction.
    pub representatives: Vec<String>,
    /// Topic.
    pub topic: String,
    /// Description.
    pub description: String,
    /// Severity.
    pub severity: String,
}

/// Pick the cluster pair(s) with the highest disagreement. The
/// heuristic is simple: the centroid-distance ranking from the
/// clustering step is in `distances` (already sorted descending by
/// the cluster phase), and we return the top-`max_n` pairs.
pub fn top_pairs(distances: &[(String, String, f32)], max_n: usize) -> Vec<(String, String, f32)> {
    distances.iter().take(max_n).cloned().collect()
}

/// Severity ordering used to sort contradictions before persistence.
pub fn severity_rank(s: &str) -> u8 {
    match s {
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_pairs_caps_at_max_n() {
        let d = vec![
            ("c1".into(), "c2".into(), 0.9),
            ("c1".into(), "c3".into(), 0.7),
            ("c2".into(), "c3".into(), 0.5),
        ];
        let top = top_pairs(&d, 2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].0, "c1");
    }

    #[test]
    fn top_pairs_handles_empty() {
        assert!(top_pairs(&[], 5).is_empty());
    }

    #[test]
    fn severity_rank_orders_canonical_values() {
        assert!(severity_rank("high") > severity_rank("medium"));
        assert!(severity_rank("medium") > severity_rank("low"));
        assert_eq!(severity_rank("unknown"), 0);
    }

    #[test]
    fn contradiction_record_round_trips() {
        let r = ContradictionRecord {
            cluster_a: "cluster_01".into(),
            cluster_b: "cluster_05".into(),
            representatives: vec!["sk_001".into(), "sk_022".into()],
            topic: "consistency".into(),
            description: "ACID vs eventual".into(),
            severity: "high".into(),
        };
        let j = serde_json::to_string(&r).unwrap();
        let back: ContradictionRecord = serde_json::from_str(&j).unwrap();
        assert_eq!(back.severity, "high");
    }
}
