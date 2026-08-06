//! D.22.3: `RefineAction` dispatcher. When the judge phase surfaces
//! an [`AdversaryPattern`] verdict it picks one of the seven
//! [`RefineAction`] variants and routes the proposal through the
//! matching refinement step. This module owns that routing.
//!
//! Spec contract:
//!
//! - [`dispatch_refine_action`] is a pure function. It does no I/O,
//!   no LLM calls, and no DB writes. The caller receives a
//!   [`RefineDispatchPlan`] describing the action's effects (a
//!   mutated proposal, an augmented [`SynthesisRequest`], an
//!   augmented system prompt, and an optional
//!   [`TelemetryEvent::StaleArtifact`]) and is responsible for
//!   actually issuing the LLM re-call, writing the proposal sidecar,
//!   and emitting the telemetry event.
//! - `TightenConstraint` augments the [`SynthesisRequest`] with the
//!   verdict detail as a new `prohibited_decisions` entry so the
//!   re-LLM call avoids the same drift.
//! - `AddEvidence` augments the system prompt with a "Sources from
//!   past runs" block drawn from [`EpistemicLegacy`] so the re-LLM
//!   call is grounded in operator-curated knowledge.
//! - `RerunCritique` marks the dispatch plan with a `target_role`
//!   of `Role::Critique` so the orchestrator can re-issue the
//!   critique call with a fresh prompt seed.
//! - `DropProposal` stamps the proposal's `replaced_by` field with
//!   `Some("dropped")` so the rank phase filters it out.
//! - `SplitProposal` and `MergeProposal` are log+no-op placeholders
//!   documented as follow-up; the dispatch plan carries the action
//!   tag so a post-mortem can see which verdicts were deferred.
//! - `RequestHumanInput` synthesizes a [`TelemetryEvent::StaleArtifact`]
//!   whose `path` names the proposal id and whose `age_secs` is 0;
//!   the operator picks it up from the telemetry stream.
//!
//! Every variant round-trips through a unit test that pins the
//! post-condition on the returned plan; the orchestrator can rely
//! on those tests as a contract.

use crate::discovery::epistemic_legacy::EpistemicLegacy;
use crate::domain::Proposal;
use crate::domain::synthesis_request::SynthesisRequest;
use crate::llm::Role;
use crate::ranking::RefineAction;
use crate::telemetry::event::TelemetryEvent;

/// Sentinel `replaced_by` value written by `DropProposal`. The
/// rank phase filters out any proposal whose `replaced_by` equals
/// this sentinel so a "drop" is distinguishable from a "merged
/// into synthesis" stamp (which carries a real synthesis id).
pub const DROPPED_SENTINEL: &str = "dropped";

/// Marker recorded by the dispatcher for `SplitProposal`. The
/// post-mortem greps for this marker in the dispatch plan to know
/// which verdicts were deferred to follow-up.
pub const NOOP_REASON_SPLIT: &str = "split_proposal: deferred (follow-up)";

/// Marker recorded by the dispatcher for `MergeProposal`. The
/// post-mortem greps for this marker in the dispatch plan to know
/// which verdicts were deferred to follow-up.
pub const NOOP_REASON_MERGE: &str = "merge_proposal: deferred (follow-up)";

/// Context the dispatcher needs to apply a refinement step. The
/// caller assembles one from the proposal sidecar, the active
/// `SynthesisRequest`, and the loaded `EpistemicLegacy`; the
/// dispatcher never reaches out to disk or the LLM registry on
/// its own.
#[derive(Debug, Clone)]
pub struct RefineContext {
    /// Proposal the verdict fired on. The dispatcher returns a
    /// (possibly mutated) copy in the dispatch plan; the caller
    /// decides whether to persist it.
    pub proposal: Proposal,
    /// Active synthesis request. `TightenConstraint` appends the
    /// verdict detail as a new prohibited decision.
    pub synthesis_request: SynthesisRequest,
    /// System prompt the next LLM call would have used. `AddEvidence`
    /// appends a sources block drawn from the legacy.
    pub system_prompt: String,
    /// Loaded epistemic legacy. `AddEvidence` consults this to
    /// render the "Sources from past runs" block.
    pub legacy: EpistemicLegacy,
    /// Verdict detail from the [`AdversaryPattern`] verdict that
    /// triggered the action. Used by `TightenConstraint` (recorded
    /// as a prohibited decision) and `AddEvidence` (recorded as a
    /// source-context note).
    pub verdict_detail: String,
}

impl RefineContext {
    /// Build a context for tests / direct callers. Defaults every
    /// field to a sensible empty value so a unit test only has to
    /// override the parts it cares about.
    pub fn new(proposal: Proposal) -> Self {
        Self {
            proposal,
            synthesis_request: SynthesisRequest::new(),
            system_prompt: String::new(),
            legacy: EpistemicLegacy::empty(),
            verdict_detail: String::new(),
        }
    }
}

/// Pure-data result of [`dispatch_refine_action`]. The orchestrator
/// inspects each field to decide what to do next: persist the
/// proposal, re-issue the LLM call, emit telemetry. No I/O happens
/// inside the dispatcher — the caller owns that step.
#[derive(Debug, Clone)]
pub struct RefineDispatchPlan {
    /// The action the dispatcher applied (echoed back so the
    /// caller does not have to thread it through).
    pub action: RefineAction,
    /// The (possibly mutated) proposal after the action was
    /// applied. `DropProposal` flips `replaced_by` to
    /// `Some("dropped")`; every other action leaves the proposal
    /// unchanged.
    pub proposal: Proposal,
    /// The (possibly augmented) synthesis request. `TightenConstraint`
    /// appends a new prohibited decision; every other action
    /// leaves the request unchanged.
    pub synthesis_request: SynthesisRequest,
    /// The (possibly augmented) system prompt. `AddEvidence`
    /// appends a sources block; every other action leaves the
    /// prompt unchanged.
    pub system_prompt: String,
    /// Optional telemetry event the caller must emit.
    /// `RequestHumanInput` populates this with a
    /// [`TelemetryEvent::StaleArtifact`]; every other action leaves
    /// it `None`.
    pub telemetry_event: Option<TelemetryEvent>,
    /// For `RerunCritique`, the role the caller should re-invoke.
    /// `Some(Role::Critique)` only for `RerunCritique`; `None` for
    /// every other action.
    pub target_role: Option<Role>,
}

impl RefineDispatchPlan {
    /// Convenience helper: emit the plan's telemetry event through
    /// `tracing` if one is present. The caller may prefer to route
    /// the event through `Telemetry::dispatch` instead; this helper
    /// is the fallback when the orchestrator does not have a
    /// telemetry handle in scope.
    pub fn emit_telemetry(&self) {
        if let Some(event) = &self.telemetry_event {
            event.emit();
        }
    }
}

/// Apply the requested refinement action to the supplied
/// [`RefineContext`] and return a [`RefineDispatchPlan`] describing
/// the effects. Pure function: no I/O, no LLM, no DB.
pub fn dispatch_refine_action(action: RefineAction, mut ctx: RefineContext) -> RefineDispatchPlan {
    match action {
        RefineAction::TightenConstraint => {
            let mut next_request = ctx.synthesis_request.clone();
            if !ctx.verdict_detail.is_empty() {
                next_request = next_request.forbid(&ctx.verdict_detail);
            }
            RefineDispatchPlan {
                action,
                proposal: ctx.proposal,
                synthesis_request: next_request,
                system_prompt: ctx.system_prompt.clone(),
                telemetry_event: None,
                target_role: None,
            }
        }
        RefineAction::AddEvidence => {
            let mut augmented = ctx.system_prompt.clone();
            if !ctx.legacy.preferred_strategies.is_empty() {
                augmented.push_str("\n\nSources from past runs:\n");
                for (idx, strategy) in ctx.legacy.preferred_strategies.iter().enumerate() {
                    augmented.push_str(&format!("- [{}] {}\n", idx + 1, strategy));
                }
            }
            RefineDispatchPlan {
                action,
                proposal: ctx.proposal,
                synthesis_request: ctx.synthesis_request.clone(),
                system_prompt: augmented,
                telemetry_event: None,
                target_role: None,
            }
        }
        RefineAction::RerunCritique => RefineDispatchPlan {
            action,
            proposal: ctx.proposal,
            synthesis_request: ctx.synthesis_request.clone(),
            system_prompt: ctx.system_prompt.clone(),
            telemetry_event: None,
            target_role: Some(Role::Critique),
        },
        RefineAction::DropProposal => {
            ctx.proposal.replaced_by = Some(DROPPED_SENTINEL.to_owned());
            RefineDispatchPlan {
                action,
                proposal: ctx.proposal,
                synthesis_request: ctx.synthesis_request.clone(),
                system_prompt: ctx.system_prompt.clone(),
                telemetry_event: None,
                target_role: None,
            }
        }
        RefineAction::SplitProposal | RefineAction::MergeProposal => {
            let reason = match action {
                RefineAction::SplitProposal => NOOP_REASON_SPLIT,
                RefineAction::MergeProposal => NOOP_REASON_MERGE,
                _ => unreachable!("SplitProposal/MergeProposal arm is exhaustive"),
            };
            tracing::info!(
                event = "refine_action.noop",
                action = action.as_str(),
                reason = reason,
                proposal_id = %ctx.proposal.id,
                "RefineAction deferred (no-op)"
            );
            RefineDispatchPlan {
                action,
                proposal: ctx.proposal,
                synthesis_request: ctx.synthesis_request.clone(),
                system_prompt: ctx.system_prompt.clone(),
                telemetry_event: None,
                target_role: None,
            }
        }
        RefineAction::RequestHumanInput => {
            let event = TelemetryEvent::StaleArtifact {
                path: format!("proposals/{}.json", ctx.proposal.id),
                age_secs: 0,
                at_unix: crate::time::now_unix_secs(),
            };
            RefineDispatchPlan {
                action,
                proposal: ctx.proposal,
                synthesis_request: ctx.synthesis_request.clone(),
                system_prompt: ctx.system_prompt.clone(),
                telemetry_event: Some(event),
                target_role: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_proposal(id: &str) -> Proposal {
        Proposal {
            id: id.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn refine_action_dispatch_tighten_calls_re_llm() {
        let mut ctx = RefineContext::new(sample_proposal("p_001"));
        ctx.verdict_detail = "vague about auth model".to_owned();
        let plan = dispatch_refine_action(RefineAction::TightenConstraint, ctx);

        assert_eq!(plan.action, RefineAction::TightenConstraint);
        assert_eq!(
            plan.synthesis_request.prohibited_decisions,
            vec!["vague about auth model".to_owned()]
        );
        assert!(plan.proposal.replaced_by.is_none());
        assert!(plan.telemetry_event.is_none());
    }

    #[test]
    fn refine_action_dispatch_drop_marks_replaced_by() {
        let ctx = RefineContext::new(sample_proposal("p_002"));
        let plan = dispatch_refine_action(RefineAction::DropProposal, ctx);
        assert_eq!(plan.action, RefineAction::DropProposal);
        assert_eq!(plan.proposal.replaced_by.as_deref(), Some(DROPPED_SENTINEL));
        assert!(plan.telemetry_event.is_none());
    }

    #[test]
    fn refine_action_dispatch_human_input_emits_stale_event() {
        let ctx = RefineContext::new(sample_proposal("p_003"));
        let plan = dispatch_refine_action(RefineAction::RequestHumanInput, ctx);
        assert_eq!(plan.action, RefineAction::RequestHumanInput);

        let event = plan
            .telemetry_event
            .as_ref()
            .expect("RequestHumanInput must emit a StaleArtifact event");
        match event {
            TelemetryEvent::StaleArtifact { path, age_secs, .. } => {
                assert_eq!(path, "proposals/p_003.json");
                assert_eq!(*age_secs, 0);
            }
            other => panic!("expected StaleArtifact event, got {:?}", other),
        }

        plan.emit_telemetry();
    }

    #[test]
    fn refine_action_dispatch_add_evidence_augments_system_prompt() {
        let mut ctx = RefineContext::new(sample_proposal("p_004"));
        ctx.system_prompt = "base".to_owned();
        let mut legacy = EpistemicLegacy::empty();
        legacy.preferred_strategies = vec!["use prepared statements".to_owned()];
        ctx.legacy = legacy;

        let plan = dispatch_refine_action(RefineAction::AddEvidence, ctx);
        assert!(plan.system_prompt.starts_with("base"));
        assert!(plan.system_prompt.contains("Sources from past runs"));
        assert!(plan.system_prompt.contains("use prepared statements"));
    }

    #[test]
    fn refine_action_dispatch_rerun_critique_marks_target_role() {
        let ctx = RefineContext::new(sample_proposal("p_005"));
        let plan = dispatch_refine_action(RefineAction::RerunCritique, ctx);
        assert_eq!(plan.target_role, Some(Role::Critique));
        assert!(plan.telemetry_event.is_none());
        assert!(plan.proposal.replaced_by.is_none());
    }

    #[test]
    fn refine_action_dispatch_split_and_merge_are_noop() {
        let ctx = RefineContext::new(sample_proposal("p_006"));
        let plan_split = dispatch_refine_action(RefineAction::SplitProposal, ctx.clone());
        assert_eq!(plan_split.action, RefineAction::SplitProposal);
        assert!(plan_split.proposal.replaced_by.is_none());
        assert!(plan_split.telemetry_event.is_none());

        let plan_merge = dispatch_refine_action(RefineAction::MergeProposal, ctx);
        assert_eq!(plan_merge.action, RefineAction::MergeProposal);
        assert!(plan_merge.proposal.replaced_by.is_none());
        assert!(plan_merge.telemetry_event.is_none());
    }
}
