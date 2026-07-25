//! LLM role enum. Typed inverse of the T01-06 §4 role list.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::Error;

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
}

impl Role {
    /// Stable lowercase string for storage and telemetry.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Intake => "intake",
            Self::Clarify => "clarify",
            Self::Route => "route",
            Self::Propose => "propose",
            Self::Gate => "gate",
            Self::Critique => "critique",
            Self::Repair => "repair",
            Self::Judge => "judge",
            Self::Rank => "rank",
            Self::Deliver => "deliver",
        }
    }

    /// All roles in canonical order.
    pub fn all() -> &'static [Role] {
        &[
            Self::Intake,
            Self::Clarify,
            Self::Route,
            Self::Propose,
            Self::Gate,
            Self::Critique,
            Self::Repair,
            Self::Judge,
            Self::Rank,
            Self::Deliver,
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

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "intake" => Ok(Self::Intake),
            "clarify" => Ok(Self::Clarify),
            "route" => Ok(Self::Route),
            "propose" => Ok(Self::Propose),
            "gate" => Ok(Self::Gate),
            "critique" => Ok(Self::Critique),
            "repair" => Ok(Self::Repair),
            "judge" => Ok(Self::Judge),
            "rank" => Ok(Self::Rank),
            "deliver" => Ok(Self::Deliver),
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
    fn all_roles_are_count_ten() {
        assert_eq!(Role::all().len(), 10);
    }

    #[test]
    fn serde_round_trip() {
        let r = Role::Gate;
        let j = serde_json::to_string(&r).unwrap();
        assert_eq!(j, "\"gate\"");
        let back: Role = serde_json::from_str(&j).unwrap();
        assert_eq!(r, back);
    }
}
