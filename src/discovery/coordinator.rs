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
        tracing::debug!(
            run_id = %run_id,
            current_strategy = %current_strategy,
            mode = ?mode,
            "DiscoveryCoordinator::new"
        );
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
        tracing::debug!(
            run_id = %self.run_id,
            candidates = candidates.len(),
            clusters = clusters.len(),
            mode = ?self.mode,
            "DiscoveryCoordinator::run_with_pickers (async)"
        );
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
        tracing::debug!(
            run_id = %run_id,
            strategy = %strategy,
            target,
            "run_with_pickers: loop params resolved"
        );

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
    /// When `sketches_per_cell_override` is `Some(n)`, the matrix
    /// is sized from `n` instead of
    /// `ctx.config.discovery_matrix.sketches_per_cell`. The CLI uses
    /// this to honour `--sketches-per-cell` directly. F2 (Track
    /// G.2) removed the v0.5 `Cardinality::for_mode_default(mode)`
    /// derivation from the discovery path; the coordinator now
    /// owns the per-cell fan-out, not the mode.
    ///
    /// # Flow
    ///
    /// 1. Build the [`ExplorationMatrix`] from the operator's
    ///    `sketches_per_cell` (CLI flag → config). The matrix is
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
    ///    `~60 sketches at 50% saturation` contract. The tracker's
    ///    target is `matrix.cardinality()` (cells ×
    ///    sketches_per_cell × profile_total) — the F2 contract.
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
    /// accepts an explicit `sketches_per_cell` override. The CLI
    /// uses this to honour `--sketches-per-cell` directly; tests
    /// and callers that want the config-derived default should
    /// stick with [`DiscoveryCoordinator::run_with_ctx`].
    ///
    /// F2 (Track G.2) renamed the previous `target_override`
    /// parameter from "cardinality" to "sketches_per_cell" because
    /// the discovery matrix no longer derives its fan-out from the
    /// linear pipeline's `Cardinality::for_mode_default(mode).soft`.
    /// The override is the per-cell count the operator picked; the
    /// loop target becomes `matrix.cardinality()` (= cells ×
    /// sketches_per_cell × profile_total) below.
    pub async fn run_with_ctx_and_target(
        self,
        ctx: Arc<RunContext>,
        sketches_per_cell_override: Option<usize>,
    ) -> Result<DiscoveryOutcome, CoordinatorError> {
        tracing::debug!(
            run_id = %self.run_id,
            sketches_per_cell_override = ?sketches_per_cell_override,
            default_model = %ctx.default_model,
            default_provider = %ctx.default_provider,
            "DiscoveryCoordinator::run_with_ctx_and_target (async)"
        );
        let DiscoveryCoordinator {
            home,
            run_id,
            cancel,
            brief: _,
            mut legacy,
            state,
            mode: _,
        } = self;

        let run_dir = {
            let handle = home.run_dir(run_id);
            handle.ensure()?;
            handle.root().to_path_buf()
        };
        let strategy = state.current_strategy.clone();
        // F2 (Track G.2): the discovery matrix's per-cell fan-out
        // comes from the CLI flag (via `opts.sketches_per_cell` →
        // `effective_cfg.discovery_matrix.sketches_per_cell`), the
        // `MOAGAN_DISCOVERY_SKETCHES_PER_CELL` env var, the
        // `[discovery_matrix].sketches_per_cell` TOML block, or
        // the default `10`. The CLI override wins. The previous
        // v0.5 mode-derived `Cardinality::for_mode_default(mode).soft`
        // is gone — the discovery path never derived its fan-out
        // from a linear-pipeline mode.
        let config_sketches_per_cell = ctx.config.discovery_matrix.sketches_per_cell;
        let sketches_per_cell = sketches_per_cell_override.unwrap_or(config_sketches_per_cell);

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
        //
        // F1 (Track G.2 `discover_dimensions`): the coordinator
        // builds its `ExplorationMatrix` from one of three sources,
        // first match wins:
        //
        // 1. `<run_dir>/discovery_dimensions.json` sidecar
        //    (populated by the `discover_dimensions` phase or a
        //    prior run with `--matrix-spec` / `--llm-derive`).
        //    The matrix inherits those dimensions verbatim so a
        //    resumed run never re-derives.
        // 2. The operator's `--matrix-spec` flag (carried on
        //    `ctx.config.discovery_matrix.matrix_spec`). The spec
        //    is parsed and promoted to a matrix.
        // 3. The legacy `--dimensions N --facets-per-dimension M`
        //    pair (carried on the same config block). The matrix
        //    is built with `dim-NN` placeholders so the legacy
        //    CLI still works during the F1 transition window.
        //
        // F2 (Track G.2): `sketches_per_cell` is now the operator's
        // explicit per-cell knob — no integer division between
        // `--cardinality` and `cells()`. The matrix's
        // `cardinality()` is `cells() × sketches_per_cell` and
        // drives the loop target below.
        let temperature_profiles = ctx.config.discovery_matrix.temperature_profiles.clone();
        let default_profile = ctx
            .config
            .discovery_matrix
            .default_profile
            .clone()
            .unwrap_or_default();
        let matrix =
            build_coordinator_matrix(&run_dir, sketches_per_cell, &ctx.config.discovery_matrix)?;
        let mut matrix = matrix;
        matrix.temperature_profiles = temperature_profiles;
        matrix.default_profile = default_profile;
        // PR-7: rewrite every per-provider temperature profile
        // against the auto-discovered supported set. The runtime
        // gate in `RunContext::dispatch_to_provider` is the safety
        // net for per-role defaults and direct callers; this
        // boundary rewriter reshapes the operator's matrix profile
        // so the per-cell fan-out and the cache-key cardinality
        // reflect the post-clamp reality (a `0.7` that gets clamped
        // to `0.5` no longer counts as a distinct cell from the
        // explicit `0.5` in the same profile).
        if let Some(table) = ctx.temperature_table.as_ref() {
            let mut supported_sets: std::collections::HashMap<String, Vec<f32>> =
                std::collections::HashMap::new();
            for model in matrix.temperature_profiles.keys() {
                let set = table.supported_for(&ctx.default_provider, model);
                if !set.is_empty() {
                    supported_sets.insert(model.clone(), set);
                }
            }
            let events = matrix.rewrite_temperatures_to_supported(&supported_sets);
            for e in events {
                tracing::warn!(
                    provider_model = %e.provider_model,
                    n_clamped = e.n_clamped,
                    requested = ?e.requested,
                    clamped_to = ?e.clamped_to,
                    "temperature profile rewritten to nearest supported values"
                );
            }
        }
        // F2: the loop target is the matrix's cardinality (cells ×
        // sketches_per_cell × profile_total). This replaces the v0.5
        // `Cardinality::for_mode_default(mode).soft` derivation
        // because the discovery path never derived its fan-out
        // from a linear-pipeline mode.
        let target = matrix.cardinality();
        let matrix_path = run_dir.join("exploration_matrix.json");
        write_json(&matrix_path, &matrix)?;
        tracing::info!(
            cells = matrix.cells(),
            per_cell = matrix.sketches_per_cell,
            sketches_per_cell,
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
            if ctx.config.discovery.persona_enabled
                && !auto_candidates.is_empty()
                && sketches_per_cell * matrix.cells().max(1) > 4
            {
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
        let state = match SketchLoopState::load(&run_dir)? {
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
        // `cells × sketches_per_cell` (v0.5). F2: `target` is now
        // `matrix.cardinality()` (= cells × sketches_per_cell) so
        // `target.max(matrix.cardinality()) == matrix.cardinality()`
        // — the saturation tracker anchors to the matrix fan-out
        // the operator picked.
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
            sketches_per_cell,
            target = target,
            matrix_cardinality = matrix.cardinality(),
            tracker_target = tracker.target,
            tracker_hard_cap = tracker.policy.hard_cap,
            tracker_max_sketches = tracker.policy.max_sketches,
            tracker_min_sketches = tracker.policy.min_sketches,
            resume_from = resume_from,
            completed_so_far = state.completed_sketches.len(),
            "discovery: loop initialised"
        );

        // 5. Main loop: fan out every (cell, temperature, replica, sketch_index) tuple.
        //
        // PR-2 (perf/discovery-parallelism): the loop is now parallel. Each iteration
        // is spawned as a `tokio::task` (recorded in a `JoinSet` per AGENTS.md) and
        // the `ctx.parallelism` semaphore limits the number of concurrent LLM calls.
        // The previous implementation was sequential because the loop awaited each
        // call in-place — the semaphore was acquired/released per iteration but
        // only one permit was ever in flight. Throughput was ~3.3 sketches/min on
        // every run regardless of `--max-parallelism` (verified against run7
        // 8h 1619 sketches and run8 5h 40m 1348 sketches). Now the throughput
        // honours the operator's chosen parallelism.
        //
        // Shared state is wrapped in `Arc<std::sync::Mutex<>>` so concurrent tasks
        // can mutate `state` and `tracker`. The mutex is held briefly (just for
        // the mutation + `save`) and the file IO completes before the next spawn
        // — there is no observed contention in practice because the LLM round-trip
        // (15–18 s) dwarfs the lock window.
        //
        // Stop conditions:
        // - `total` reached (`completed + failed >= total`)
        // - `tracker` returned `Stop` (any task may set this)
        // - `cancel` token tripped
        // The outer loop polls these between spawns. In-flight tasks complete
        // whatever they have in flight before the `join_set` drains.
        let shared_state = Arc::new(std::sync::Mutex::new(state));
        let shared_tracker = Arc::new(std::sync::Mutex::new(tracker));
        let shared_stop_reason: Arc<std::sync::Mutex<Option<StopReason>>> =
            Arc::new(std::sync::Mutex::new(None));
        let id_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut join_set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        let mut n: usize = 0;
        'outer: for cell in cells.iter() {
            for &temperature in profile_temperatures.iter() {
                for replica in 0..profile_replicas {
                    for sketch_index in 0..per_cell {
                        if cancel.is_cancelled() {
                            tracing::warn!(n = n, total = total, "discovery: cancelled mid-loop");
                            break 'outer;
                        }
                        // Stop condition: target reached OR the tracker has declared Stop.
                        {
                            let s = shared_state.lock().expect("state poisoned");
                            let completed = s.completed_sketches.len();
                            let failed = s.failed_attempts as usize;
                            drop(s);
                            if completed + failed >= total {
                                tracing::info!(
                                    n = n,
                                    total = total,
                                    "discovery: loop target reached; break 'outer"
                                );
                                break 'outer;
                            }
                            if shared_stop_reason
                                .lock()
                                .expect("stop_reason poisoned")
                                .is_some()
                            {
                                break 'outer;
                            }
                        }

                        let id = format!(
                            "sk_{:04}",
                            id_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        );
                        let user = build_user_payload(&brief_text, cell, sketch_index);

                        let cell_for_angle = cell.clone();
                        let system_for_attempt = system.clone();
                        let user_for_attempt = user.clone();
                        let id_for_attempt = id.clone();
                        let ctx_for_attempt = ctx.clone();
                        let n_for_attempt = n;
                        let state_for_task = Arc::clone(&shared_state);
                        let tracker_for_task = Arc::clone(&shared_tracker);
                        let stop_reason_for_task = Arc::clone(&shared_stop_reason);
                        let sketches_dir_for_task = sketches_dir.clone();
                        let run_dir_for_task = run_dir.clone();
                        let cancel_for_task = cancel.clone();

                        if n < 5 || n.is_multiple_of(100) {
                            // PR-7 (operator-visibility): the field is renamed
                            // from `temperature` to `temperature_profile` so the
                            // operator can tell at a glance that the value
                            // shown here is the post-rewrite matrix profile
                            // temperature, not necessarily the value the
                            // runtime ends up sending. The actual sent value
                            // is logged separately at dispatch time by
                            // `RunContext::dispatch_to_provider` (look for
                            // `requested` / `clamped_to` /
                            // `temperature in supported set; no clamp`).
                            tracing::trace!(
                                n = n,
                                total = total,
                                cell_dim = %cell.dimension_id,
                                cell_facet = %cell.facet_id,
                                temperature_profile = temperature,
                                replica = replica,
                                sketch_index = sketch_index,
                                "discovery: iteration start"
                            );
                        }

                        // PR-D2 follow-up: 2 retries (up from 1, down from the
                        // original 3) because run8 on 2026-08-19 had a 4.2 % sketch
                        // rejection rate vs 1.6 % on run7. Verified bucket: 45 of
                        // 57 rejections were JSON parse failures (trailing comma,
                        // schema mismatch) caused by the temperature 1.0+ pathology
                        // on MiniMax-M3. Two retries (3 attempts) recover the
                        // majority of those failures without re-introducing the
                        // 30-day cardinalidad 880 projection that motivated the
                        // drop from 3 to 1 in the first place — the dominant
                        // retry cost is the 15–18 s LLM round-trip, and 2 retries
                        // add at most 36 s per failing iteration, which is bounded
                        // by the rest of the test runtime.
                        //
                        // PR-2: the entire iteration runs inside the spawned task. The
                        // task acquires a parallelism permit (semaphore.acquire) BEFORE
                        // the LLM call so the in-flight count is bounded; the permit is
                        // released when the permit guard drops at the end of the task.
                        join_set.spawn(async move {
                            // Cancellation honor: bail before burning API budget.
                            if cancel_for_task.is_cancelled() {
                                return;
                            }
                            let _permit = match ctx_for_attempt.parallelism.acquire().await {
                                Ok(p) => p,
                                Err(_) => return,
                            };

                            let sketch_result = retry_sketch_extraction(10, || {
                                let ctx = ctx_for_attempt.clone();
                                let user = user_for_attempt.clone();
                                let system = system_for_attempt.to_string();
                                let id = id_for_attempt.clone();
                                let cell = cell_for_angle.clone();
                                let attempt = n_for_attempt;
                                async move {
                                    let started_unix = crate::time::now_unix_secs();
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
                                    let schema_hint = crate::llm::prompts::system_prompt(
                                        crate::llm::Role::Sketch,
                                    )
                                    .to_owned();
                                    let mut sketch: Sketch = ctx.parse_model_json(
                                        crate::llm::Role::Sketch,
                                        &raw.text,
                                        &schema_hint,
                                    )?;
                                    if sketch.id.is_empty() {
                                        sketch.id = id;
                                    }
                                    sketch.angle =
                                        format!("{}:{}", cell.dimension_id, cell.facet_id);
                                    Ok::<Sketch, Error>(sketch)
                                }
                            })
                            .await;

                            match sketch_result {
                                Ok(sketch) if sketch.thesis.trim().len() >= 30 => {
                                    let path = sketches_dir_for_task
                                        .join(format!("{}.json", sketch.id));
                                    let _ = write_json(&path, &sketch);
                                    let decision = {
                                        let mut s = state_for_task.lock().expect("state poisoned");
                                        s.record_completion(sketch.id.clone());
                                        let _ = s.save(&run_dir_for_task);
                                        drop(s);
                                        let mut t = tracker_for_task.lock().expect("tracker poisoned");
                                        t.record_completions(1);
                                        let t_completed = t.completed;
                                        let t_target = t.target;
                                        let decision = t.update(std::slice::from_ref(&sketch), &[]);
                                        let coverage = t.coverage();
                                        tracing::debug!(
                                            sketch_id = %sketch.id,
                                            n = n_for_attempt,
                                            total = total,
                                            angle = %sketch.angle,
                                            cell_dim = %cell_for_angle.dimension_id,
                                            cell_facet = %cell_for_angle.facet_id,
                                            temperature_profile = temperature,
                                            replica = replica,
                                            sketch_index = sketch_index,
                                            thesis_len = sketch.thesis.len(),
                                            completed = t_completed,
                                            "discovery: sketch accepted"
                                        );
                                        if let StopDecision::Stop { reason } = &decision {
                                            tracing::warn!(
                                                n = n_for_attempt,
                                                total = total,
                                                completed = t_completed,
                                                target = t_target,
                                                reason = ?reason,
                                                "discovery: tracker returned Stop"
                                            );
                                            if matches!(reason, StopReason::Saturated) {
                                                TelemetryEvent::DiscoverySaturated {
                                                    run_id: run_id.to_string(),
                                                    coverage,
                                                    at_unix: crate::time::now_unix_secs(),
                                                }
                                                .emit();
                                            }
                                        }
                                        decision
                                    };
                                    if let StopDecision::Stop { reason } = decision {
                                        *stop_reason_for_task
                                            .lock()
                                            .expect("stop_reason poisoned") = Some(reason);
                                    }
                                }
                                Ok(sketch) => {
                                    tracing::warn!(
                                        sketch_id = %id,
                                        n = n_for_attempt,
                                        thesis_len = sketch.thesis.trim().len(),
                                        "discovery: sketch rejected (thesis too short)"
                                    );
                                    let mut s = state_for_task.lock().expect("state poisoned");
                                    s.record_failure();
                                    let _ = s.save(&run_dir_for_task);
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        sketch_id = %id,
                                        n = n_for_attempt,
                                        error = %e,
                                        "discovery: sketch extraction failed after retries; recording failure"
                                    );
                                    let mut s = state_for_task.lock().expect("state poisoned");
                                    s.record_failure();
                                    let _ = s.save(&run_dir_for_task);
                                }
                            }
                        });

                        n += 1;
                        tokio::task::yield_now().await;
                    }
                }
            }
        }

        // Drain in-flight tasks. Cancellation cooperates: each task
        // checks `cancel_for_task.is_cancelled()` before the LLM call
        // and bails, so a SIGTERM mid-loop returns within ~one LLM
        // round-trip.
        while join_set.join_next().await.is_some() {}

        // Snapshot final state for the trace + outcome under the
        // mutex and release the lock before the cleanup and the
        // outcome construction.
        let (final_completed, final_failed, final_stop_reason) = {
            let s = shared_state.lock().expect("state poisoned");
            let completed = s.completed_sketches.len();
            let failed = s.failed_attempts as usize;
            drop(s);
            let stop = shared_stop_reason
                .lock()
                .expect("stop_reason poisoned")
                .clone();
            (completed, failed, stop)
        };

        tracing::info!(
            completed = final_completed,
            total = total,
            stop_reason = ?final_stop_reason,
            completed_in_state = final_completed,
            failed = final_failed,
            "discovery: loop exit"
        );

        // 6. Clean up the persisted state file on a clean stop
        //    (either the target was reached or the tracker said
        //    `Stop`). The sketches under <run_dir>/sketches/ stay
        //    on disk so downstream phases can read them.
        {
            let mut s = shared_state.lock().expect("state poisoned");
            s.mark_done();
            let _ = s.save(&run_dir);
        }
        SketchLoopState::delete(&run_dir)?;

        let _ = legacy;

        Ok(DiscoveryOutcome {
            run_id,
            sketches_completed: final_completed,
            sketches_failed: final_failed,
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

/// Build the matrix the coordinator will fan out against. F1
/// (Track G.2) sources the matrix from three places, first match
/// wins:
///
/// 1. `<run_dir>/discovery_dimensions.json` sidecar — the
///    dimensions the `discover_dimensions` phase derived (or
///    that a prior run persisted via `--matrix-spec`). A resumed
///    run picks up the cached dimensions verbatim.
/// 2. `ctx.config.discovery_matrix.matrix_spec` — the operator's
///    `--matrix-spec` flag(s), persisted via the
///    `[discovery_matrix]` block. The coordinator parses the spec
///    the same way the CLI does and promotes it to a matrix.
/// 3. The legacy `--dimensions N --facets-per-dimension M` pair
///    carried on `ctx.config.discovery_matrix.dimensions` /
///    `facets_per_dimension`. The matrix uses `dim-NN` placeholders
///    so legacy CLI invocations still work during the F1
///    transition window. F2 drops the path entirely.
///
/// F2 (Track G.2) replaces the v0.5 `cardinality` floor with the
/// explicit `sketches_per_cell` knob. The CLI passes the operator's
/// chosen `sketches_per_cell` directly (CLI > env > TOML > default)
/// so the matrix is built around the operator's per-cell fan-out
/// without an integer-division shortfall between `cardinality` and
/// `cells()`. The loop target is the matrix's
/// [`ExplorationMatrix::cardinality`] (cells × sketches_per_cell).
fn build_coordinator_matrix(
    run_dir: &Path,
    sketches_per_cell: usize,
    cfg: &crate::config::DiscoveryMatrixConfig,
) -> crate::error::Result<ExplorationMatrix> {
    tracing::debug!(
        run_dir = %run_dir.display(),
        sketches_per_cell,
        "build_coordinator_matrix"
    );
    // 1. Sidecar (resume + LLM-derive)
    if let Some(matrix) = ExplorationMatrix::load_or_derive(run_dir, sketches_per_cell)? {
        tracing::info!(
            source = "sidecar",
            cells = matrix.cells(),
            "build_coordinator_matrix: sourced from sidecar"
        );
        return Ok(matrix);
    }
    // 2. Operator-supplied spec (repetible / consolidated)
    let non_empty: Vec<&String> = cfg
        .matrix_spec
        .iter()
        .filter(|s| !s.trim().is_empty())
        .collect();
    if !non_empty.is_empty() {
        tracing::debug!(
            entries = non_empty.len(),
            "build_coordinator_matrix: parsing operator matrix_spec"
        );
        let spec = crate::discovery::MatrixSpec::parse_all(non_empty.into_iter().cloned())?;
        spec.validate()?;
        let m = ExplorationMatrix::from_spec(spec, sketches_per_cell);
        tracing::info!(
            source = "matrix_spec",
            cells = m.cells(),
            "build_coordinator_matrix: built from operator spec"
        );
        return Ok(m);
    }
    // 3. Legacy `--dimensions N --facets-per-dimension M` pair
    if let (Some(dims), Some(facets)) = (cfg.dimensions, cfg.facets_per_dimension) {
        tracing::debug!(
            dims = dims,
            facets = facets,
            "build_coordinator_matrix: building from legacy dims pair"
        );
        let mut spec = crate::discovery::MatrixSpec::default();
        for i in 0..dims.max(1) {
            let id = format!("dim-{:02}", i);
            let mut spec_facets = Vec::with_capacity(facets.max(1));
            for j in 0..facets.max(1) {
                spec_facets.push(crate::discovery::matrix_spec::FacetSpec {
                    id: format!("f{}", j + 1),
                    label: format!("F{}", j + 1),
                    description: String::new(),
                });
            }
            spec.dimensions
                .push(crate::discovery::matrix_spec::DimensionSpec {
                    id,
                    label: format!("Dimension {}", i),
                    facets: spec_facets,
                });
        }
        let m = ExplorationMatrix::from_spec(spec, sketches_per_cell);
        tracing::info!(
            source = "legacy_dims",
            cells = m.cells(),
            "build_coordinator_matrix: built from legacy dims"
        );
        return Ok(m);
    }
    // 4. Pure LLM-derive: no spec, no legacy counts — the matrix
    //    starts empty and the `discover_dimensions` phase will
    //    populate it before the matrix fan-out runs. The
    //    coordinator surfaces the empty matrix so a downstream
    //    `ExplorationMatrix::load_or_derive` call (in the matrix
    //    phase) reads the freshly-written sidecar.
    tracing::debug!("build_coordinator_matrix: falling through to empty matrix (LLM-derive)");
    Ok(ExplorationMatrix::new(Vec::new(), sketches_per_cell))
}

/// Build the user payload the LLM sees for one `(cell, sketch_index)`
/// pair. Mirrors `DiscoverMatrixPhase::user_payload` so the coordinator
/// and the flat-pipeline path emit equivalent prompts — a
/// `moagan discover` run driven by the coordinator produces the same
/// sketch text a flat-pipeline run would, which is the parity
/// guarantee PR-17 ships.
fn build_user_payload(brief: &str, cell: &MatrixCell, sketch_index: usize) -> String {
    tracing::trace!(
        cell_dim = %cell.dimension_id,
        cell_facet = %cell.facet_id,
        sketch_index,
        "build_user_payload"
    );
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
    let p = home.run_dir(*run_id).sketches();
    tracing::trace!(run_id = %run_id, path = %p.display(), "sketches_dir");
    p
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
        Err(_) => {
            tracing::trace!(dir = %dir.display(), "count_existing_sketches: absent");
            return 0;
        }
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
    tracing::trace!(
        dir = %dir.display(),
        count,
        "count_existing_sketches result"
    );
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
    ///
    /// F1 (Track G.2): the default config has no
    /// `[discovery_matrix].matrix_spec` and no legacy dimension
    /// counts, so the coordinator's matrix would be empty (the
    /// `discover_dimensions` phase is the one that fills it in
    /// the production path). For these regression tests we
    /// pre-populate the `matrix_spec` with the legacy 4×2 layout
    /// so the coordinator can build a non-empty matrix without
    /// touching an LLM.
    ///
    /// F2 (Track G.2): `sketches_per_cell` is set to `1` so the
    /// matrix cardinality equals the cell count (8 cells × 1 = 8
    /// sketches) — matching the v0.5 pre-F1 contract these
    /// regression tests pinned. The new F2 default
    /// (`sketches_per_cell = 10`) would fan out 80 sketches and
    /// require a 80-entry mock buffer; pinning `1` keeps the
    /// existing test fixtures bit-identical to the pre-F1
    /// behaviour.
    fn build_run_ctx(home: MoaganHome, scripted: Arc<ScriptedProvider>) -> Arc<RunContext> {
        let mut registry = crate::llm::ProviderRegistry::default();
        registry.insert("mock".into(), scripted);
        let mut cfg = crate::config::Config::default();
        cfg.discovery_matrix.matrix_spec = vec![
            "a=x,y".to_string(),
            "b=x,y".to_string(),
            "c=x,y".to_string(),
            "d=x,y".to_string(),
        ];
        cfg.discovery_matrix.sketches_per_cell = 1;
        let cfg = Arc::new(cfg);
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
        // F1 (Track G.2): the coordinator now sources its matrix
        // from the operator's `--matrix-spec` (carried on
        // `ctx.config.discovery_matrix.matrix_spec`) instead of the
        // legacy `default_for` 4×2 default. The test config
        // pre-populates `matrix_spec` with 4 dims × 2 facets
        // (`a=x,y`, `b=x,y`, …) so the matrix shape matches the
        // pre-F1 contract: 8 cells × `sketches_per_cell` sketches
        // per cell. `build_run_ctx` pins `sketches_per_cell = 1`
        // so the matrix cardinality stays at 8 (F2 default of 10
        // would fan out 80 and break the 8-entry mock buffer).
        // The coordinator's actual fan-out is the matrix's
        // cardinality (= 8 cells × 1 = 8 sketches), not the
        // mode-derived soft target (7), so the assertion uses the
        // matrix-driven value.
        let matrix_card = legacy_4x2_cardinality(target);
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
        let matrix_card = legacy_4x2_cardinality(target);
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
            // `TemperatureProfile::default()`. F1 (Track G.2):
            // the test populates `matrix_spec` with the legacy
            // 4×2 layout so the coordinator's matrix builder has
            // a non-empty shape to fan out against — without
            // this, the F1 build_coordinator_matrix would
            // produce an empty matrix and the v0.5 sketch-count
            // contract this test pins would no longer hold.
            // F2 (Track G.2): pin `sketches_per_cell = 1` so the
            // matrix cardinality stays at 8 cells × 1 = 8 — the
            // F2 default of 10 would inflate the mock buffer
            // requirement from 8 to 80 entries.
            let mut registry = crate::llm::ProviderRegistry::default();
            registry.insert("mock".into(), scripted_for_ctx);
            let mut cfg = crate::config::Config::default();
            cfg.discovery_matrix.matrix_spec = vec![
                "a=x,y".to_string(),
                "b=x,y".to_string(),
                "c=x,y".to_string(),
                "d=x,y".to_string(),
            ];
            cfg.discovery_matrix.sketches_per_cell = 1;
            let cfg = Arc::new(cfg);
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
        // F1 (Track G.2): the legacy `default_for_with_profiles`
        // constructor is gone. The test reconstructs the same
        // 4×2 matrix shape via `ExplorationMatrix::new` plus the
        // explicit profiles. The test config populates
        // `discovery_matrix.matrix_spec` with the equivalent
        // spec so the coordinator picks it up the same way the
        // pre-F1 path did.
        let matrix = crate::discovery::matrix::ExplorationMatrix::new(
            legacy_4x2_dimensions(),
            (target / 8).max(1),
        );
        let mut matrix = matrix;
        matrix.temperature_profiles = profiles.clone();
        matrix.default_profile = default_profile;
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
            // F1 (Track G.2): the config also carries the legacy
            // 4×2 `matrix_spec` so the coordinator's matrix
            // builder picks up the same shape the pre-F1 test
            // derived from `default_for_with_profiles`.
            let cfg = crate::config::Config {
                discovery_matrix: crate::config::DiscoveryMatrixConfig {
                    temperature_profiles: matrix.temperature_profiles.clone(),
                    default_profile: Some(matrix.default_profile.clone()),
                    matrix_spec: vec![
                        "a=x,y".to_string(),
                        "b=x,y".to_string(),
                        "c=x,y".to_string(),
                        "d=x,y".to_string(),
                    ],
                    dimensions: None,
                    facets_per_dimension: None,
                    llm_derive_first: false,
                    // F2 (Track G.2): pin `sketches_per_cell = 1`
                    // so the matrix cardinality matches the
                    // pre-F2 8-skeleton the test asserts. The F2
                    // default of 10 would inflate the mock buffer
                    // by 10× and break the `expected_calls`
                    // assertion below.
                    sketches_per_cell: 1,
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

    // ---- F1 helpers: legacy 4×2 matrix shape used by the ----
    // ---- pre-F1 coordinator regression tests.            ----

    /// F2 (Track G.2): `build_coordinator_matrix` takes the
    /// operator's `sketches_per_cell` directly (no integer
    /// division between `--cardinality` and `cells()`). The
    /// matrix's `cardinality()` collapses to `cells() ×
    /// sketches_per_cell` so a 4-dim × 2-facet spec with
    /// `sketches_per_cell = 10` produces an 80-sketch matrix
    /// (the v0.5 legacy floor) — the F2 contract just decouples
    /// the floor from the cell count.
    #[test]
    fn build_coordinator_matrix_uses_sketches_per_cell_directly() {
        use crate::config::DiscoveryMatrixConfig;
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = DiscoveryMatrixConfig {
            matrix_spec: vec![
                "a=x,y".to_string(),
                "b=x,y".to_string(),
                "c=x,y".to_string(),
                "d=x,y".to_string(),
            ],
            ..DiscoveryMatrixConfig::default()
        };
        let m = build_coordinator_matrix(dir.path(), 10, &cfg).expect("build matrix");
        // 4 dims × 2 facets = 8 cells × 10 sketches = 80 sketches
        // — the v0.5 cardinality baseline.
        assert_eq!(m.cells(), 8);
        assert_eq!(m.sketches_per_cell, 10);
        assert_eq!(m.cardinality(), 80);
    }

    /// F2: the per-cell fan-out flows through the legacy
    /// `--dimensions N --facets-per-dimension M` pair as well.
    /// The previous F1 path divided `cardinality / cells` to
    /// derive the per-cell hint; F2 reads the operator's
    /// explicit value verbatim.
    #[test]
    fn build_coordinator_matrix_honours_dimensions_pair_with_per_cell() {
        use crate::config::DiscoveryMatrixConfig;
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = DiscoveryMatrixConfig {
            dimensions: Some(2),
            facets_per_dimension: Some(3),
            ..DiscoveryMatrixConfig::default()
        };
        let m = build_coordinator_matrix(dir.path(), 7, &cfg).expect("build matrix");
        // 2 dims × 3 facets = 6 cells × 7 sketches per cell = 42.
        assert_eq!(m.cells(), 6);
        assert_eq!(m.sketches_per_cell, 7);
        assert_eq!(m.cardinality(), 42);
    }

    /// F1 (Track G.2): the pre-F1 coordinator regression tests
    /// assume the legacy 4×2 default matrix. The new
    /// `ExplorationMatrix` API has no `default_for` constructor;
    /// these helpers rebuild the same shape (4 dims × 2 facets)
    /// explicitly so the assertions stay byte-identical to the
    /// pre-F1 tests.
    fn legacy_4x2_cardinality(target: usize) -> usize {
        legacy_4x2_dimensions().len() * 2 * ((target / (legacy_4x2_dimensions().len() * 2)).max(1))
    }

    /// Build the 4-dim × 2-facet legacy matrix dimensions. Mirrors
    /// the pre-F1 `default_for` 4-axis layout: `deployment-model`,
    /// `storage`, `consistency`, `observability` (each with 2
    /// facets).
    fn legacy_4x2_dimensions() -> Vec<crate::discovery::matrix::Dimension> {
        use crate::discovery::matrix::{Dimension, Facet};
        vec![
            Dimension {
                id: "deployment-model".into(),
                label: "Deployment model".into(),
                facets: vec![
                    Facet {
                        id: "serverless".into(),
                        label: "serverless".into(),
                    },
                    Facet {
                        id: "self-hosted".into(),
                        label: "self-hosted".into(),
                    },
                ],
            },
            Dimension {
                id: "storage".into(),
                label: "Storage strategy".into(),
                facets: vec![
                    Facet {
                        id: "sql".into(),
                        label: "SQL".into(),
                    },
                    Facet {
                        id: "kv".into(),
                        label: "embedded key-value".into(),
                    },
                ],
            },
            Dimension {
                id: "consistency".into(),
                label: "Consistency model".into(),
                facets: vec![
                    Facet {
                        id: "strong".into(),
                        label: "strong".into(),
                    },
                    Facet {
                        id: "eventual".into(),
                        label: "eventual".into(),
                    },
                ],
            },
            Dimension {
                id: "observability".into(),
                label: "Observability".into(),
                facets: vec![
                    Facet {
                        id: "logs-only".into(),
                        label: "logs only".into(),
                    },
                    Facet {
                        id: "metrics-tracing".into(),
                        label: "metrics + tracing".into(),
                    },
                ],
            },
        ]
    }
}
