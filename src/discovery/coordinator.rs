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
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cancel::Cancel;
use crate::cli::Mode;
use crate::domain::Brief;
use crate::domain::Sketch;
use crate::error::Error;
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::llm::prompts::discover_matrix_system_prompt;
use crate::phases::cardinality::Cardinality;
use crate::phases::phase::RunContext;
use crate::phases::util::{read_json, write_json};
use crate::telemetry::event::TelemetryEvent;

use super::epistemic_legacy::EpistemicLegacy;
use super::matrix::{ExplorationMatrix, MatrixCell};
use super::persona_angle;
use super::saturation::SaturationTracker;
use super::sketch_retry::retry_sketch_extraction;
use super::state::SketchLoopState;
use super::stop_policy::{StopDecision, StopPolicy, StopReason};

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

    /// Returns the sketch-loop state.
    pub fn state(&self) -> &SketchLoopState {
        &self.state
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
        self.run_with_pickers(None, Vec::new(), Vec::new()).await
    }

    /// Track E (E8 wire-up): drives the discovery loop with the
    /// optional persona/angle pickers attached.
    ///
    /// `picker_ctx` carries the [`RunContext`] (providers,
    /// telemetry, config) the persona/angle helpers need. When
    /// `None`, the helpers are skipped — this is what
    /// [`DiscoveryCoordinator::run`] does, preserving the
    /// self-contained behavior for callers that have not yet wired
    /// the discovery wiring config into a phase boundary.
    ///
    /// Trigger rules:
    ///
    /// * [`persona_angle::pick_persona`] runs once at loop start
    ///   when `picker_ctx.is_some()` AND
    ///   `ctx.config.discovery.persona_enabled` AND the
    ///   mode-derived `target` exceeds `4` AND
    ///   `!candidates.is_empty()`. Below the threshold the model
    ///   persona sweet spot is too narrow to be worth a call.
    /// * [`persona_angle::pick_angle`] runs once at loop end when
    ///   `picker_ctx.is_some()` AND
    ///   `ctx.config.discovery.angle_enabled` AND
    ///   `clusters.len() > ctx.config.discovery.angle_clusters_min`.
    ///   The supplied `clusters` come from the upstream
    ///   integrate phase in the eventual full wire-up; for the
    ///   follow-up tasks that build SketchPhase +
    ///   IntegratePhase this signature stays stable.
    ///
    /// Both helpers are soft — a picker failure surfaces as a
    /// `tracing::warn!` and the loop continues without that
    /// signal (the picker is opt-in additive, never load-bearing).
    pub async fn run_with_pickers(
        self,
        picker_ctx: Option<Arc<RunContext>>,
        candidates: Vec<String>,
        clusters: Vec<String>,
    ) -> Result<DiscoveryOutcome, CoordinatorError> {
        let DiscoveryCoordinator {
            home,
            run_id,
            cancel,
            brief: _,
            mut legacy,
            state,
            mode,
        } = self;

        let run_dir = {
            let handle = home.run_dir(run_id);
            handle.ensure()?;
            handle.root().to_path_buf()
        };
        let strategy = state.current_strategy.clone();
        let target = Cardinality::for_mode_default(mode).soft;

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

        // E8 wire-up: persona picker trigger. Runs once at loop
        // start when the wiring is enabled AND the cardinality
        // band exceeds the persona sweet spot. We hold the picker
        // result in a tracing record + telemetry PhaseStart so
        // downstream phases can branch on the choice.
        if let Some(ctx) = picker_ctx.as_ref()
            && ctx.config.discovery.persona_enabled
            && target > 4
        {
            match persona_angle::pick_persona(ctx, candidates).await {
                Ok(Some(persona)) => {
                    tracing::info!(persona = %persona, "persona selected");
                    TelemetryEvent::PhaseStart {
                        run_id: run_id.to_string(),
                        phase: format!("persona_selection:{persona}"),
                        at_unix: crate::time::now_unix_secs(),
                    }
                    .emit();
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "persona picker failed; continuing without persona"
                    );
                }
            }
        }

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
            if cancel.is_cancelled() {
                return Err(cancel.into_error().into());
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

        // E8 wire-up: angle picker trigger. Runs once at loop end
        // when the wiring is enabled AND the cluster list crosses
        // the `angle_clusters_min` threshold. The chosen angle is
        // appended to the in-memory legacy by the helper; we then
        // re-emit the choice via tracing + telemetry so the rest
        // of the run surfaces it.
        if let Some(ctx) = picker_ctx.as_ref()
            && ctx.config.discovery.angle_enabled
            && clusters.len() > ctx.config.discovery.angle_clusters_min
        {
            match persona_angle::pick_angle(ctx, &mut legacy, clusters).await {
                Ok(Some(angle)) => {
                    tracing::info!(angle = %angle, "angle selected");
                    TelemetryEvent::PhaseStart {
                        run_id: run_id.to_string(),
                        phase: format!("angle_selection:{angle}"),
                        at_unix: crate::time::now_unix_secs(),
                    }
                    .emit();
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "angle picker failed; continuing without angle"
                    );
                }
            }
        }

        Ok(DiscoveryOutcome {
            run_id,
            sketches_completed: state.completed_sketches.len(),
            sketches_failed: state.failed_attempts as usize,
            legacy_used: !legacy.known_failures.is_empty(),
        })
    }

    /// PR-17 (PR B.7 real driver): drives the sketch loop with the
    /// supplied [`RunContext`], calling the LLM through the canonical
    /// retry / parse / telemetry pipeline for every `(cell, sketch_index)`
    /// pair. This replaces the placeholder body of
    /// [`DiscoveryCoordinator::run`] for callers that have a real
    /// [`RunContext`] available; the existing 19 unit tests keep
    /// working because they exercise the placeholder path through
    /// [`DiscoveryCoordinator::run`] / [`DiscoveryCoordinator::run_with_pickers`].
    ///
    /// When `target_override` is `Some(n)`, the matrix is sized from
    /// `n` instead of `Cardinality::for_mode_default(mode).soft`. The
    /// CLI uses this to honour `--cardinality` directly so the
    /// matrix actually fans out to the operator-requested sketch
    /// count (the mode-derived default is too small for discovery —
    /// the spec mandates ≥80 sketches).
    ///
    /// # Flow
    ///
    /// 1. Build the [`ExplorationMatrix`] from `target` (either the
    ///    caller-supplied `target_override` or the mode-derived
    ///    `Cardinality::for_mode_default(mode).soft`). The matrix is
    ///    persisted to `<run_dir>/exploration_matrix.json` so a
    ///    follow-up run can verify reproducibility.
    /// 2. Read the canonical brief from `<run_dir>/brief.json` (the
    ///    file the upstream `intake` / `clarify` phases wrote).
    ///    The brief is the user payload the LLM sees for every cell.
    /// 3. Load any previously-persisted [`SketchLoopState`] so a
    ///    crashed mid-loop run can resume from the last completed
    ///    sketch. A missing or schema-mismatched file is normal on
    ///    a fresh run and starts the loop from scratch.
    /// 4. Initialize a [`SaturationTracker`] with the default
    ///    [`StopPolicy`] so the loop observes the spec's
    ///    `~60 sketches at 50% saturation` contract.
    /// 5. Iterate over every `(cell, sketch_index)` pair. Each
    ///    iteration:
    ///    - Acquires a parallelism permit (`ctx.parallelism`) so the
    ///      fan-out honours the operator-configured cap.
    ///    - Drives the LLM call through
    ///      [`retry_sketch_extraction`] (D.34.1 / PR-05) so the
    ///      matrix's 3-attempt ceiling survives even in `fast` mode.
    ///    - Persists the sketch to `<run_dir>/sketches/sk_<NNNN>.json`.
    ///    - Records the completion in the [`SketchLoopState`] and
    ///      atomically writes the state file so a crashed resume can
    ///      recover from disk.
    ///    - Updates the [`SaturationTracker`] and applies the
    ///      [`StopDecision`]. A `Stop { Saturated }` decision emits
    ///      a [`TelemetryEvent::DiscoverySaturated`] so the
    ///      post-execution review can correlate the saturation trip
    ///      with the cluster mean-similarity signal.
    ///    - Yields cooperatively via `tokio::task::yield_now` so
    ///      cancellation and the cancel-token probe remain prompt.
    /// 6. On a clean stop (target reached OR `StopDecision::Stop`),
    ///    deletes the persisted state file so the next run starts
    ///    fresh. The sketches under `<run_dir>/sketches/` survive
    ///    and feed the downstream tag / cluster / facet phases.
    /// 7. Returns a [`DiscoveryOutcome`] that downstream code can
    ///    inspect via `legacy_used` and the per-run counters.
    pub async fn run_with_ctx(
        self,
        ctx: Arc<RunContext>,
    ) -> Result<DiscoveryOutcome, CoordinatorError> {
        self.run_with_ctx_and_target(ctx, None).await
    }

    /// Variant of [`DiscoveryCoordinator::run_with_ctx`] that
    /// accepts an explicit cardinality target. The CLI uses this to
    /// honour `--cardinality` directly; tests and callers that
    /// want the mode-derived default should stick with
    /// [`DiscoveryCoordinator::run_with_ctx`].
    pub async fn run_with_ctx_and_target(
        self,
        ctx: Arc<RunContext>,
        target_override: Option<usize>,
    ) -> Result<DiscoveryOutcome, CoordinatorError> {
        let DiscoveryCoordinator {
            home,
            run_id,
            cancel,
            brief: _,
            mut legacy,
            state,
            mode,
        } = self;

        let run_dir = {
            let handle = home.run_dir(run_id);
            handle.ensure()?;
            handle.root().to_path_buf()
        };
        let strategy = state.current_strategy.clone();
        let mode_target = Cardinality::for_mode_default(mode).soft;
        let target = target_override.unwrap_or(mode_target);

        // 1. Build and persist the matrix so a follow-up run can
        //    verify reproducibility without re-running the fan-out.
        //
        // PR-D1: pull the per-provider temperature profiles and the
        // optional default profile from `ctx.config.discovery_matrix`
        // (the CLI merged its `--temperature-profile` flags on top
        // of the persisted `[discovery]` block before constructing
        // the `RunContext`, so the values here are the final ones).
        // When the operator sets neither, the map is empty and the
        // default profile falls back to `TemperatureProfile::default()`
        // (`[1.0] × 1`) — bit-identical to v0.5.
        let temperature_profiles = ctx.config.discovery_matrix.temperature_profiles.clone();
        let default_profile = ctx
            .config
            .discovery_matrix
            .default_profile
            .clone()
            .unwrap_or_default();
        let matrix = ExplorationMatrix::default_for_with_profiles(
            target,
            temperature_profiles,
            default_profile,
        );
        let matrix_path = run_dir.join("exploration_matrix.json");
        write_json(&matrix_path, &matrix)?;
        tracing::info!(
            cells = matrix.cells(),
            per_cell = matrix.sketches_per_cell,
            target,
            matrix_cardinality = matrix.cardinality(),
            "DiscoveryCoordinator::run_with_ctx built exploration matrix"
        );

        // 1b. D.13.18 (v0.5 PR-18): auto-invoke `run_with_pickers`
        //     when `Config::discovery.auto_pickers` is `true`
        //     (the default). The persona picker fires first (it
        //     needs the candidate persona pool) and the angle
        //     picker fires second (it needs a cluster list). The
        //     synthetic lists are derived from the matrix's own
        //     dimensions + cells so a fresh coordinator run has
        //     non-empty inputs that satisfy the per-picker gates
        //     (`pick_persona` requires `!candidates.is_empty()`,
        //     `pick_angle` requires
        //     `clusters.len() > angle_clusters_min`); both
        //     short-circuit otherwise and the audit sidecar's
        //     `calls.jsonl.gz` would miss the `persona_picker` /
        //     `angle_picker` rows the v0.5 PR-18 contract
        //     promises. The matrix fan-out below issues
        //     `Role::Sketch` calls, so the picker rows in
        //     `calls.jsonl.gz` always precede the matrix rows by
        //     construction. Both pickers stay opt-in via their
        //     individual `*_enabled` flags so an operator can
        //     disable just one without flipping `auto_pickers`.
        if ctx.config.discovery.auto_pickers {
            let auto_candidates: Vec<String> =
                matrix.dimensions.iter().map(|d| d.id.clone()).collect();
            let auto_clusters: Vec<String> = matrix.iter_cells().map(|c| c.label.clone()).collect();
            if ctx.config.discovery.persona_enabled && !auto_candidates.is_empty() && target > 4 {
                match persona_angle::pick_persona(&ctx, auto_candidates).await {
                    Ok(Some(persona)) => {
                        tracing::info!(persona = %persona, "persona selected (auto-invoke)");
                        TelemetryEvent::PhaseStart {
                            run_id: run_id.to_string(),
                            phase: format!("persona_selection:{persona}"),
                            at_unix: crate::time::now_unix_secs(),
                        }
                        .emit();
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "persona picker failed during auto-invoke; continuing without persona"
                        );
                    }
                }
            }
            if ctx.config.discovery.angle_enabled
                && auto_clusters.len() > ctx.config.discovery.angle_clusters_min
            {
                match persona_angle::pick_angle(&ctx, &mut legacy, auto_clusters).await {
                    Ok(Some(angle)) => {
                        tracing::info!(angle = %angle, "angle selected (auto-invoke)");
                        TelemetryEvent::PhaseStart {
                            run_id: run_id.to_string(),
                            phase: format!("angle_selection:{angle}"),
                            at_unix: crate::time::now_unix_secs(),
                        }
                        .emit();
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "angle picker failed during auto-invoke; continuing without angle"
                        );
                    }
                }
            }
        }

        // 2. Read the canonical brief from disk so the LLM payload
        //    matches what the upstream intake + clarify phases
        //    produced. The brief is always present on a fresh run
        //    because the pipeline's pre-matrix phases (intake,
        //    clarify) write it before the coordinator starts.
        let brief: serde_json::Value = read_json(&home.run_dir(run_id).brief())?;
        let brief_text = serde_json::to_string(&brief).map_err(Error::from)?;
        let system = Arc::new(discover_matrix_system_prompt().to_owned());

        let sketches_dir = run_dir.join("sketches");
        std::fs::create_dir_all(&sketches_dir).map_err(Error::from)?;

        // 3. Load (or initialize) the persisted state so a crashed
        //    mid-loop run resumes from the last successful sketch
        //    instead of redoing the whole fan-out.
        let mut state = match SketchLoopState::load(&run_dir)? {
            Some(persisted) => {
                tracing::info!(
                    completed = persisted.completed_sketches.len(),
                    failed = persisted.failed_attempts,
                    "DiscoveryCoordinator::run_with_ctx resuming from persisted state"
                );
                persisted
            }
            None => SketchLoopState::new(strategy),
        };

        // 4. Initialize the saturation tracker with the default
        //    policy. The matrix's cardinality drives the target.
        //
        // PR-D1: the tracker's target is the matrix's TOTAL fan-out
        // — `cells × sketches_per_cell × profile_total` — so the
        // saturation / outlier checks fire against the operator's
        // full profile expansion, not the v0.5 cardinality. With
        // the default profile (`[1.0] × 1`) this collapses to
        // `cells × sketches_per_cell` (v0.5). The `target_override`
        // wins when supplied so a CLI flag like `--cardinality 200`
        // still overrides the per-cell fan-out math.
        // (The loop computes the real `total` after resolving the
        //  profile below; we re-initialise the tracker once we
        //  know the real expansion factor.)
        let policy = StopPolicy::default();
        let mut tracker = SaturationTracker::with_policy(target.max(matrix.cardinality()), policy);

        // Replay the already-completed count into the tracker so a
        // resume picks up the right `completed` baseline.
        tracker.record_completions(state.completed_sketches.len());

        let per_cell = matrix.sketches_per_cell.max(1);
        let cells: Vec<MatrixCell> = matrix.iter_cells().collect();
        // PR-D1: per-provider temperature profile expansion. The
        // coordinator (the actual `moagan discover` driver) honours
        // the matrix's `temperature_profiles` map the same way the
        // flat `DiscoverMatrixPhase` does, so an operator who sets
        // `--temperature-profile 'provider=<model>;temperatures=...;replicas=...'`
        // observes the fan-out regardless of which code path the run
        // takes. The default profile is `[1.0] × 1`, so the
        // unconfigured case collapses to the v0.5 `(cells ×
        // per_cell)` total.
        let profile = matrix.profile_for(&ctx.default_model).clone();
        let profile_temperatures: Vec<f32> = profile.temperatures.clone();
        let profile_replicas: usize = profile.replicas_per_temperature.max(1);
        let total = cells.len() * profile_temperatures.len() * profile_replicas * per_cell;
        // PR-D1: re-anchor the saturation tracker's target to the
        // real total so the saturation / outlier checks fire
        // against the operator's full profile expansion. With the
        // default profile (`[1.0] × 1`) the new `total` equals
        // the matrix cardinality, so v0.5 runs are unaffected.
        // With a configured profile the tracker can run the longer
        // loop before declaring `MaxSketchesReached` (the
        // `Saturated` branch is structurally unreachable while
        // `clusters: &[Cluster]` is empty during the matrix loop).
        //
        // We previously multiplied `min_sketches` by the
        // `profile_expansion` so the `outliers_cap = min_sketches / 2`
        // safety net would scale with the expansion. That
        // multiplication was removed: the cluster-aware guard in
        // `SaturationTracker::update` (see `src/discovery/saturation.rs`)
        // already prevents the outlier counter from accumulating
        // while clusters are empty, so the multiplication shrunk
        // the cap to 420 on a `[7 temps × 3 replicas]` profile and
        // cut the loop short at iteration #420 — the operator's
        // intent was the full 1680.
        let expanded_policy = policy;
        tracker = SaturationTracker::with_policy(total.max(target), expanded_policy);

        let resume_from = state.completed_sketches.len();
        tracing::info!(
            total = total,
            cells = cells.len(),
            per_cell = per_cell,
            profile_temps = profile_temperatures.len(),
            profile_replicas = profile_replicas,
            target_override = target,
            tracker_target = tracker.target,
            tracker_hard_cap = tracker.policy.hard_cap,
            tracker_max_sketches = tracker.policy.max_sketches,
            tracker_min_sketches = tracker.policy.min_sketches,
            resume_from = resume_from,
            completed_so_far = state.completed_sketches.len(),
            "discovery: loop initialised"
        );

        // 5. Main loop: fan out every (cell, temperature, replica, sketch_index) tuple.
        let mut n: usize = state.completed_sketches.len();
        let mut stop_reason: Option<StopReason> = None;
        'outer: for cell in cells.iter() {
            for &temperature in profile_temperatures.iter() {
                for replica in 0..profile_replicas {
                    for sketch_index in 0..per_cell {
                        if cancel.is_cancelled() {
                            tracing::warn!(n = n, total = total, "discovery: cancelled mid-loop");
                            return Err(cancel.into_error().into());
                        }
                        if n >= total {
                            tracing::info!(
                                n = n,
                                total = total,
                                "discovery: loop target reached; break 'outer"
                            );
                            break 'outer;
                        }
                        let _permit = ctx.parallelism.acquire().await?;
                        let id = format!("sk_{:04}", n);
                        let user = build_user_payload(&brief_text, cell, sketch_index);

                        let cell_for_angle = cell.clone();
                        let system_for_attempt = system.clone();
                        let user_for_attempt = user.clone();
                        let id_for_attempt = id.clone();
                        let ctx_for_attempt = ctx.clone();
                        let n_for_attempt = n;

                        if n < 5 || n.is_multiple_of(100) {
                            tracing::trace!(
                                n = n,
                                total = total,
                                cell_dim = %cell.dimension_id,
                                cell_facet = %cell.facet_id,
                                temperature = temperature,
                                replica = replica,
                                sketch_index = sketch_index,
                                "discovery: iteration start"
                            );
                        }

                        // PR-D2 follow-up: 1 retry (down from 3) because MiniMax-M3
                        // deterministically produces unescaped-double-quote pathology at
                        // temperature >= 1.0 (see PR-D2 notes). 4 attempts per failed
                        // iteration was projecting onto a ~30-day test for cardinalidad 880
                        // × 21 temps × 3 replicas. The coordinator ALREADY records parse
                        // failures via `state.record_failure()` and continues, so dropping
                        // retries loses nothing on the happy path and trades ~75 % wasted
                        // LLM calls for a 4× speedup overall.
                        let sketch_result = retry_sketch_extraction(1, || {
                            let ctx = ctx_for_attempt.clone();
                            let user = user_for_attempt.clone();
                            let system = system_for_attempt.to_string();
                            let id = id_for_attempt.clone();
                            let cell = cell_for_angle.clone();
                            let attempt = n_for_attempt;
                            async move {
                                let started_unix = crate::time::now_unix_secs();
                                // PR-D1: stamp the explicit
                                // `temperature` from the active
                                // profile so the cache key in
                                // `src/llm/cache/mod.rs:117`
                                // differentiates across
                                // `(cell, temperature, replica)`
                                // tuples. Retries bypass the
                                // cache (the original
                                // `call_with_retry_parse` rule;
                                // pinned here so the coordinator
                                // mirrors it).
                                let raw = if attempt == 0 {
                                    ctx.call_with_retry_at_temp(
                                        crate::llm::Role::Sketch,
                                        system,
                                        user,
                                        0,
                                        temperature,
                                    )
                                    .await?
                                } else {
                                    ctx.call_uncached_at_temp(
                                        crate::llm::Role::Sketch,
                                        system,
                                        user,
                                        started_unix,
                                        attempt as u32,
                                        temperature,
                                    )
                                    .await?
                                };
                                let schema_hint =
                                    crate::llm::prompts::system_prompt(crate::llm::Role::Sketch)
                                        .to_owned();
                                let mut sketch: Sketch = ctx.parse_model_json(
                                    crate::llm::Role::Sketch,
                                    &raw.text,
                                    &schema_hint,
                                )?;
                                if sketch.id.is_empty() {
                                    sketch.id = id;
                                }
                                sketch.angle = format!("{}:{}", cell.dimension_id, cell.facet_id);
                                Ok::<Sketch, Error>(sketch)
                            }
                        })
                        .await;

                        match sketch_result {
                            Ok(sketch) if sketch.thesis.trim().len() >= 30 => {
                                let path = sketches_dir.join(format!("{}.json", sketch.id));
                                write_json(&path, &sketch)?;
                                state.record_completion(sketch.id.clone());
                                state.save(&run_dir)?;
                                tracker.record_completions(1);
                                tracing::debug!(
                                    sketch_id = %sketch.id,
                                    n = n,
                                    total = total,
                                    angle = %sketch.angle,
                                    thesis_len = sketch.thesis.len(),
                                    completed = tracker.completed,
                                    "discovery: sketch accepted"
                                );
                                let decision = tracker.update(&[sketch], &[]);
                                if let StopDecision::Stop { reason } = decision {
                                    tracing::warn!(
                                        n = n,
                                        total = total,
                                        completed = tracker.completed,
                                        target = tracker.target,
                                        reason = ?reason,
                                        "discovery: tracker returned Stop"
                                    );
                                    if matches!(reason, StopReason::Saturated) {
                                        TelemetryEvent::DiscoverySaturated {
                                            run_id: run_id.to_string(),
                                            coverage: tracker.coverage(),
                                            at_unix: crate::time::now_unix_secs(),
                                        }
                                        .emit();
                                    }
                                    stop_reason = Some(reason);
                                    break 'outer;
                                }
                            }
                            Ok(sketch) => {
                                tracing::warn!(
                                    sketch_id = %id,
                                    n = n,
                                    thesis_len = sketch.thesis.trim().len(),
                                    "discovery: sketch rejected (thesis too short)"
                                );
                                state.record_failure();
                                state.save(&run_dir)?;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    sketch_id = %id,
                                    n = n,
                                    error = %e,
                                    "discovery: sketch extraction failed after retries; recording failure"
                                );
                                state.record_failure();
                                state.save(&run_dir)?;
                            }
                        }

                        n += 1;
                        tokio::task::yield_now().await;
                    }
                }
            }
        }

        tracing::info!(
            completed = n,
            total = total,
            stop_reason = ?stop_reason,
            completed_in_state = state.completed_sketches.len(),
            failed = state.failed_attempts,
            "discovery: loop exit"
        );

        // 6. Clean up the persisted state file on a clean stop
        //    (either the target was reached or the tracker said
        //    `Stop`). The sketches under <run_dir>/sketches/ stay
        //    on disk so downstream phases can read them.
        state.mark_done();
        SketchLoopState::delete(&run_dir)?;

        let _ = stop_reason; // Reserved for the future post-stop telemetry surface.
        let _ = legacy;

        Ok(DiscoveryOutcome {
            run_id,
            sketches_completed: state.completed_sketches.len(),
            sketches_failed: state.failed_attempts as usize,
            legacy_used: false,
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

/// Build the user payload the LLM sees for one `(cell, sketch_index)`
/// pair. Mirrors `DiscoverMatrixPhase::user_payload` so the coordinator
/// and the flat-pipeline path emit equivalent prompts — a
/// `moagan discover` run driven by the coordinator produces the same
/// sketch text a flat-pipeline run would, which is the parity
/// guarantee PR-17 ships.
fn build_user_payload(brief: &str, cell: &MatrixCell, sketch_index: usize) -> String {
    format!(
        "{brief}\n\n\
         Use dimension=\"{dim_id}\" and facet=\"{facet_id}\" (label: \"{label}\") and \
         produce exactly one sketch (cell index {sketch_index}).",
        brief = brief,
        dim_id = cell.dimension_id,
        facet_id = cell.facet_id,
        label = cell.label,
        sketch_index = sketch_index,
    )
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

    // Track E (E8 wire-up): tests for the persona + angle picker
    // trigger rules. They all build a mock-provider-backed
    // `RunContext` so the picker helpers get a controlled
    // environment, then assert either that the provider was
    // touched (or not) or that the legacy was mutated by
    // `pick_angle`.

    /// Reusable scripted-provider harness — mirrors the helper
    /// inside `persona_angle::tests` so this module does not
    /// have to expose internals across the public boundary.
    struct ScriptedProvider {
        outcomes: parking_lot::Mutex<Vec<String>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: parking_lot::Mutex::new(responses),
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl crate::llm::Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "mock-coordinator-pickers"
        }
        fn model(&self) -> &str {
            "mock-model"
        }
        fn endpoint(&self) -> &str {
            "mock://coordinator-pickers"
        }
        async fn send(
            &self,
            _req: &crate::llm::Request,
        ) -> crate::Result<(u16, crate::llm::Response)> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = self
                .outcomes
                .lock()
                .pop()
                .expect("ScriptedProvider was drained");
            Ok((
                200,
                crate::llm::Response {
                    text,
                    finish_reason: Some("end_turn".into()),
                    truncated: false,
                    usage: Default::default(),
                },
            ))
        }
    }

    /// Build a [`RunContext`] wired to `scripted` with the supplied
    /// discovery wiring config. Kept local to the tests module so
    /// nobody outside pulls the helper into a public API surface.
    fn build_picker_ctx(
        home: MoaganHome,
        scripted: Arc<ScriptedProvider>,
        discovery: crate::config::DiscoveryWiringConfig,
    ) -> Arc<RunContext> {
        let mut registry = crate::llm::ProviderRegistry::default();
        registry.insert("mock".into(), scripted);
        let cfg = Arc::new(crate::config::Config {
            discovery,
            ..crate::config::Config::default()
        });
        Arc::new(RunContext::new_with_config(
            RunId::new(),
            Arc::new(home),
            Arc::new(registry),
            "mock".to_owned(),
            "mock-model".to_owned(),
            crate::execution::Parallelism::new(1),
            crate::telemetry::Telemetry::noop(),
            String::new(),
            "standard".to_owned(),
            cfg,
        ))
    }

    /// E8 wire-up test 1 — when the target sketch cardinality
    /// crosses the persona sweet spot (>4) AND the wiring is
    /// enabled AND a non-empty candidate list is supplied,
    /// `DiscoveryCoordinator::run_with_pickers` issues one
    /// `Role::PersonaPicker` call before the loop starts.
    #[test]
    fn coordinator_run_invokes_persona_picker_when_cardinality_high() {
        let rt = single_thread_runtime();
        let scripted = ScriptedProvider::new(vec![
            r#"{
            "selected": "skeptic",
            "rationale": "audit-first mindset catches corner cases",
            "schema_version": "persona_picker.v1"
        }"#
            .to_owned(),
        ]);
        let scripted_for_ctx = scripted.clone();
        let outcome = with_moagan_home("discovery-coordinator-persona-high", |tmp| {
            EpistemicLegacy::empty()
                .save_to(&tmp.join("epistemic_legacy.json"))
                .unwrap();
            let home = MoaganHome::at(tmp.to_path_buf());
            let coordinator_inner = DiscoveryCoordinator::new(
                home.clone(),
                RunId::new(),
                Cancel::new(),
                Brief::default(),
                "deployment-model:serverless".to_owned(),
                Mode::Standard,
            );
            let ctx = build_picker_ctx(
                home,
                scripted_for_ctx,
                crate::config::DiscoveryWiringConfig {
                    persona_enabled: true,
                    ..crate::config::DiscoveryWiringConfig::default()
                },
            );
            rt.block_on(coordinator_inner.run_with_pickers(
                Some(ctx),
                vec!["skeptic".into(), "optimist".into()],
                Vec::new(),
            ))
        })
        .expect("high-cardinality run should succeed");

        assert_eq!(
            outcome.sketches_completed,
            Cardinality::for_mode_default(Mode::Standard).soft
        );
        assert!(
            scripted.calls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "persona picker must issue at least one provider call when cardinality > 4 and enabled"
        );
    }

    /// E8 wire-up test 2 — when the wiring is disabled (default)
    /// `run_with_pickers` must not touch the provider. This is
    /// the bit-identical baseline that protects existing callers
    /// that opt out of the discovery wiring config.
    #[test]
    fn coordinator_run_skips_persona_picker_when_disabled() {
        let rt = single_thread_runtime();
        let scripted = ScriptedProvider::new(vec![]);
        let scripted_for_ctx = scripted.clone();
        let outcome = with_moagan_home("discovery-coordinator-persona-disabled", |tmp| {
            EpistemicLegacy::empty()
                .save_to(&tmp.join("epistemic_legacy.json"))
                .unwrap();
            let home = MoaganHome::at(tmp.to_path_buf());
            let coordinator_inner = DiscoveryCoordinator::new(
                home.clone(),
                RunId::new(),
                Cancel::new(),
                Brief::default(),
                "deployment-model:serverless".to_owned(),
                Mode::Standard,
            );
            let ctx = build_picker_ctx(
                home,
                scripted_for_ctx,
                crate::config::DiscoveryWiringConfig {
                    persona_enabled: false,
                    ..crate::config::DiscoveryWiringConfig::default()
                },
            );
            rt.block_on(coordinator_inner.run_with_pickers(
                Some(ctx),
                vec!["skeptic".into(), "optimist".into()],
                Vec::new(),
            ))
        })
        .expect("disabled-persona run should still succeed");

        assert_eq!(
            outcome.sketches_completed,
            Cardinality::for_mode_default(Mode::Standard).soft
        );
        assert_eq!(
            scripted.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "persona picker must short-circuit when persona_enabled=false"
        );
    }

    /// E8 wire-up test 3 — after the sketch loop completes, when
    /// the wiring is enabled AND the cluster list supplied by
    /// the integrate phase crosses `angle_clusters_min`,
    /// `run_with_pickers` invokes `Role::AnglePicker` and
    /// appends the chosen angle to the in-memory legacy.
    #[test]
    fn coordinator_run_invokes_angle_picker_after_clustering() {
        let rt = single_thread_runtime();
        let scripted = ScriptedProvider::new(vec![
            r#"{
            "problem": "auth",
            "existing_angles": ["jwt", "mtls"],
            "selected": "oauth2_pkce",
            "rationale": "delegates to a trusted IdP",
            "schema_version": "angle_picker.v1"
        }"#
            .to_owned(),
        ]);
        let scripted_for_ctx = scripted.clone();
        let outcome = with_moagan_home("discovery-coordinator-angle-after-clustering", |tmp| {
            EpistemicLegacy::empty()
                .save_to(&tmp.join("epistemic_legacy.json"))
                .unwrap();
            let home = MoaganHome::at(tmp.to_path_buf());
            let coordinator_inner = DiscoveryCoordinator::new(
                home.clone(),
                RunId::new(),
                Cancel::new(),
                Brief::default(),
                "deployment-model:serverless".to_owned(),
                Mode::Standard,
            );
            let ctx = build_picker_ctx(
                home,
                scripted_for_ctx,
                crate::config::DiscoveryWiringConfig {
                    angle_enabled: true,
                    angle_clusters_min: 1,
                    ..crate::config::DiscoveryWiringConfig::default()
                },
            );
            rt.block_on(coordinator_inner.run_with_pickers(
                Some(ctx),
                Vec::new(),
                vec!["jwt".into(), "mtls".into()],
            ))
        })
        .expect("post-clustering run should succeed");

        assert_eq!(
            outcome.sketches_completed,
            Cardinality::for_mode_default(Mode::Standard).soft
        );
        assert!(
            scripted.calls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "angle picker must issue at least one provider call when clusters.len() > angle_clusters_min"
        );
        // The angle picker appends `angle:<id>` to
        // `legacy.preferred_strategies` inside the helper itself;
        // the assertion above (provider was invoked with a valid
        // AnglePicker payload) is sufficient evidence the trigger
        // fired. The persisted mutation is independently pinned by
        // `persona_angle::tests::pick_angle_persists_to_legacy`.
    }

    /// Build a single-threaded tokio runtime for the E8 tests.
    /// `#[tokio::test]` is not used because the harness runs
    /// `with_moagan_home`'s sync closure and we cannot nest a
    /// tokio runtime inside an existing one; instead each test
    /// owns its own current-thread runtime and drives the
    /// async coordinator through `block_on`.
    fn single_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
    }

    // -----------------------------------------------------------------
    // PR-17 regression tests: the new `run_with_ctx` path actually
    // drives LLM calls and persists sketches to disk. The 16
    // pre-existing tests in this module only exercise the
    // placeholder body (state.record_completion + yield_now);
    // they would never have caught a wiring regression where the
    // coordinator looped zero times or never wrote a sketch JSON.
    // -----------------------------------------------------------------

    /// Sketch payload the mock surfaces for every `Role::Sketch`
    /// call. Mirrors the structure `DiscoverMatrixPhase::execute`
    /// uses so the coordinator's parse path matches the flat
    /// pipeline's byte-for-byte. The 35-char thesis clears the
    /// 30-char minimum-thesis gate the matrix phase applies.
    fn sketch_payload(id: &str) -> String {
        format!(
            r#"{{
              "id": "{id}",
              "thesis": "Use Rust and SQLite for a single binary backend with strong typing.",
              "key_decisions": ["single binary", "embedded sqlite"],
              "architecture_outline": "The CLI binary owns the database, the cache, and the agent registry.",
              "assumptions": ["users are comfortable with one process per run"],
              "strengths": ["simple deployment", "easy to test"],
              "weaknesses": ["no horizontal scaling"],
              "hard_constraint_check": {{"single_binary": true}},
              "expected_validation": "Build a 1k-line Rust crate that compiles in <2s.",
              "angle": "minimalist"
            }}"#
        )
    }

    /// Build a [`RunContext`] wired to the supplied scripted
    /// provider. Mirrors `build_picker_ctx` but uses the standard
    /// `mock` registry so the coordinator's LLM calls go through
    /// the production `RunContext::call_with_retry` path.
    fn build_run_ctx(home: MoaganHome, scripted: Arc<ScriptedProvider>) -> Arc<RunContext> {
        let mut registry = crate::llm::ProviderRegistry::default();
        registry.insert("mock".into(), scripted);
        let cfg = Arc::new(crate::config::Config::default());
        Arc::new(RunContext::new_with_config(
            RunId::new(),
            Arc::new(home),
            Arc::new(registry),
            "mock".to_owned(),
            "mock-model".to_owned(),
            crate::execution::Parallelism::new(1),
            crate::telemetry::Telemetry::noop(),
            String::new(),
            "standard".to_owned(),
            cfg,
        ))
    }

    /// `run_with_ctx` actually drives LLM calls and persists one
    /// `sk_<NNNN>.json` per `(cell, sketch_index)` pair. Without a
    /// real LLM call the placeholder body would either panic on the
    /// brief read or leave the sketches directory empty — this
    /// test pins the happy path so a future refactor cannot silently
    /// drop the LLM wiring.
    #[test]
    fn coordinator_run_with_ctx_produces_sketches() {
        let rt = single_thread_runtime();
        let scripted = ScriptedProvider::new(vec![
            sketch_payload("sk_0000"),
            sketch_payload("sk_0001"),
            sketch_payload("sk_0002"),
            sketch_payload("sk_0003"),
            sketch_payload("sk_0004"),
            sketch_payload("sk_0005"),
            sketch_payload("sk_0006"),
            sketch_payload("sk_0007"),
        ]);
        let scripted_for_ctx = scripted.clone();
        let outcome = with_moagan_home("discovery-coordinator-with-ctx", |tmp| {
            EpistemicLegacy::empty()
                .save_to(&tmp.join("epistemic_legacy.json"))
                .unwrap();
            let home = MoaganHome::at(tmp.to_path_buf());
            let run_id = RunId::new();
            let run_dir = home.run_dir(run_id);
            run_dir.ensure().unwrap();
            let brief = serde_json::json!({
                "problem": "Design a multi-tenant SaaS backend",
                "objectives": ["Auth", "Storage"],
                "constraints": ["Rust single binary"],
                "non_goals": [],
                "open_questions": [],
                "raw_prompt": "Design a multi-tenant SaaS backend"
            });
            std::fs::write(run_dir.brief(), serde_json::to_vec_pretty(&brief).unwrap()).unwrap();

            let coordinator_inner = DiscoveryCoordinator::new(
                home.clone(),
                run_id,
                Cancel::new(),
                Brief::default(),
                "deployment-model:serverless".to_owned(),
                Mode::Standard,
            );
            let ctx = build_run_ctx(home.clone(), scripted_for_ctx);
            rt.block_on(coordinator_inner.run_with_ctx(ctx))
        })
        .expect("run_with_ctx should succeed with mock provider");

        let target = Cardinality::for_mode_default(Mode::Standard).soft;
        // `ExplorationMatrix::default_for(target)` produces
        // `4 dims × 2 facets × max(target/8, 1) = 8` sketches; the
        // mode-derived soft target (7) is a lower bound the matrix
        // rounds up to fill its 8 cells. The coordinator's actual
        // fan-out is the matrix's cardinality (8), not the soft
        // target (7), so the assertion uses the matrix-driven value.
        let matrix_card =
            crate::discovery::matrix::ExplorationMatrix::default_for(target).cardinality();
        assert_eq!(
            outcome.sketches_completed, matrix_card,
            "matrix cardinality must be reached end-to-end (target={target}, matrix={matrix_card})"
        );
        assert!(
            scripted.calls.load(std::sync::atomic::Ordering::SeqCst) >= matrix_card,
            "coordinator must issue one LLM call per (cell, sketch_index) pair"
        );
    }

    /// `run_with_ctx` persists each successful sketch as
    /// `<run_dir>/sketches/sk_<NNNN>.json`. The audit fix-list
    /// requires this to be the canonical location because the
    /// downstream `discover_tag`, `discover_cluster`, and
    /// `discover_facet` phases all read from that directory.
    #[test]
    fn coordinator_run_with_ctx_persists_sketches_to_disk() {
        let rt = single_thread_runtime();
        let scripted = ScriptedProvider::new(vec![
            sketch_payload("sk_0000"),
            sketch_payload("sk_0001"),
            sketch_payload("sk_0002"),
            sketch_payload("sk_0003"),
            sketch_payload("sk_0004"),
            sketch_payload("sk_0005"),
            sketch_payload("sk_0006"),
            sketch_payload("sk_0007"),
        ]);
        let scripted_for_ctx = scripted.clone();
        with_moagan_home("discovery-coordinator-with-ctx-persists", |tmp| {
            EpistemicLegacy::empty()
                .save_to(&tmp.join("epistemic_legacy.json"))
                .unwrap();
            let home = MoaganHome::at(tmp.to_path_buf());
            let run_id = RunId::new();
            let run_dir = home.run_dir(run_id);
            run_dir.ensure().unwrap();
            let brief = serde_json::json!({
                "problem": "Design a multi-tenant SaaS backend",
                "objectives": ["Auth", "Storage"],
                "constraints": ["Rust single binary"],
                "non_goals": [],
                "open_questions": [],
                "raw_prompt": "Design a multi-tenant SaaS backend"
            });
            std::fs::write(run_dir.brief(), serde_json::to_vec_pretty(&brief).unwrap()).unwrap();

            let coordinator_inner = DiscoveryCoordinator::new(
                home.clone(),
                run_id,
                Cancel::new(),
                Brief::default(),
                "deployment-model:serverless".to_owned(),
                Mode::Standard,
            );
            let ctx = build_run_ctx(home.clone(), scripted_for_ctx);
            let outcome = rt.block_on(coordinator_inner.run_with_ctx(ctx))?;
            assert!(outcome.sketches_completed > 0);

            let sketches_dir = run_dir.sketches();
            let mut entries: Vec<_> = std::fs::read_dir(&sketches_dir)
                .unwrap()
                .filter_map(|r| r.ok())
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
                .collect();
            assert!(
                !entries.is_empty(),
                "coordinator must persist at least one sketch to disk; looked in {}",
                sketches_dir.display()
            );
            entries.sort_by_key(|e| e.file_name());
            let first_name = entries[0].file_name().into_string().unwrap();
            assert!(
                first_name.starts_with("sk_") && first_name.ends_with(".json"),
                "sketch filenames must follow the sk_<NNNN>.json convention; got {first_name}"
            );
            Ok::<_, CoordinatorError>(())
        })
        .expect("run_with_ctx should succeed");
    }

    /// `run_with_ctx` cleans up the persisted `<run_dir>/.discovery_state.json`
    /// on a clean exit so the next run starts fresh (same contract the
    /// placeholder `run` enforces). Without this guard a stale state
    /// file could trick a follow-up run into resuming from a phantom
    /// baseline.
    #[test]
    fn coordinator_run_with_ctx_cleans_up_state_file() {
        let rt = single_thread_runtime();
        let scripted = ScriptedProvider::new(vec![
            sketch_payload("sk_0000"),
            sketch_payload("sk_0001"),
            sketch_payload("sk_0002"),
            sketch_payload("sk_0003"),
            sketch_payload("sk_0004"),
            sketch_payload("sk_0005"),
            sketch_payload("sk_0006"),
            sketch_payload("sk_0007"),
        ]);
        let scripted_for_ctx = scripted.clone();
        with_moagan_home("discovery-coordinator-with-ctx-cleanup", |tmp| {
            EpistemicLegacy::empty()
                .save_to(&tmp.join("epistemic_legacy.json"))
                .unwrap();
            let home = MoaganHome::at(tmp.to_path_buf());
            let run_id = RunId::new();
            let run_dir = home.run_dir(run_id);
            run_dir.ensure().unwrap();
            let brief = serde_json::json!({
                "problem": "Design a multi-tenant SaaS backend",
                "objectives": ["Auth", "Storage"],
                "constraints": ["Rust single binary"],
                "non_goals": [],
                "open_questions": [],
                "raw_prompt": "Design a multi-tenant SaaS backend"
            });
            std::fs::write(run_dir.brief(), serde_json::to_vec_pretty(&brief).unwrap()).unwrap();

            let coordinator_inner = DiscoveryCoordinator::new(
                home.clone(),
                run_id,
                Cancel::new(),
                Brief::default(),
                "deployment-model:serverless".to_owned(),
                Mode::Standard,
            );
            let ctx = build_run_ctx(home.clone(), scripted_for_ctx);
            rt.block_on(coordinator_inner.run_with_ctx(ctx))
                .expect("run_with_ctx should succeed");

            let state_path = home.runs_dir().join(run_id.to_string()).join(STATE_FILE);
            assert!(
                !state_path.exists(),
                "completed run_with_ctx must delete the persisted state file"
            );
        });
    }

    /// Test helper (PR-D1): a provider that captures the resolved
    /// sampling temperature from every `Request` it receives so
    /// the per-provider temperature profile tests can assert on
    /// each call's temperature (the v0.5 `ScriptedProvider` only
    /// counts calls — it doesn't record the resolved wire
    /// parameters). The responses are scripted so the coordinator
    /// can complete the matrix fan-out deterministically.
    struct TemperatureRecordingProvider {
        outcomes: parking_lot::Mutex<Vec<String>>,
        calls: std::sync::atomic::AtomicUsize,
        temperatures: parking_lot::Mutex<Vec<f32>>,
    }

    impl TemperatureRecordingProvider {
        fn new(responses: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: parking_lot::Mutex::new(responses),
                calls: std::sync::atomic::AtomicUsize::new(0),
                temperatures: parking_lot::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl crate::llm::Provider for TemperatureRecordingProvider {
        fn name(&self) -> &str {
            "mock-coordinator-temperature-recorder"
        }
        fn model(&self) -> &str {
            "mock-model"
        }
        fn endpoint(&self) -> &str {
            "mock://coordinator-temperature-recorder"
        }
        async fn send(
            &self,
            req: &crate::llm::Request,
        ) -> crate::Result<(u16, crate::llm::Response)> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(t) = req.temperature {
                self.temperatures.lock().push(t);
            }
            let text = self
                .outcomes
                .lock()
                .pop()
                .expect("TemperatureRecordingProvider was drained");
            Ok((
                200,
                crate::llm::Response {
                    text,
                    finish_reason: Some("end_turn".into()),
                    truncated: false,
                    usage: Default::default(),
                },
            ))
        }
    }

    /// PR-D1: when the operator does NOT set any
    /// `--temperature-profile` flags and the persisted
    /// `[discovery_matrix]` block is empty, the coordinator must
    /// spawn exactly the same number of LLM calls as the v0.5
    /// fan-out (the audit's "bit-identical default" promise).
    /// Pin the default profile's `total() == 1` property so a
    /// future refactor cannot silently inflate the loop.
    #[test]
    fn phase_continue_does_not_loop_when_profile_is_default() {
        let rt = single_thread_runtime();
        // 4 dims × 2 facets × 1 sketch per cell = 8 sketches
        // (the matrix's default cardinality at the
        // `Cardinality::for_mode_default(Mode::Standard).soft = 7`
        // floor rounds up to fill the 8 cells).
        let target = Cardinality::for_mode_default(Mode::Standard).soft;
        let matrix_card =
            crate::discovery::matrix::ExplorationMatrix::default_for(target).cardinality();
        let scripted = TemperatureRecordingProvider::new(
            (0..matrix_card)
                .map(|i| sketch_payload(&format!("sk_{i:04}")))
                .collect(),
        );
        let scripted_for_ctx = scripted.clone();
        with_moagan_home("discovery-coordinator-default-profile", |tmp| {
            EpistemicLegacy::empty()
                .save_to(&tmp.join("epistemic_legacy.json"))
                .unwrap();
            let home = MoaganHome::at(tmp.to_path_buf());
            let run_id = RunId::new();
            let run_dir = home.run_dir(run_id);
            run_dir.ensure().unwrap();
            let brief = serde_json::json!({
                "problem": "Design a multi-tenant SaaS backend",
                "objectives": ["Auth"],
                "constraints": ["Rust single binary"],
                "non_goals": [],
                "open_questions": [],
                "raw_prompt": "Design a multi-tenant SaaS backend"
            });
            std::fs::write(run_dir.brief(), serde_json::to_vec_pretty(&brief).unwrap()).unwrap();

            let coordinator_inner = DiscoveryCoordinator::new(
                home.clone(),
                run_id,
                Cancel::new(),
                Brief::default(),
                "deployment-model:serverless".to_owned(),
                Mode::Standard,
            );
            // The default Config has an empty `discovery_matrix`;
            // the matrix picks up `[1.0] × 1` from
            // `TemperatureProfile::default()`.
            let mut registry = crate::llm::ProviderRegistry::default();
            registry.insert("mock".into(), scripted_for_ctx);
            let cfg = Arc::new(crate::config::Config::default());
            let ctx = Arc::new(RunContext::new_with_config(
                run_id,
                Arc::new(home.clone()),
                Arc::new(registry),
                "mock".to_owned(),
                "mock-model".to_owned(),
                crate::execution::Parallelism::new(1),
                crate::telemetry::Telemetry::noop(),
                String::new(),
                "standard".to_owned(),
                cfg,
            ));
            rt.block_on(coordinator_inner.run_with_ctx(ctx))
                .expect("run_with_ctx should succeed with default profile");
        });
        let calls = scripted.calls.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            calls, matrix_card,
            "default profile ([1.0] × 1) must produce exactly the v0.5 sketch count \
             (matrix cardinality = {matrix_card}); got {calls}"
        );
        let recorded = scripted.temperatures.lock().clone();
        // The default profile's temperatures list is `[1.0]` so
        // every recorded call must carry `1.0`. Any drift here is
        // a regression on the "bit-identical default" promise.
        assert!(
            recorded.iter().all(|&t| (t - 1.0).abs() < f32::EPSILON),
            "every recorded temperature must be 1.0 (the default profile); got {recorded:?}"
        );
    }

    /// PR-D1: when the operator sets a per-provider temperature
    /// profile, the coordinator fans out the matrix across the
    /// `(cell, temperature, replica)` Cartesian product. The
    /// test asserts:
    ///
    /// * The total number of LLM calls equals `cells × per_cell ×
    ///   Σ(providers: |temperatures| × replicas)`.
    /// * The recorded temperatures match the expected per-provider
    ///   profile (the `mock-model` profile has 2 temperatures × 2
    ///   replicas; the `other-model` profile has 1 temperature ×
    ///   1 replica — but it never fires because the active
    ///   provider's model is `mock-model`).
    /// * The mock provider's model name (`"mock-model"`) is used
    ///   as the lookup key; a typo in the profile map key would
    ///   fall back to `default_profile` and the test would fail.
    #[test]
    fn phase_continue_iterates_per_provider_and_per_replica() {
        let rt = single_thread_runtime();
        // Build the profile map the coordinator will read.
        // The active provider's model name (`mock-model`) keys
        // into this map.
        let mut profiles = std::collections::HashMap::new();
        profiles.insert(
            "mock-model".to_owned(),
            crate::discovery::matrix::TemperatureProfile {
                temperatures: vec![0.0, 0.5],
                replicas_per_temperature: 2,
            },
        );
        profiles.insert(
            "other-model".to_owned(),
            crate::discovery::matrix::TemperatureProfile {
                temperatures: vec![0.7],
                replicas_per_temperature: 1,
            },
        );
        let default_profile = crate::discovery::matrix::TemperatureProfile {
            temperatures: vec![0.99],
            replicas_per_temperature: 1,
        };
        // The coordinator uses `default_for_with_profiles(target)`
        // internally (target = `Cardinality::for_mode_default(Standard).soft = 7`).
        // For target=7 the default matrix has 4 dims × 2 facets
        // = 8 cells × max(7/8, 1) per_cell = 1 sketch per cell.
        // Cardinality = 8. With the active provider's profile
        // (`[0.0, 0.5] × 2 = 4`) the total fan-out is 8 × 4 = 32.
        let target = Cardinality::for_mode_default(Mode::Standard).soft;
        let matrix = crate::discovery::matrix::ExplorationMatrix::default_for_with_profiles(
            target,
            profiles.clone(),
            default_profile,
        );
        let expected_calls =
            matrix.cells() * matrix.sketches_per_cell * matrix.profile_for("mock-model").total();
        let scripted = TemperatureRecordingProvider::new(
            (0..expected_calls)
                .map(|i| sketch_payload(&format!("sk_{i:04}")))
                .collect(),
        );
        let scripted_for_ctx = scripted.clone();
        with_moagan_home("discovery-coordinator-iterates-per-provider", |tmp| {
            EpistemicLegacy::empty()
                .save_to(&tmp.join("epistemic_legacy.json"))
                .unwrap();
            let home = MoaganHome::at(tmp.to_path_buf());
            let run_id = RunId::new();
            let run_dir = home.run_dir(run_id);
            run_dir.ensure().unwrap();
            let brief = serde_json::json!({
                "problem": "Design a multi-tenant SaaS backend",
                "objectives": ["Auth"],
                "constraints": ["Rust single binary"],
                "non_goals": [],
                "open_questions": [],
                "raw_prompt": "Design a multi-tenant SaaS backend"
            });
            std::fs::write(run_dir.brief(), serde_json::to_vec_pretty(&brief).unwrap()).unwrap();

            let coordinator_inner = DiscoveryCoordinator::new(
                home.clone(),
                run_id,
                Cancel::new(),
                Brief::default(),
                "deployment-model:serverless".to_owned(),
                Mode::Standard,
            );
            let mut registry = crate::llm::ProviderRegistry::default();
            registry.insert("mock".into(), scripted_for_ctx);
            // Build the effective Config with the explicit profile
            // map; the coordinator reads it via
            // `ctx.config.discovery_matrix.temperature_profiles`.
            let cfg = crate::config::Config {
                discovery_matrix: crate::config::DiscoveryMatrixConfig {
                    temperature_profiles: matrix.temperature_profiles.clone(),
                    default_profile: Some(matrix.default_profile.clone()),
                },
                ..crate::config::Config::default()
            };
            let ctx = Arc::new(RunContext::new_with_config(
                run_id,
                Arc::new(home.clone()),
                Arc::new(registry),
                "mock".to_owned(),
                "mock-model".to_owned(),
                crate::execution::Parallelism::new(1),
                crate::telemetry::Telemetry::noop(),
                String::new(),
                "standard".to_owned(),
                Arc::new(cfg),
            ));
            rt.block_on(coordinator_inner.run_with_ctx(ctx))
                .expect("run_with_ctx should succeed with explicit profile");
        });
        let calls = scripted.calls.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            calls, expected_calls,
            "explicit profile must drive a fan-out of cells × per_cell × (temperatures × replicas); \
             expected {expected_calls}, got {calls}"
        );
        let recorded = scripted.temperatures.lock().clone();
        assert_eq!(
            recorded.len(),
            expected_calls,
            "every call must record its temperature; expected {expected_calls} entries, got {}",
            recorded.len()
        );
        // The active provider's model is `mock-model`, so the
        // recorded temperatures must come from the `mock-model`
        // profile (`[0.0, 0.5] × 2`). No call may carry the
        // `other-model` profile's `0.7` or the default profile's
        // `0.99` — a typo in the lookup key would silently fall
        // back to the default and trip this assertion.
        for (i, &t) in recorded.iter().enumerate() {
            assert!(
                (t - 0.0).abs() < f32::EPSILON || (t - 0.5).abs() < f32::EPSILON,
                "call #{i} carried unexpected temperature {t}; the active provider's \
                 profile must be `[0.0, 0.5] × 2`"
            );
        }
    }
}
