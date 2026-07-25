//! Route phase. Reads the brief, asks the model for fast/standard.

use crate::domain::Route;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Route phase.
pub struct RoutePhase;

impl Phase for RoutePhase {
    fn name(&self) -> &'static str {
        "route"
    }

    fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let brief: serde_json::Value = read_json(&ctx.run_dir().brief())?;
        let user = serde_json::to_string(&brief).map_err(crate::Error::from)?;
        let system = system_prompt(Role::Route).to_owned();
        let resp = pollster::block_on(ctx.call(Role::Route, system, user))?;
        let route: Route = pollster::block_on(ctx.parse_model_json(
            Role::Route,
            &resp.text,
            "Route: {mode, reason, sketches, proposals, judges}",
        ))?;
        let path = ctx.run_dir().final_dir().join("route.json");
        write_json(&path, &route)?;
        Ok(PhaseOutput::Route(path))
    }
}
