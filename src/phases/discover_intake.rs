//! `intake` for `moagan discover`: reuse a valid brief, else run intake.
//!
//! A run dir whose `brief.json` already holds a brief keeps it, so a
//! resumed run never calls the intake model twice.

use std::path::Path;

use async_trait::async_trait;

use crate::domain::Intake;
use crate::error::Result;
use crate::phases::util::read_json;
use crate::phases::{IntakePhase, Phase, PhaseOutput, RunContext};

/// Discover's intake phase; see the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiscoverIntakePhase;

/// `true` when `path` is a file that parses as an [`Intake`] whose
/// trimmed `problem` is non-empty.
pub fn brief_is_valid(path: &Path) -> bool {
    path.is_file()
        && read_json::<Intake>(path).is_ok_and(|intake| !intake.problem.trim().is_empty())
}

#[async_trait]
impl Phase for DiscoverIntakePhase {
    fn name(&self) -> &'static str {
        "intake"
    }

    /// Return the existing `brief.json` when it is valid; otherwise
    /// run [`IntakePhase`] unchanged.
    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let brief = ctx.run_dir().brief();
        if brief_is_valid(&brief) {
            tracing::info!(brief = %brief.display(), "intake: brief.json is valid; reused");
            return Ok(PhaseOutput::Intake(brief));
        }
        IntakePhase.execute(ctx).await
    }
}

#[cfg(test)]
mod tests;
