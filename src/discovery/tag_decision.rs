//! D.13.10: tag_decision enum used by the tagger to explain
//! each tagging outcome.

/// Categorical outcome of a single tagging pass. Stored alongside
/// each sketch so downstream rankers can see *why* a sketch ended
/// up tagged (or not) without re-running the tagger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TagDecision {
    /// Sketch was confidently assigned one or more tags.
    Tagged,
    /// Tagger ran but every candidate tag fell below the threshold.
    Uncategorized,
    /// Tagger was skipped (e.g. sketch too short or marked low-value).
    Skipped,
    /// Tagger failed on the first attempt but a retry recovered.
    Recovered,
}

impl TagDecision {
    /// Stable snake-case string used in CSV/JSONL telemetry. Kept
    /// in sync with the serde representation so log scrapers do
    /// not have to translate between forms.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tagged => "tagged",
            Self::Uncategorized => "uncategorized",
            Self::Skipped => "skipped",
            Self::Recovered => "recovered",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_decision_as_str_round_trip() {
        assert_eq!(TagDecision::Tagged.as_str(), "tagged");
        assert_eq!(TagDecision::Uncategorized.as_str(), "uncategorized");
        assert_eq!(TagDecision::Skipped.as_str(), "skipped");
        assert_eq!(TagDecision::Recovered.as_str(), "recovered");
    }

    #[test]
    fn tag_decision_serializes_to_snake_case() {
        let json = serde_json::to_string(&TagDecision::Tagged).unwrap();
        assert_eq!(json, "\"tagged\"");
        let json = serde_json::to_string(&TagDecision::Uncategorized).unwrap();
        assert_eq!(json, "\"uncategorized\"");
        let json = serde_json::to_string(&TagDecision::Recovered).unwrap();
        assert_eq!(json, "\"recovered\"");
    }
}
