//! D.22.2: seven-variant [`RefineAction`] enum used by the refine
//! loop. When an [`AdversaryPattern`] verdict fires on a proposal
//! the dispatcher picks one of these actions to apply; the
//! pipeline then runs the corresponding refinement step before
//! re-scoring.
//!
//! The enum is intentionally small and exhaustive: every variant
//! maps to a known refine-loop step. [`RefineAction::as_str`]
//! returns the snake_case wire form for telemetry and audit
//! logs (D.5.1). Adding a variant is a breaking change for any
//! downstream consumer that `match`-es on the wire form, so the
//! total variant count is pinned by the
//! `refine_action_variants_count_is_seven` test.

/// Refinement action the pipeline dispatches when an
/// [`AdversaryPattern`](super::adversary_patterns::AdversaryPattern)
/// verdict fires on a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum RefineAction {
    /// Tighten the constraint set in the proposal text. Used
    /// when the panel flagged the proposal as too vague.
    TightenConstraint,
    /// Add more evidence items to the proposal. Used when
    /// `InsufficientEvidence` fired.
    AddEvidence,
    /// Split a compound proposal into two smaller ones. Used
    /// when the panel disagreed about which half of a
    /// compound proposal was correct.
    SplitProposal,
    /// Merge the proposal with a sibling that covers the
    /// same ground. Used when two proposals collide on the
    /// Pareto front and the panel flagged the overlap.
    MergeProposal,
    /// Re-run the critique phase on the same proposal with
    /// a different prompt seed. Used when the panel's
    /// disagreement is judged to be prompt-induced noise.
    RerunCritique,
    /// Drop the proposal entirely. Used when the
    /// `HallucinationSignature` pattern fires and no salvage
    /// path applies.
    DropProposal,
    /// Surface the proposal to a human reviewer. Used as
    /// the catch-all when the refine loop cannot resolve
    /// the disagreement automatically.
    RequestHumanInput,
}

impl RefineAction {
    /// Stable snake_case wire form. The string values are part
    /// of the public audit format (D.5.1) and must not change
    /// without a coordinated migration.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TightenConstraint => "tighten_constraint",
            Self::AddEvidence => "add_evidence",
            Self::SplitProposal => "split_proposal",
            Self::MergeProposal => "merge_proposal",
            Self::RerunCritique => "rerun_critique",
            Self::DropProposal => "drop_proposal",
            Self::RequestHumanInput => "request_human_input",
        }
    }

    /// Canonical CLI form (kebab-case). The CLI accepts both the
    /// canonical kebab-case (`tighten-constraint`) and the audit
    /// snake-case (`tighten_constraint`) forms so an operator can
    /// paste either into a shell. The wire form for telemetry /
    /// logs remains `as_str()` snake-case (D.5.1).
    pub fn as_cli_str(&self) -> &'static str {
        match self {
            Self::TightenConstraint => "tighten-constraint",
            Self::AddEvidence => "add-evidence",
            Self::SplitProposal => "split-proposal",
            Self::MergeProposal => "merge-proposal",
            Self::RerunCritique => "rerun-critique",
            Self::DropProposal => "drop-proposal",
            Self::RequestHumanInput => "request-human-input",
        }
    }
}

impl std::str::FromStr for RefineAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        tracing::trace!(input = %s, "ranking::refine_action::RefineAction::from_str");
        let normalised = s.trim().to_ascii_lowercase().replace('-', "_");
        let action = match normalised.as_str() {
            "tighten_constraint" => Self::TightenConstraint,
            "add_evidence" => Self::AddEvidence,
            "split_proposal" => Self::SplitProposal,
            "merge_proposal" => Self::MergeProposal,
            "rerun_critique" => Self::RerunCritique,
            "drop_proposal" => Self::DropProposal,
            "request_human_input" => Self::RequestHumanInput,
            other => {
                tracing::warn!(
                    input = %s,
                    "ranking::refine_action::RefineAction::from_str: unknown action"
                );
                return Err(format!(
                    "unknown refine action '{other}' (expected one of: tighten-constraint, add-evidence, split-proposal, merge-proposal, rerun-critique, drop-proposal, request-human-input)"
                ));
            }
        };
        tracing::trace!(
            ?action,
            "ranking::refine_action::RefineAction::from_str: parsed"
        );
        Ok(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `as_str` returns the canonical snake_case wire form for
    /// every variant. Pins the audit-log contract (D.5.1): a
    /// refactor that drifts a string trips the test before the
    /// change lands in production.
    #[test]
    fn refine_action_as_str_returns_snake_case() {
        let cases = [
            (RefineAction::TightenConstraint, "tighten_constraint"),
            (RefineAction::AddEvidence, "add_evidence"),
            (RefineAction::SplitProposal, "split_proposal"),
            (RefineAction::MergeProposal, "merge_proposal"),
            (RefineAction::RerunCritique, "rerun_critique"),
            (RefineAction::DropProposal, "drop_proposal"),
            (RefineAction::RequestHumanInput, "request_human_input"),
        ];
        for (action, expected) in cases {
            assert_eq!(action.as_str(), expected);
        }
    }

    /// The enum has exactly seven variants. A future refactor
    /// that adds or removes a variant trips this test before
    /// the change can land, so downstream consumers can
    /// coordinate their `match` arms.
    #[test]
    fn refine_action_variants_count_is_seven() {
        let all = [
            RefineAction::TightenConstraint,
            RefineAction::AddEvidence,
            RefineAction::SplitProposal,
            RefineAction::MergeProposal,
            RefineAction::RerunCritique,
            RefineAction::DropProposal,
            RefineAction::RequestHumanInput,
        ];
        assert_eq!(all.len(), 7);
        // Every variant must round-trip through `as_str` and
        // produce a unique wire form (otherwise audit log
        // entries become ambiguous).
        let mut wire_forms: Vec<&'static str> = all.iter().map(|a| a.as_str()).collect();
        wire_forms.sort_unstable();
        wire_forms.dedup();
        assert_eq!(wire_forms.len(), 7, "wire forms must be unique");
    }

    /// `FromStr` accepts both the kebab-case CLI form
    /// (`tighten-constraint`) and the audit snake-case form
    /// (`tighten_constraint`). Case-insensitive; leading /
    /// trailing whitespace is trimmed. An unknown action
    /// returns an error listing every valid form.
    #[test]
    fn refine_action_from_str_accepts_both_forms() {
        use std::str::FromStr;

        let cases = [
            ("tighten-constraint", RefineAction::TightenConstraint),
            ("Tighten-Constraint", RefineAction::TightenConstraint),
            ("tighten_constraint", RefineAction::TightenConstraint),
            ("TIGHTEN_CONSTRAINT", RefineAction::TightenConstraint),
            ("add-evidence", RefineAction::AddEvidence),
            ("add_evidence", RefineAction::AddEvidence),
            ("split-proposal", RefineAction::SplitProposal),
            ("merge-proposal", RefineAction::MergeProposal),
            ("rerun-critique", RefineAction::RerunCritique),
            ("drop-proposal", RefineAction::DropProposal),
            ("request-human-input", RefineAction::RequestHumanInput),
            ("  drop_proposal  ", RefineAction::DropProposal),
        ];
        for (raw, expected) in cases {
            let parsed = RefineAction::from_str(raw)
                .unwrap_or_else(|e| panic!("failed to parse {raw:?}: {e}"));
            assert_eq!(parsed, expected, "wrong parse for {raw:?}");
        }

        // Unknown forms produce a helpful error.
        let err = RefineAction::from_str("nope").unwrap_err();
        assert!(err.contains("unknown refine action"), "{err}");
        assert!(err.contains("tighten-constraint"), "{err}");
    }

    /// `as_cli_str` returns kebab-case; `as_str` returns
    /// snake_case; the two are bijective.
    #[test]
    fn refine_action_cli_form_is_bijective_with_wire_form() {
        let all = [
            RefineAction::TightenConstraint,
            RefineAction::AddEvidence,
            RefineAction::SplitProposal,
            RefineAction::MergeProposal,
            RefineAction::RerunCritique,
            RefineAction::DropProposal,
            RefineAction::RequestHumanInput,
        ];
        for action in all {
            // The CLI form minus the dashes must equal the wire form.
            assert_eq!(
                action.as_cli_str().replace('-', "_"),
                action.as_str(),
                "as_cli_str / as_str must agree on identifier (variant {:?})",
                action
            );
        }
    }
}
