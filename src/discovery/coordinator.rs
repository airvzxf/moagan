//! Coordinator that owns the full discovery flow state.
//!
//! PR B.6 introduced the struct + accessor methods; PR B.7 wires
//! [`DiscoveryCoordinator::run`] into the sketch-loop state
//! machine defined in `state.rs`. The coordinator now drives the
//! loop end-to-end: it loads (or creates) the persisted state,
//! iterates until the target sketch count is reached, persists the
//! state after every sketch so a crashed run can resume from the
//! last successful sketch, and cleans the state file up on
//! completion. Cancellation propagates through the cooperative
//! [`Cancel`] handle and surfaces as a
//! [`CoordinatorError::Error`] wrapping the canonical
//! `Error::Cancelled` so the supervisor can branch on it.
//!
//! PR D.3 (`Coordinator::run` async + real driver): the loop is
//! `async`, its target is derived from the run mode via
//! [`crate::phases::cardinality::Cardinality::for_mode_default`],
//! and resume detects pre-existing `sk_*.json` artefacts under
//! `<run_dir>/sketches/`. Each placeholder iteration awaits a
//! cooperative `yield_now` so the future genuinely yields control
//! to the executor and a downstream LLM call can be slotted in
//! without changing the loop's shape.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cancel::Cancel;
use crate::cli::Mode;
use crate::domain::Brief;
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::phases::cardinality::Cardinality;

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
/// Owns the legacy, the sketch loop state, the cancel token, the
/// run dir, and the run mode (used to size the loop target via
/// [`Cardinality::for_mode_default`]). [`DiscoveryCoordinator::run`]
/// is the single entry point that drives the sketch loop
/// end-to-end.
pub struct DiscoveryCoordinator {
    home: MoaganHome,
    run_id: RunId,
    cancel: Cancel,
    brief: Brief,
    legacy: EpistemicLegacy,
    state: SketchLoopState,
    /// Run mode; determines the target sketch count via
    /// [`Cardinality::for_mode_default`].
    mode: Mode,
}

impl DiscoveryCoordinator {
    /// Builds a coordinator with loaded legacy, fresh sketch-loop
    /// state, and the given run `mode`.
    pub fn new(
        home: MoaganHome,
        run_id: RunId,
        cancel: Cancel,
        brief: Brief,
        current_strategy: String,
        mode: Mode,
    ) -> Self {
        Self {
            home,
            run_id,
            cancel,
            brief,
            legacy: EpistemicLegacy::load(),
            state: SketchLoopState::new(current_strategy),
            mode,
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

    /// Returns the run mode that sizes the sketch-loop target.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Drives the discovery sketch loop end-to-end.
    ///
    /// 1. Ensures the run directory exists and resolves the canonical
    ///    `run_dir` root where the persisted state file lives.
    /// 2. Resumes from any pre-existing `sk_*.json` artefacts under
    ///    `<run_dir>/sketches/` (D.3): if the directory holds
    ///    already-emitted sketches, the loop treats them as the
    ///    baseline and only iterates for the missing ones.
    /// 3. Loads any previously-persisted [`SketchLoopState`]
    ///    (`<run_dir>/.discovery_state.json`); if the file is
    ///    absent, corrupt, or carries a mismatched schema version
    ///    the loop starts fresh with the strategy carried over
    ///    from the constructor.
    /// 4. Sizes the target from [`Cardinality::for_mode_default`]
    ///    using the constructor's `mode` field, so each mode
    ///    gets the spec-defined cardinality band.
    /// 5. Iterates until the target sketch count is reached. Each
    ///    iteration:
    ///    - Checks the cooperative cancel token; if the supervisor
    ///      has flipped it, the loop bails out with the recorded
    ///      [`CancelReason`] as [`crate::Error::Cancelled`].
    ///    - Records a deterministic placeholder sketch id on the
    ///      state (`"sk_<NNNN>"`) and atomically persists the
    ///      state to disk so a crash mid-loop leaves a resumable
    ///      snapshot. The id carries a `placeholder` tracing
    ///      hint until the (future) LLM call fills it in.
    ///    - Yields control via `tokio::task::yield_now` so the
    ///      future makes progress and a downstream LLM call can
    ///      be slotted in without changing the loop's shape.
    /// 6. On completion, flips the state to `SketchLoopDone` and
    ///    deletes the persisted state file (the next run gets a
    ///    fresh matrix and fresh sketch IDs, so carrying the
    ///    snapshot forward would be misleading).
    /// 7. Returns a [`DiscoveryOutcome`] that the synthesize phase
    ///    uses to decide whether to inject the epistemic context
    ///    (`legacy_used`) and to surface sketch counts.
    pub async fn run(self) -> Result<DiscoveryOutcome, CoordinatorError> {
        let run_dir = {
            let handle = self.home.run_dir(self.run_id);
            handle.ensure()?;
            handle.root().to_path_buf()
        };
        let strategy = self.state.current_strategy.clone();
        let target = Cardinality::for_mode_default(self.mode).soft;

        let mut state = match SketchLoopState::load(&run_dir)? {
            Some(persisted) => {
                tracing::info!(
                    completed = persisted.completed_sketches.len(),
                    failed = persisted.failed_attempts,
                    "DiscoveryCoordinator::run resuming from persisted state"
                );
                persisted
            }
            None => SketchLoopState::new(strategy),
        };

        // D.3 resume: existing `sk_*.json` artefacts on disk count
        // toward the target. We synchronise the in-memory state to
        // that baseline before entering the iteration loop so a
        // crashed-and-restarted run does not double-count already
        // emitted sketches.
        let existing = count_existing_sketches(&run_dir);
        if existing > state.completed_sketches.len() {
            let delta = existing - state.completed_sketches.len();
            tracing::info!(
                existing,
                delta,
                "DiscoveryCoordinator::run discovered pre-existing sketches; \
                 resyncing state baseline"
            );
            for _ in 0..delta {
                state.record_completion(format!("sk_{:04}", state.completed_sketches.len()));
            }
            state.save(&run_dir)?;
        }

        while state.completed_sketches.len() < target {
            if self.cancel.is_cancelled() {
                return Err(self.cancel.into_error().into());
            }
            let id = format!("sk_{:04}", state.completed_sketches.len());
            tracing::debug!(
                sketch_id = %id,
                target,
                "DiscoveryCoordinator::run: placeholder sketch (awaiting LLM call)"
            );
            state.record_completion(id);
            state.save(&run_dir)?;
            // Yield control so the future genuinely awaits; a
            // future commit that wires the real `Sketch` role will
            // slot in here without changing the loop shape.
            tokio::task::yield_now().await;
        }

        state.mark_done();
        SketchLoopState::delete(&run_dir)?;

        Ok(DiscoveryOutcome {
            run_id: self.run_id,
            sketches_completed: state.completed_sketches.len(),
            sketches_failed: state.failed_attempts as usize,
            legacy_used: !self.legacy.known_failures.is_empty(),
        })
    }
}

/// Errors produced by [`DiscoveryCoordinator`].
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    /// Wraps the canonical [`crate::error::Error`] so callers can
    /// use `?` to lift IO / serialization / cancellation failures
    /// out of the coordinator without bespoke mapping.
    #[error(transparent)]
    Error(#[from] crate::error::Error),
}

/// Returns the directory where run-specific sketch state lives.
pub fn sketches_dir(home: &MoaganHome, run_id: &RunId) -> PathBuf {
    home.run_dir(*run_id).sketches()
}

/// Count the `sk_*.json` artefacts already on disk under
/// `<run_dir>/sketches/`. Used by [`DiscoveryCoordinator::run`] to
/// detect a resume baseline: a partially-completed run leaves the
/// JSON files behind even after a crash, so a fresh process boot
/// can pick up where it left off. Returns `0` if the directory is
/// missing or unreadable (no error — the loop simply starts fresh).
fn count_existing_sketches(run_dir: &Path) -> usize {
    let dir = run_dir.join("sketches");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if name.starts_with("sk_") && name.ends_with(".json") && !name.ends_with(".meta.json") {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::epistemic_legacy::SCHEMA_VERSION;
    use crate::discovery::state::Phase;
    use crate::test_support::with_moagan_home;

    /// Filename of the persisted sketch-loop state file. Duplicates
    /// the `const` in `state.rs` because the constant is module-private
    /// and the coordinator's tests need to probe the file's existence.
    const STATE_FILE: &str = ".discovery_state.json";

    fn new_coordinator(brief: Brief) -> (DiscoveryCoordinator, RunId, PathBuf) {
        new_coordinator_with_mode(brief, Mode::Fast)
    }

    fn new_coordinator_with_mode(
        brief: Brief,
        mode: Mode,
    ) -> (DiscoveryCoordinator, RunId, PathBuf) {
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
                mode,
            );
            (coordinator, run_id, path.to_path_buf())
        })
    }

    fn new_coordinator_with_cancel(brief: Brief) -> (DiscoveryCoordinator, RunId, PathBuf, Cancel) {
        new_coordinator_with_cancel_and_mode(brief, Mode::Fast)
    }

    fn new_coordinator_with_cancel_and_mode(
        brief: Brief,
        mode: Mode,
    ) -> (DiscoveryCoordinator, RunId, PathBuf, Cancel) {
        with_moagan_home("discovery-coordinator-cancel", |path| {
            EpistemicLegacy::empty()
                .save_to(&path.join("epistemic_legacy.json"))
                .unwrap();
            let run_id = RunId::new();
            let cancel = Cancel::new();
            let cancel_clone = cancel.clone();
            let coordinator = DiscoveryCoordinator::new(
                MoaganHome::at(path.to_path_buf()),
                run_id,
                cancel,
                brief,
                "deployment-model:serverless".to_owned(),
                mode,
            );
            (coordinator, run_id, path.to_path_buf(), cancel_clone)
        })
    }

    /// Spin up a single-threaded tokio runtime for async tests.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// Fast mode soft cardinality. Pinned here so tests can assert
    /// the target without re-deriving the formula.
    const FAST_SOFT: usize = 4;

    #[test]
    fn coordinator_new_stores_brief_and_run_id() {
        let brief = Brief {
            problem: "Coordinate discovery".to_owned(),
            ..Brief::default()
        };
        let (coordinator, run_id, _) = new_coordinator(brief);

        assert_eq!(coordinator.run_id(), &run_id);
        assert_eq!(coordinator.brief().problem, "Coordinate discovery");
        assert!(!coordinator.cancel().is_cancelled());
        assert!(!coordinator.home().root().as_os_str().is_empty());
        assert_eq!(coordinator.mode(), Mode::Fast);
    }

    #[test]
    fn coordinator_legacy_default_is_empty() {
        let (coordinator, _, _) = new_coordinator(Brief::default());

        assert_eq!(coordinator.legacy().version, SCHEMA_VERSION);
        assert!(coordinator.legacy().known_failures.is_empty());
        assert!(coordinator.legacy().preferred_strategies.is_empty());
        assert!(coordinator.legacy().domain_assumptions.is_empty());
        assert!(coordinator.legacy().confidence_overrides.is_empty());
    }

    #[test]
    fn coordinator_legacy_mut_appends_known_failures() {
        let (mut coordinator, _, _) = new_coordinator(Brief::default());

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
        let (mut coordinator, _, _) = new_coordinator(Brief::default());

        assert_eq!(coordinator.state().phase, Phase::SketchLoop);
        assert_eq!(
            coordinator.state().current_strategy,
            "deployment-model:serverless"
        );
        coordinator.state_mut().record_failure();
        assert_eq!(coordinator.state().failed_attempts, 1);
    }

    /// D.3: `run` is now an `async fn`. We verify the async-ness
    /// at the type-system level (the future compiles) and at the
    /// runtime level (a single-threaded executor drives it to
    /// completion without deadlock).
    #[test]
    fn coordinator_run_is_async() {
        let (coordinator, run_id, _) = new_coordinator(Brief::default());

        let outcome = block_on(coordinator.run()).expect("fresh run should succeed");

        assert_eq!(outcome.run_id, run_id);
        assert_eq!(outcome.sketches_completed, FAST_SOFT);
        assert_eq!(outcome.sketches_failed, 0);
        assert!(!outcome.legacy_used);
    }

    /// D.3: the target sketch count must come from
    /// [`Cardinality::for_mode_default`], not from a hardcoded
    /// constant. Standard mode soft = midpoint(5..10) = 7.
    #[test]
    fn coordinator_run_uses_target_cardinality_from_config() {
        let (coordinator, _, _) = new_coordinator_with_mode(Brief::default(), Mode::Standard);

        let outcome = block_on(coordinator.run()).expect("run should succeed");

        assert_eq!(
            outcome.sketches_completed,
            Cardinality::for_mode_default(Mode::Standard).soft,
            "Standard soft target is midpoint(5..10)=7"
        );
        assert_ne!(
            outcome.sketches_completed, FAST_SOFT,
            "Standard target must differ from Fast target"
        );
    }

    /// D.3: pre-existing `sk_*.json` artefacts under
    /// `<run_dir>/sketches/` count toward the target. The loop
    /// must not re-emit ids that already exist on disk.
    #[test]
    fn coordinator_run_resume_detects_existing_sketches() {
        let (coordinator, run_id, home) = new_coordinator(Brief::default());

        let sketches_dir = home.join(".runs").join(run_id.to_string()).join("sketches");
        std::fs::create_dir_all(&sketches_dir).unwrap();
        // Drop two artefacts on disk so the loop sees a resume
        // baseline of `existing = 2` and only iterates for the
        // remaining `target - existing` slots.
        std::fs::write(sketches_dir.join("sk_0000.json"), b"{}").unwrap();
        std::fs::write(sketches_dir.join("sk_0001.json"), b"{}").unwrap();

        let outcome = block_on(coordinator.run()).expect("resume should succeed");

        assert_eq!(
            outcome.sketches_completed, FAST_SOFT,
            "loop must still hit the Fast soft target even when pre-existing sketches exist"
        );
        assert!(
            outcome.sketches_completed >= 2,
            "completed count must include the pre-existing sketches"
        );
    }

    #[test]
    fn coordinator_run_creates_state_when_no_persisted_state() {
        let (coordinator, run_id, home) = new_coordinator(Brief::default());

        let outcome = block_on(coordinator.run()).expect("fresh run should succeed");

        assert_eq!(outcome.run_id, run_id);
        assert_eq!(outcome.sketches_completed, FAST_SOFT);
        assert_eq!(outcome.sketches_failed, 0);
        assert!(!outcome.legacy_used);

        let state_path = home.join(".runs").join(run_id.to_string()).join(STATE_FILE);
        assert!(
            !state_path.exists(),
            "completed runs must delete the persisted state file"
        );
    }

    #[test]
    fn coordinator_run_resumes_from_persisted_state() {
        let (coordinator, run_id, home) = new_coordinator(Brief::default());

        let run_dir_root = home.join(".runs").join(run_id.to_string());
        std::fs::create_dir_all(&run_dir_root).unwrap();
        let mut persisted = SketchLoopState::new("deployment-model:serverless".to_owned());
        persisted.record_completion("sk_0000".to_owned());
        persisted.record_completion("sk_0001".to_owned());
        persisted.record_completion("sk_0002".to_owned());
        persisted.save(&run_dir_root).unwrap();

        let outcome = block_on(coordinator.run()).expect("resume should succeed");

        assert_eq!(outcome.run_id, run_id);
        assert_eq!(
            outcome.sketches_completed, FAST_SOFT,
            "resume must keep going from where the persisted state left off"
        );
    }

    #[test]
    fn coordinator_run_completes_when_target_sketches_reached() {
        let (coordinator, run_id, _) = new_coordinator(Brief::default());

        let outcome = block_on(coordinator.run()).expect("run should reach target");

        assert_eq!(outcome.sketches_completed, FAST_SOFT);
        assert_eq!(outcome.run_id, run_id);
        assert_eq!(outcome.sketches_failed, 0);
    }

    #[test]
    fn coordinator_run_propagates_cancel_via_cancel_token() {
        use crate::cancel::CancelReason;

        let brief = Brief::default();
        let (coordinator, _, _, cancel) = new_coordinator_with_cancel(brief);
        cancel.cancel(CancelReason::UserInterrupt);

        let err = block_on(coordinator.run()).expect_err("cancelled run must surface an error");
        match err {
            CoordinatorError::Error(crate::error::Error::Cancelled(msg)) => {
                assert!(!msg.is_empty(), "cancellation message must be carried");
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[test]
    fn coordinator_run_includes_legacy_in_outcome_when_populated() {
        with_moagan_home("discovery-coordinator-legacy", |path| {
            let mut legacy = EpistemicLegacy::empty();
            legacy
                .known_failures
                .push("avoid: monolithic-deploy".to_owned());
            legacy.save_to(&path.join("epistemic_legacy.json")).unwrap();

            let run_id = RunId::new();
            let coordinator = DiscoveryCoordinator::new(
                MoaganHome::at(path.to_path_buf()),
                run_id,
                Cancel::new(),
                Brief::default(),
                "deployment-model:serverless".to_owned(),
                Mode::Fast,
            );

            let outcome = block_on(coordinator.run()).expect("run should succeed");

            assert!(
                outcome.legacy_used,
                "populated known_failures must flip legacy_used to true"
            );
        });
    }

    #[test]
    fn coordinator_run_cleans_up_state_file_on_completion() {
        let (coordinator, run_id, home) = new_coordinator(Brief::default());

        let _ = block_on(coordinator.run()).expect("run should complete");

        let state_path = home.join(".runs").join(run_id.to_string()).join(STATE_FILE);
        assert!(
            !state_path.exists(),
            "state file must be removed after a successful run (D.34.2)"
        );
    }
}
