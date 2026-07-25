//! `moagan continue`, `moagan resume`, `moagan rerun` — run-state
//! operations. In v0.1 these are minimal: they verify the run exists
//! and report its status. Full re-execution lands in v0.2.

use crate::error::{Error, Result};
use crate::ids::RunId;

/// Stub for `moagan continue`. Returns a friendly "not yet" error.
pub fn run_continue(run_id: RunId) -> Result<()> {
    Err(Error::InvalidState(format!(
        "continue for {run_id} not yet implemented; v0.2 will resume from manifest"
    )))
}

/// Stub for `moagan resume`.
pub fn run_resume(run_id: RunId) -> Result<()> {
    Err(Error::InvalidState(format!(
        "resume for {run_id} not yet implemented; v0.2 will resume mid-phase"
    )))
}

/// Stub for `moagan rerun`.
pub fn run_rerun(run_id: RunId) -> Result<()> {
    Err(Error::InvalidState(format!(
        "rerun for {run_id} not yet implemented; v0.2 will clone the run config"
    )))
}
