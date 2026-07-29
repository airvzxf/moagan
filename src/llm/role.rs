//! LLM role enum. Typed inverse of the T01-06 §4 role list.
//!
//! Each role also exposes a short schema description and a validator
//! function so that callers can both teach the model what shape to
//! emit AND detect shape drift when the model's output doesn't match
//! the expected type. The schemas live next to the role enum so
//! adding a new role requires touching exactly one Rust file.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::domain::{
    Brief, Critique, FinalReport, Intake, JudgeScore, Proposal, Repair, Route, Sketch,
};
use crate::error::{Error, Result};

/// The set of LLM roles in the MVP pipeline.
///
/// Order is the canonical order they appear in a run; keep it stable
/// because telemetry rows index by role name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Intake — ingest the prompt + context.
    Intake,
    /// Clarify — produce the canonical brief.
    Clarify,
    /// Route — decide fast/standard depth.
    Route,
    /// Sketch — short exploration artefact (v0.2, T01-06 §5.5).
    Sketch,
    /// Propose — generate a proposal.
    Propose,
    /// Gate — structural validation of a proposal.
    Gate,
    /// Critique — domain critic on a proposal.
    Critique,
    /// Repair — apply a fix when the gate fails.
    Repair,
    /// Judge — independent evaluation of a proposal.
    Judge,
    /// Rank — produce the weighted ranking.
    Rank,
    /// Deliver — produce the final artefact.
    Deliver,
    /// Tagger — discovery mode (Plan B sub-phase B). Classifies a
    /// sketch into a primary category with subcategory + difficulty.
    /// Uses temperature 0.0 and top_p 0.2 for determinism.
    Tagger,
    /// Extractor — discovery mode. Pulls the per-facet markdown
    /// out of a cluster's sketches. Uses temperature 0.4 and top_p
    /// 0.8 for variation across facets.
    Extractor,
    /// Integrator — discovery mode. Joins the per-facet markdown
    /// into a coherent category document. Uses temperature 0.4 and
    /// top_p 0.9 for prose fluency.
    Integrator,
}

impl Role {
    /// Stable lowercase string for storage and telemetry.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Intake => "intake",
            Self::Clarify => "clarify",
            Self::Route => "route",
            Self::Sketch => "sketch",
            Self::Propose => "propose",
            Self::Gate => "gate",
            Self::Critique => "critique",
            Self::Repair => "repair",
            Self::Judge => "judge",
            Self::Rank => "rank",
            Self::Deliver => "deliver",
            Self::Tagger => "tagger",
            Self::Extractor => "extractor",
            Self::Integrator => "integrator",
        }
    }

    /// Short, machine- and human-readable schema description for
    /// this role. Used in system prompts and log lines so an operator
    /// can see at a glance what fields the model is supposed to emit.
    /// Long descriptions belong in `system_prompt(role)` instead.
    pub fn schema_description(&self) -> &'static str {
        match self {
            Self::Intake => {
                "Intake: {problem, objectives[], constraints[], non_goals[], open_questions[], raw_prompt}"
            }
            Self::Clarify => {
                "Brief: {problem, objectives[], deliverables[], constraints[], assumptions[], non_goals[], acceptance[], risks[]}"
            }
            Self::Route => "Route: {mode, reason, sketches, proposals, judges}",
            Self::Sketch => {
                "Sketch: {thesis, key_decisions[], architecture_outline, assumptions[], strengths[], weaknesses[], hard_constraint_check{}, expected_validation}"
            }
            Self::Propose => {
                "Proposal: {id, summary, approach, tradeoffs[], evidence[], artifacts[]{kind,language,source}?}"
            }
            Self::Gate => "Gate: {pass, issues[], missing[]}",
            Self::Critique => "Critique: {verdict, issues[], suggestions[]}",
            Self::Repair => "Repair: {id, summary, approach, tradeoffs[], evidence[], changes[]}",
            Self::Judge => {
                "JudgeScore: {score, criteria{correctness,completeness,fit,evidence,clarity}, comments}"
            }
            Self::Rank => "Ranking: {ranked[], winner}",
            Self::Deliver => {
                "FinalReport: {title, summary, recommendation, alternatives[], next_steps[]}"
            }
            Self::Tagger => {
                "SketchTags: {sketch_id, primary, secondary[], subcategory, difficulty, similarity_to_category, notes}"
            }
            Self::Extractor => "FacetExtraction: {facet_id, category_id, body, sources[]}",
            Self::Integrator => {
                "CategoryDoc: {category_id, cluster_id, body, sources[], density}"
            }
        }
    }

    /// Validate that `value` is the shape this role expects, by
    /// deserializing into the corresponding domain type. The current
    /// implementation reuses the existing typed deserialization so
    /// adding a new role means adding it once here.
    ///
    /// On failure, the error embeds the role name and the field that
    /// could not be parsed, which is much friendlier than the raw
    /// "expected `,` or `]` at line 1 column N" that serde produces.
    pub fn validate_json(&self, value: &serde_json::Value) -> Result<()> {
        let result: std::result::Result<(), serde_json::Error> = match self {
            Self::Intake => serde_json::from_value::<Intake>(value.clone()).map(|_| ()),
            Self::Clarify => serde_json::from_value::<Brief>(value.clone()).map(|_| ()),
            Self::Route => serde_json::from_value::<Route>(value.clone()).map(|_| ()),
            Self::Sketch => serde_json::from_value::<Sketch>(value.clone()).map(|_| ()),
            Self::Propose => serde_json::from_value::<Proposal>(value.clone()).map(|_| ()),
            Self::Gate => {
                // Gate does not call the LLM in v0.1 (the validator
                // exists for completeness / future use). Gate accepts
                // a boolean `pass` plus the issues/missing arrays
                // via the domain type — defined in domain.rs.
                serde_json::from_value::<crate::domain::Gate>(value.clone()).map(|_| ())
            }
            Self::Critique => serde_json::from_value::<Critique>(value.clone()).map(|_| ()),
            Self::Repair => serde_json::from_value::<Repair>(value.clone()).map(|_| ()),
            Self::Judge => serde_json::from_value::<JudgeScore>(value.clone()).map(|_| ()),
            Self::Rank => {
                serde_json::from_value::<crate::domain::Ranking>(value.clone()).map(|_| ())
            }
            Self::Deliver => serde_json::from_value::<FinalReport>(value.clone()).map(|_| ()),
            Self::Tagger => serde_json::from_value::<crate::domain::SketchTags>(value.clone()).map(|_| ()),
            Self::Extractor => serde_json::from_value::<crate::domain::FacetExtraction>(value.clone()).map(|_| ()),
            Self::Integrator => serde_json::from_value::<crate::domain::CategoryDoc>(value.clone()).map(|_| ()),
        };
        if let Err(e) = result {
            return Err(Error::SchemaViolation(format!(
                "role={} schema mismatch: {e}; expected {}",
                self.as_str(),
                self.schema_description()
            )));
        }
        Ok(())
    }

    /// All roles in canonical order.
    pub fn all() -> &'static [Role] {
        &[
            Self::Intake,
            Self::Clarify,
            Self::Route,
            Self::Sketch,
            Self::Propose,
            Self::Gate,
            Self::Critique,
            Self::Repair,
            Self::Judge,
            Self::Rank,
            Self::Deliver,
            Self::Tagger,
            Self::Extractor,
            Self::Integrator,
        ]
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "intake" => Ok(Self::Intake),
            "clarify" => Ok(Self::Clarify),
            "route" => Ok(Self::Route),
            "sketch" => Ok(Self::Sketch),
            "propose" => Ok(Self::Propose),
            "gate" => Ok(Self::Gate),
            "critique" => Ok(Self::Critique),
            "repair" => Ok(Self::Repair),
            "judge" => Ok(Self::Judge),
            "rank" => Ok(Self::Rank),
            "deliver" => Ok(Self::Deliver),
            "tagger" => Ok(Self::Tagger),
            "extractor" => Ok(Self::Extractor),
            "integrator" => Ok(Self::Integrator),
            other => Err(Error::InvalidArgs(format!("unknown role: {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for r in Role::all() {
            let s = r.as_str();
            let back: Role = s.parse().unwrap();
            assert_eq!(*r, back);
        }
    }

    #[test]
    fn unknown_role_errors() {
        let r = "not-a-role".parse::<Role>();
        assert!(r.is_err());
    }

    #[test]
    fn all_roles_are_count_fourteen() {
        // v0.2 added the `sketch` role between route and propose (10→11).
        // Sub-phase B added tagger, extractor, integrator (11→14).
        assert_eq!(Role::all().len(), 14);
    }

    #[test]
    fn serde_round_trip() {
        let r = Role::Gate;
        let j = serde_json::to_string(&r).unwrap();
        assert_eq!(j, "\"gate\"");
        let back: Role = serde_json::from_str(&j).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn schema_description_covers_every_role() {
        // Every role we ship must expose a non-empty schema. Roles
        // added without a description break the system prompt used
        // for new modes (`discovery`, `deep`, `explore`).
        for r in Role::all() {
            let desc = r.schema_description();
            assert!(!desc.is_empty(), "{:?} has no schema_description", r);
            assert!(
                desc.starts_with("Proposal:")
                    || desc.starts_with("Critique:")
                    || desc.starts_with("Brief:")
                    || desc.starts_with("Intake:")
                    || desc.starts_with("Route:")
                    || desc.starts_with("Sketch:")
                    || desc.starts_with("Gate:")
                    || desc.starts_with("Repair:")
                    || desc.starts_with("JudgeScore:")
                    || desc.starts_with("Ranking:")
                    || desc.starts_with("FinalReport:")
                    || desc.starts_with("SketchTags:")
                    || desc.starts_with("FacetExtraction:")
                    || desc.starts_with("CategoryDoc:"),
                "{:?} description does not start with its name: {desc}",
                r
            );
        }
    }

    #[test]
    fn schema_description_uses_role_name_as_prefix() {
        // Easier-to-read than the loop above; documents intent.
        assert!(Role::Intake.schema_description().starts_with("Intake:"));
        assert!(Role::Critique.schema_description().starts_with("Critique:"));
        assert!(Role::Propose.schema_description().starts_with("Proposal:"));
        assert!(Role::Judge.schema_description().starts_with("JudgeScore:"));
    }

    #[test]
    fn validate_json_accepts_a_well_formed_critique() {
        // A minimal-but-valid Critique. With `#[serde(default)]` on
        // the domain type, even empty fields are accepted; this test
        // exercises the happy path with realistic content.
        let raw = serde_json::json!({
            "verdict": "fix",
            "issues": ["It lacks a fallback"],
            "suggestions": ["Add a fallback path"]
        });
        assert!(Role::Critique.validate_json(&raw).is_ok());
    }

    #[test]
    fn validate_json_rejects_wrong_type() {
        // `verdict` is supposed to be a string; pass a number and
        // expect the validator to flag it. The error must mention
        // the role name so an operator scanning logs knows what
        // phase produced the broken JSON.
        let bad = serde_json::json!({
            "verdict": 42,
            "issues": [],
            "suggestions": []
        });
        let err = Role::Critique.validate_json(&bad).unwrap_err();
        assert!(err.to_string().contains("critique"), "got: {err}");
    }

    #[test]
    fn validate_json_returns_ok_for_default_filled_types() {
        // The domain types are `#[serde(default)]` since commit
        // ce309b2, so an empty object is acceptable. Pinning that
        // here so future schema additions don't silently break the
        // validator.
        let empty = serde_json::json!({});
        assert!(Role::Critique.validate_json(&empty).is_ok());
        assert!(Role::Intake.validate_json(&empty).is_ok());
        assert!(Role::Propose.validate_json(&empty).is_ok());
        assert!(Role::Judge.validate_json(&empty).is_ok());
        assert!(Role::Deliver.validate_json(&empty).is_ok());
        assert!(Role::Sketch.validate_json(&empty).is_ok());
    }
}
