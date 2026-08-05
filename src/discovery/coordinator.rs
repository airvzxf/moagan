//! Coordinator that owns the full discovery flow state.
//!
//! PR B.6 introduces the struct + accessor methods; PR B.7 wires it
//! into `discover_matrix`. The coordinator carries the legacy,
//! the sketch loop state, the cancel token, and the run dir; once
//! the wiring lands it will own the loop dispatch as well.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cancel::Cancel;
use crate::domain::Brief;
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;

use super::epistemic_legacy::EpistemicLegacy;
use super::state::SketchLoopState;

/// Public outcome summary used by callers (and tests).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryOutcome {
    /// Identifier of the discovery run.
    pub run_id: RunId,
    /// Number of sketches completed successfully.
    pub sketches_completed: usize,
    /// Number of sketches that failed.
    pub sketches_failed: usize,
    /// Whether epistemic legacy influenced the run.
    pub legacy_used: bool,
}

/// Coordinates the discovery flow for a single run.
///
/// In PR B.6 only the state-holding methods are real; `run()` is
/// wired in PR B.7.
pub struct DiscoveryCoordinator {
    home: MoaganHome,
    run_id: RunId,
    cancel: Cancel,
    brief: Brief,
    legacy: EpistemicLegacy,
    state: SketchLoopState,
}

impl DiscoveryCoordinator {
    /// Builds a coordinator with loaded legacy and fresh sketch-loop state.
    pub fn new(
        home: MoaganHome,
        run_id: RunId,
        cancel: Cancel,
        brief: Brief,
        current_strategy: String,
    ) -> Self {
        Self {
            home,
            run_id,
            cancel,
            brief,
            legacy: EpistemicLegacy::load(),
            state: SketchLoopState::new(current_strategy),
        }
    }

    /// Returns the resolved Moagan home.
    pub fn home(&self) -> &MoaganHome {
        &self.home
    }

    /// Returns the run identifier.
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Returns the cooperative cancellation handle.
    pub fn cancel(&self) -> &Cancel {
        &self.cancel
    }

    /// Returns the canonical brief.
    pub fn brief(&self) -> &Brief {
        &self.brief
    }

    /// Returns the loaded epistemic legacy.
    pub fn legacy(&self) -> &EpistemicLegacy {
        &self.legacy
    }

    /// Returns mutable access to the loaded epistemic legacy.
    pub fn legacy_mut(&mut self) -> &mut EpistemicLegacy {
        &mut self.legacy
    }

    /// Returns the sketch-loop state.
    pub fn state(&self) -> &SketchLoopState {
        &self.state
    }

    /// Returns mutable access to the sketch-loop state.
    pub fn state_mut(&mut self) -> &mut SketchLoopState {
        &mut self.state
    }

    /// Placeholder that returns [`CoordinatorError::NotImplemented`] until PR B.7.
    pub fn run(self) -> Result<DiscoveryOutcome, CoordinatorError> {
        let _ = self;
        Err(CoordinatorError::NotImplemented)
    }
}

/// Errors produced by [`DiscoveryCoordinator`].
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    /// The discovery loop dispatch is deferred to PR B.7.
    #[error("discovery coordinator run() not implemented yet (lands in PR B.7)")]
    NotImplemented,
}

/// Returns the directory where run-specific sketch state lives.
pub fn sketches_dir(home: &MoaganHome, run_id: &RunId) -> PathBuf {
    home.run_dir(*run_id).sketches()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::epistemic_legacy::SCHEMA_VERSION;
    use crate::discovery::state::Phase;
    use crate::test_support::with_moagan_home;

    fn new_coordinator(brief: Brief) -> (DiscoveryCoordinator, RunId) {
        with_moagan_home("discovery-coordinator", |path| {
            EpistemicLegacy::empty()
                .save_to(&path.join("epistemic_legacy.json"))
                .unwrap();
            let run_id = RunId::new();
            let coordinator = DiscoveryCoordinator::new(
                MoaganHome::at(path.to_path_buf()),
                run_id,
                Cancel::new(),
                brief,
                "deployment-model:serverless".to_owned(),
            );
            (coordinator, run_id)
        })
    }

    #[test]
    fn coordinator_new_stores_brief_and_run_id() {
        let brief = Brief {
            problem: "Coordinate discovery".to_owned(),
            ..Brief::default()
        };
        let (coordinator, run_id) = new_coordinator(brief);

        assert_eq!(coordinator.run_id(), &run_id);
        assert_eq!(coordinator.brief().problem, "Coordinate discovery");
        assert!(!coordinator.cancel().is_cancelled());
        assert!(!coordinator.home().root().as_os_str().is_empty());
    }

    #[test]
    fn coordinator_legacy_default_is_empty() {
        let (coordinator, _) = new_coordinator(Brief::default());

        assert_eq!(coordinator.legacy().version, SCHEMA_VERSION);
        assert!(coordinator.legacy().known_failures.is_empty());
        assert!(coordinator.legacy().preferred_strategies.is_empty());
        assert!(coordinator.legacy().domain_assumptions.is_empty());
        assert!(coordinator.legacy().confidence_overrides.is_empty());
    }

    #[test]
    fn coordinator_legacy_mut_appends_known_failures() {
        let (mut coordinator, _) = new_coordinator(Brief::default());

        coordinator
            .legacy_mut()
            .known_failures
            .push("repeated parse failure".to_owned());

        assert_eq!(
            coordinator.legacy().known_failures,
            vec!["repeated parse failure".to_owned()]
        );
    }

    #[test]
    fn coordinator_state_default_is_sketch_loop_phase() {
        let (mut coordinator, _) = new_coordinator(Brief::default());

        assert_eq!(coordinator.state().phase, Phase::SketchLoop);
        assert_eq!(
            coordinator.state().current_strategy,
            "deployment-model:serverless"
        );
        coordinator.state_mut().record_failure();
        assert_eq!(coordinator.state().failed_attempts, 1);
    }

    #[test]
    fn coordinator_run_returns_not_implemented_until_b7() {
        let (coordinator, _) = new_coordinator(Brief::default());

        assert!(matches!(
            coordinator.run(),
            Err(CoordinatorError::NotImplemented)
        ));
    }
}
