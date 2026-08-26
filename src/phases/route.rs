//! Route phase. Reads the brief, asks the model for fast/standard.

use async_trait::async_trait;

use crate::domain::Route;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Route phase.
pub struct RoutePhase;

#[async_trait]
impl Phase for RoutePhase {
    fn name(&self) -> &'static str {
        "route"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        tracing::debug!(role = "route", "route: enter");
        let brief: serde_json::Value = read_json(&ctx.run_dir().brief())?;
        tracing::trace!(brief_keys = ?brief.as_object().map(|o| o.len()), "route: brief loaded");
        let user = serde_json::to_string(&brief).map_err(crate::Error::from)?;
        let system = system_prompt(Role::Route).to_owned();
        let route: Route = ctx
            .call_with_retry_parse(
                Role::Route,
                system,
                user,
                "Route: {mode, reason, sketches, proposals, judges}",
                5,
            )
            .await?;
        tracing::info!(
            mode = %route.mode,
            reason = %route.reason,
            sketches = route.sketches,
            proposals = route.proposals,
            judges = route.judges,
            "route: model picked mode"
        );
        let path = ctx.run_dir().final_dir().join("route.json");
        write_json(&path, &route)?;
        tracing::debug!(path = %path.display(), "route: route.json written");
        Ok(PhaseOutput::Route(path))
    }
}
