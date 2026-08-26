//! Pipeline executor. Runs the registered phases in order, recording
//! per-phase start/end in telemetry.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

use super::phase::{Phase, PhaseOutput, RunContext};

/// Which canonical pipeline shape the resumed run belongs to.
///
/// `moagan run` builds a [`PipelineKind::Linear`] pipeline
/// (`fast | standard | deep | explore | batch`) with the 15 linear
/// phases. `moagan discover` builds a [`PipelineKind::Discovery`]
/// pipeline that prepends `intake + clarify` to the eight
/// `discover_*` phases. The two shapes share `intake` and `clarify`
/// but diverge everywhere else, so the canonical phase order
/// (and therefore the resume semantics) depends on the kind.
///
/// v0.5 PR-24 (V4 §6.11, T01-06 §10.2) splits the canonical
/// phase list by kind so `Pipeline::resume` can dispatch to the
/// right index when a paused/failed discovery run is resumed
/// with `moagan continue --kind discovery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineKind {
    /// Linear run pipeline (`fast | standard | deep | explore | batch`).
    /// Phases: `intake → clarify → route → decompose? → sketch? →
    /// propose → validate? → cluster_proposals? → synthesize? →
    /// gate → critique → repair → judge → adversary? → rank →
    /// deliver`.
    Linear,
    /// Discovery pipeline (`moagan discover`). Phases: `intake →
    /// clarify → discover_matrix → discover_tag → discover_cluster
    /// → discover_contradict → discover_facet → discover_extract
    /// → discover_integrate → discover_summary`. The matrix fan-out
    /// itself is owned by [`crate::discovery::coordinator::DiscoveryCoordinator`]
    /// when the operator runs `moagan discover`; the phase list
    /// here is the canonical reference order for resume and for
    /// tests that exercise the discovery flow without the
    /// coordinator.
    Discovery,
}

/// Pipeline of phases. Built from a list of `Box<dyn Phase>` and
/// executed in order.
#[derive(Default)]
pub struct Pipeline {
    phases: Vec<Box<dyn Phase>>,
    /// `Some(last_phase)` when this pipeline was constructed via
    /// [`Pipeline::resume`]; `None` for fresh pipelines. The flag
    /// is read by [`Pipeline::run`] so every phase event emitted
    /// on a resumed pipeline carries `resume: true` in
    /// `telemetry/phases.jsonl.gz` (and the SQLite mirror).
    resume_from: Option<String>,
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("phase_count", &self.phases.len())
            .finish()
    }
}

impl Pipeline {
    /// Build an empty pipeline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a phase. Phases run in push order.
    pub fn push<P: Phase + 'static>(mut self, phase: P) -> Self {
        self.phases.push(Box::new(phase));
        self
    }

    /// Mark this pipeline as the continuation of a paused/failed
    /// run. The flag flows into [`Pipeline::run`] so every phase
    /// event emitted by the resumed pipeline carries
    /// `resume: true`. Set automatically by [`Pipeline::resume`];
    /// callers should not need to invoke this directly.
    fn with_resume_from(mut self, last_phase: &str) -> Self {
        self.resume_from = Some(last_phase.to_owned());
        self
    }

    /// `true` when this pipeline was produced by
    /// [`Pipeline::resume`] (so phase events should carry the
    /// `resume: true` flag).
    pub fn is_resumed(&self) -> bool {
        self.resume_from.is_some()
    }

    /// Number of registered phases.
    pub fn len(&self) -> usize {
        self.phases.len()
    }

    /// True if the pipeline is empty.
    pub fn is_empty(&self) -> bool {
        self.phases.is_empty()
    }

    /// Iterate over registered phase names in order.
    pub fn names(&self) -> Vec<&'static str> {
        self.phases.iter().map(|p| p.name()).collect()
    }

    /// Run the pipeline end-to-end. Returns the outputs of each phase
    /// in order. On the first error, records `error` in telemetry and
    /// returns the error.
    ///
    /// Async so that phases can fan out LLM calls in parallel while
    /// the Tokio runtime drives network and timer progress.
    ///
    /// The lease-renewal heartbeat task is spawned as the very first
    /// step so the run's `process_locks` row is renewed every
    /// `ctx.heartbeat_interval_secs()` while phases execute. The
    /// `JoinHandle` is recorded in `ctx`; `RunContext`'s `Drop`
    /// aborts the handle so the heartbeat cannot outlive the run
    /// (compliance with AGENTS.md §"No-go list": no `tokio::spawn`
    /// without a `JoinHandle` recorded or a `CancellationToken`
    /// parent — both hold here).
    pub async fn run(&self, ctx: &RunContext) -> Result<Vec<PhaseOutput>> {
        tracing::info!(
            phase_count = self.phases.len(),
            resumed = self.is_resumed(),
            "pipeline: run: start"
        );
        ctx.ensure_heartbeat()?;
        let timeout = ctx.total_timeout();
        if timeout.is_zero() {
            return self.run_phases(ctx).await;
        }
        match tokio::time::timeout(timeout, self.run_phases(ctx)).await {
            Ok(result) => {
                tracing::info!(success = result.is_ok(), "pipeline: run: complete");
                result
            }
            Err(_) => {
                tracing::error!(
                    secs = timeout.as_secs(),
                    "pipeline: run: total timeout exceeded"
                );
                ctx.cancel()
                    .cancel(crate::cancel::CancelReason::TotalTimeout);
                Err(crate::Error::Timeout {
                    message: format!("run exceeded {} seconds", timeout.as_secs()),
                    http_status: None,
                })
            }
        }
    }

    async fn run_phases(&self, ctx: &RunContext) -> Result<Vec<PhaseOutput>> {
        let resume = self.is_resumed();
        let mut outputs = Vec::with_capacity(self.phases.len());
        for (i, phase) in self.phases.iter().enumerate() {
            let seq = i as i64;
            tracing::debug!(seq, phase = phase.name(), "pipeline: phase start");
            ctx.telemetry
                .phase(phase.name(), seq, "start", None, resume)?;
            let timeout = ctx.phase_timeout();
            let result = if timeout.is_zero() {
                phase.execute(ctx).await
            } else {
                match tokio::time::timeout(timeout, phase.execute(ctx)).await {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::error!(
                            phase = phase.name(),
                            secs = timeout.as_secs(),
                            "pipeline: phase timeout exceeded"
                        );
                        ctx.cancel()
                            .cancel(crate::cancel::CancelReason::PhaseTimeout(
                                phase.name().to_owned(),
                            ));
                        Err(crate::Error::Timeout {
                            message: format!(
                                "phase {} exceeded {} seconds",
                                phase.name(),
                                timeout.as_secs()
                            ),
                            http_status: None,
                        })
                    }
                }
            };
            match &result {
                Ok(_) => {
                    tracing::debug!(seq, phase = phase.name(), "pipeline: phase end");
                    ctx.telemetry
                        .phase(phase.name(), seq, "end", None, resume)?;
                }
                Err(e) => {
                    tracing::error!(
                        seq,
                        phase = phase.name(),
                        error = %e,
                        "pipeline: phase error"
                    );
                    ctx.telemetry.phase(
                        phase.name(),
                        seq,
                        "error",
                        Some(&e.to_string()),
                        resume,
                    )?;
                }
            }
            outputs.push(result?);
        }
        Ok(outputs)
    }

    /// Canonical ordering of phases for the linear pipeline
    /// (`fast | standard | deep | batch | explore`). The list is the
    /// exact order produced by `build_pipeline_for_mode` in
    /// `src/cli/run.rs`; tests pin the order so a future re-ordering
    /// surfaces as a failing test rather than a silently wrong resume
    /// point.
    ///
    /// Discovery and `continue`/`rerun` do not use this list; they
    /// use [`Pipeline::canonical_phase_order_for(PipelineKind::Discovery)`]
    /// instead. Prefer the explicit `*_for(kind)` form in new code;
    /// this wrapper stays so existing callers keep compiling.
    pub fn canonical_phase_order() -> &'static [&'static str] {
        Self::canonical_phase_order_for(PipelineKind::Linear)
    }

    /// Canonical ordering of phases for a given pipeline kind. The
    /// returned list is the exact order produced by the
    /// corresponding builder (`build_pipeline_for_mode` for
    /// [`PipelineKind::Linear`], the flat discovery builder in
    /// `src/cli/discover.rs` for [`PipelineKind::Discovery`]). Tests
    /// pin the order so a future re-ordering surfaces as a failing
    /// test rather than a silently wrong resume point.
    ///
    /// v0.5 PR-24: this is the entry point that lets
    /// [`Pipeline::resume`] filter a discovery pipeline correctly.
    /// Without this split, `Pipeline::resume(canonical, "clarify")`
    /// on a discovery canonical pipeline errors out with
    /// `unknown phase "discover_matrix"`.
    pub fn canonical_phase_order_for(kind: PipelineKind) -> &'static [&'static str] {
        match kind {
            // Linear names mirror the pipeline builder; do NOT
            // introduce phases here without also updating
            // `build_pipeline_for_mode`. `decompose` is `deep`-only
            // and lands after `route`; the rest of the pipeline
            // picks it up from `Mode::Deep`. `adversary` (D.22.1,
            // D.12.5) lands between `judge` and `rank` so the
            // pattern-based report runs on the freshly judged panel;
            // the pipeline builder inserts it only when the run is
            // `Mode::Deep` or `--adversary` is set, so modes that
            // don't want the report keep the empty slot.
            PipelineKind::Linear => &[
                "intake",
                "clarify",
                "route",
                "decompose",
                "sketch",
                "propose",
                "validate",
                "cluster_proposals",
                "synthesize",
                "gate",
                "critique",
                "repair",
                "judge",
                "adversary",
                "rank",
                "deliver",
            ],
            // Discovery mirrors the flat builder in
            // `src/cli/discover.rs::build_discovery_pipeline`: the
            // pre-matrix phases (`intake + clarify`) seed the brief,
            // then the eight `discover_*` phases fan out sketches,
            // tag/cluster/contradict, derive facets, extract per-
            // facet markdown, integrate per category, and finally
            // produce the executive summary. When the operator runs
            // `moagan discover` end-to-end the matrix fan-out is
            // driven by the coordinator, but the canonical phase
            // order here is the single source of truth for resume
            // and for tests that exercise the discovery flow
            // without the coordinator.
            PipelineKind::Discovery => &[
                "intake",
                "clarify",
                "discover_matrix",
                "discover_tag",
                "discover_cluster",
                "discover_contradict",
                "discover_facet",
                "discover_extract",
                "discover_integrate",
                "discover_summary",
            ],
        }
    }

    /// Build a `BTreeMap<phase_name, canonical_index>` so callers can
    /// compare phase names without an ad-hoc Vec lookup. Indexes are
    /// stable across runs of the same `mode`. Backwards-compat
    /// wrapper around [`Pipeline::phase_index_for`]; new code
    /// should prefer the explicit `*_for(kind)` form.
    pub fn phase_index() -> BTreeMap<&'static str, usize> {
        Self::phase_index_for(PipelineKind::Linear)
    }

    /// Kind-aware variant of [`Pipeline::phase_index`]. The returned
    /// map contains every phase in
    /// [`Pipeline::canonical_phase_order_for(kind)`] keyed by its
    /// canonical index.
    pub fn phase_index_for(kind: PipelineKind) -> BTreeMap<&'static str, usize> {
        Self::canonical_phase_order_for(kind)
            .iter()
            .enumerate()
            .map(|(i, n)| (*n, i))
            .collect()
    }

    /// Build a resumed `Pipeline` from a pre-built canonical
    /// pipeline, skipping every phase whose canonical index is
    /// `<= last_phase`. The caller is responsible for building the
    /// canonical pipeline (so it can pass its own `Config`); this
    /// helper just filters.
    ///
    /// `last_phase` is the phase name returned by
    /// `Db::last_completed_phase(run_id)`. Errors out when
    /// `last_phase` is unknown (typo / out-of-band name) for the
    /// linear kind.
    ///
    /// The "skip phases whose canonical index <= last_phase" rule
    /// mirrors the T01-06 §10.2 pseudocode
    /// (`Pipeline::resume(manifest, db, last_phase)`): the run is
    /// treated as "this phase is done; pick up from the next one".
    ///
    /// Backwards-compat wrapper around [`Pipeline::resume_with_kind`]
    /// that hard-codes [`PipelineKind::Linear`]; new code should
    /// pass the kind explicitly (especially for discovery runs).
    pub fn resume(canonical: Pipeline, last_phase: &str) -> Result<Self> {
        Self::resume_with_kind(canonical, last_phase, PipelineKind::Linear)
    }

    /// Kind-aware [`Pipeline::resume`]. The `kind` selects which
    /// canonical phase list the cutoff lookup uses:
    /// [`PipelineKind::Discovery`] resolves `last_phase` against
    /// the eight `discover_*` phases plus the shared `intake +
    /// clarify` pre-matrix pair, while [`PipelineKind::Linear`]
    /// resolves against the 15-phase linear pipeline.
    ///
    /// The returned pipeline carries the `resume_from` marker; its
    /// [`Pipeline::run`] emits phase events with `resume: true` in
    /// `telemetry/phases.jsonl.gz` so post-execution review can
    /// distinguish resumed runs from fresh ones.
    pub fn resume_with_kind(
        canonical: Pipeline,
        last_phase: &str,
        kind: PipelineKind,
    ) -> Result<Self> {
        let idx_map = Self::phase_index_for(kind);
        let cutoff = *idx_map.get(last_phase).ok_or_else(|| {
            Error::InvalidState(format!("unknown phase {last_phase:?} in {kind:?} resume"))
        })?;
        let canonical_idx_map = canonical_index_for(&canonical, kind)?;
        let kept: Vec<Box<dyn Phase>> = canonical
            .phases
            .into_iter()
            .filter(|p| {
                canonical_idx_map
                    .get(p.name())
                    .map(|i| *i > cutoff)
                    .unwrap_or(false)
            })
            .collect();
        Ok(Self {
            phases: kept,
            resume_from: Some(last_phase.to_owned()),
        })
        .map(|p| p.with_resume_from(last_phase))
    }
}

/// Walk the canonical pipeline's phase list and assign each
/// phase a canonical index from
/// [`Pipeline::canonical_phase_order_for(kind)`]. Phases not in
/// the canonical list (e.g. the `cluster_proposals` alias used in
/// deep mode) get `usize::MAX` so the resume filter keeps them past
/// the cutoff.
fn canonical_index_for(pipeline: &Pipeline, kind: PipelineKind) -> Result<BTreeMap<String, usize>> {
    let canonical = Pipeline::canonical_phase_order_for(kind);
    let mut map: BTreeMap<String, usize> = BTreeMap::new();
    for phase in pipeline.phases.iter() {
        let name = phase.name();
        if let Some((i, _)) = canonical.iter().enumerate().find(|(_, n)| **n == name) {
            map.insert(name.to_string(), i);
        } else {
            map.insert(name.to_string(), usize::MAX);
        }
    }
    Ok(map)
}

/// Optional DAG-backed execution path. Activated only when the
/// binary is compiled with `--features dag` AND the run is in
/// [`crate::cli::Mode::Deep`].
///
/// ADR 0001 §D-1 admits `petgraph 0.6` behind a Cargo feature flag
/// so the default build keeps the linear `phases/` vector as the
/// canonical executor. When the operator opts in, this helper is
/// the entry point the CLI dispatcher reaches for: it builds the
/// deep-mode DAG via
/// [`crate::phases::dag::build_dag_for_deep_mode`] and routes the
/// phase set through [`crate::phases::dag::execute_dag`]. The
/// linear `Pipeline::run` path is untouched on every other code
/// path, which is what the regression guard in
/// `tests/integration_petgraph_dag.rs` pins down.
///
/// Returns `None` when the feature is off OR the mode is not
/// `deep` so the dispatcher falls through to the linear executor.
/// The `Some` branch is a future that resolves to the same
/// `Result<Vec<PhaseOutput>>` shape as the linear path.
#[cfg(feature = "dag")]
#[allow(clippy::type_complexity)]
pub fn maybe_run_via_dag<'a>(
    pipeline: &'a Pipeline,
    ctx: &'a RunContext,
) -> Option<
    core::pin::Pin<Box<dyn core::future::Future<Output = Result<Vec<PhaseOutput>>> + Send + 'a>>,
> {
    if ctx.mode != "deep" {
        return None;
    }
    let graph = crate::phases::dag::build_dag_for_deep_mode();
    let phases = &pipeline.phases;
    Some(Box::pin(async move {
        crate::phases::dag::execute_dag(&graph, phases, ctx).await
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::Telemetry;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct StubPhase(&'static str);
    #[async_trait]
    impl Phase for StubPhase {
        fn name(&self) -> &'static str {
            self.0
        }
        async fn execute(&self, _ctx: &RunContext) -> Result<PhaseOutput> {
            Ok(PhaseOutput::Intake(std::path::PathBuf::from(self.0)))
        }
    }

    struct SlowPhase;
    #[async_trait]
    impl Phase for SlowPhase {
        fn name(&self) -> &'static str {
            "slow"
        }
        async fn execute(&self, _ctx: &RunContext) -> Result<PhaseOutput> {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            Ok(PhaseOutput::Intake(std::path::PathBuf::from("slow")))
        }
    }

    fn empty_ctx() -> RunContext {
        let home = std::sync::Arc::new(crate::fs_layout::MoaganHome::at(std::path::PathBuf::from(
            "/tmp/moagan-test",
        )));
        RunContext::new(
            crate::ids::RunId::default(),
            home,
            Arc::new(crate::llm::ProviderRegistry::default()),
            "mock".into(),
            "mock-model".into(),
            crate::execution::Parallelism::new(1),
            Telemetry::noop(),
            String::new(),
            "fast".into(),
        )
    }

    #[test]
    fn pipeline_runs_phases_in_order() {
        let pipe = Pipeline::new().push(StubPhase("a")).push(StubPhase("b"));
        let ctx = empty_ctx();
        let out = pollster::block_on(pipe.run(&ctx)).unwrap();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn empty_pipeline_runs_zero_phases() {
        let pipe = Pipeline::new();
        let ctx = empty_ctx();
        let out = pollster::block_on(pipe.run(&ctx)).unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn phase_timeout_cancels_the_run() {
        let pipe = Pipeline::new().push(SlowPhase);
        let ctx = empty_ctx().with_timeout_durations(
            std::time::Duration::from_millis(20),
            std::time::Duration::ZERO,
        );
        let result = pipe.run(&ctx).await;
        assert!(matches!(result, Err(crate::Error::Timeout { .. })));
        assert_eq!(
            ctx.cancel().reason(),
            Some(crate::cancel::CancelReason::PhaseTimeout("slow".into()))
        );
    }

    #[tokio::test]
    async fn total_timeout_cancels_the_run() {
        let pipe = Pipeline::new().push(SlowPhase);
        let ctx = empty_ctx().with_timeout_durations(
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(20),
        );
        let result = pipe.run(&ctx).await;
        assert!(matches!(result, Err(crate::Error::Timeout { .. })));
        assert_eq!(
            ctx.cancel().reason(),
            Some(crate::cancel::CancelReason::TotalTimeout)
        );
    }

    /// `canonical_phase_order` is stable across runs of the same
    /// function. A re-ordering breaks every `Pipeline::resume`
    /// call that referenced the old index.
    #[test]
    fn canonical_phase_order_is_stable() {
        let a = Pipeline::canonical_phase_order();
        let b = Pipeline::canonical_phase_order();
        assert_eq!(a, b);
    }

    /// `phase_index` mirrors `canonical_phase_order` with stable
    /// ordering: a phase's index is its position in the canonical
    /// list, no duplicates.
    #[test]
    fn phase_index_round_trip() {
        let idx = Pipeline::phase_index();
        for (i, name) in Pipeline::canonical_phase_order().iter().enumerate() {
            assert_eq!(idx.get(*name).copied(), Some(i));
        }
    }

    /// `Pipeline::resume` keeps only the phases whose canonical
    /// index is strictly greater than the cutoff. The test uses
    /// stub phases named after the canonical entries so the
    /// `canonical_index_for` lookup succeeds.
    #[test]
    fn resume_skips_completed_phases() {
        let canonical = Pipeline::new()
            .push(StubPhase("intake"))
            .push(StubPhase("clarify"))
            .push(StubPhase("route"))
            .push(StubPhase("propose"))
            .push(StubPhase("deliver"));
        let resumed = Pipeline::resume(canonical, "clarify").unwrap();
        assert_eq!(resumed.names(), vec!["route", "propose", "deliver"]);
    }

    /// Resuming from the last canonical phase produces an empty
    /// pipeline (the run is already done).
    #[test]
    fn resume_from_last_phase_is_empty() {
        let canonical = Pipeline::new()
            .push(StubPhase("intake"))
            .push(StubPhase("clarify"))
            .push(StubPhase("deliver"));
        let resumed = Pipeline::resume(canonical, "deliver").unwrap();
        assert!(resumed.is_empty());
    }

    /// Resuming from an unknown phase errors out (typo / out-of-
    /// band name). `Error::InvalidState` matches the
    /// `last_completed_phase` contract — `resume` cannot pick a
    /// safe default.
    #[test]
    fn resume_unknown_phase_errors() {
        let canonical = Pipeline::new()
            .push(StubPhase("intake"))
            .push(StubPhase("deliver"));
        let err = Pipeline::resume(canonical, "ghost_phase").unwrap_err();
        assert!(matches!(err, Error::InvalidState(_)), "got: {err}");
    }

    /// Spin up a temp SQLite DB so the heartbeat can acquire a lease
    /// and renew it on every interval tick. Mirrors the helpers in
    /// `src/storage/lease.rs::tests` and `src/storage/sqlite.rs::tests`.
    fn temp_db() -> crate::storage::sqlite::Db {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("meta.sqlite");
        std::mem::forget(tmp);
        crate::storage::sqlite::Db::open(&path).expect("database opens")
    }

    /// Build a `RunContext` whose telemetry is wired to `db` so the
    /// heartbeat can acquire a lease. The home directory is
    /// discarded after construction because the heartbeat never
    /// touches the filesystem; only the SQLite index matters.
    fn ctx_with_db(
        db: crate::storage::sqlite::Db,
        heartbeat_interval_secs: u64,
        run_id: crate::ids::RunId,
    ) -> RunContext {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = std::sync::Arc::new(crate::fs_layout::MoaganHome::at(tmp.path().to_path_buf()));
        std::mem::forget(tmp);
        let run_dir = home.run_dir(run_id);
        let policy = crate::redact::RedactPolicy::default();
        let telemetry = crate::telemetry::Telemetry::open(run_id, &run_dir, policy, Some(db))
            .expect("telemetry opens with db");
        RunContext::new(
            run_id,
            home,
            Arc::new(crate::llm::ProviderRegistry::default()),
            "mock".into(),
            "mock-model".into(),
            crate::execution::Parallelism::new(1),
            telemetry,
            String::new(),
            "fast".into(),
        )
        .with_heartbeat_interval_secs(heartbeat_interval_secs)
    }

    /// Read the current fence for the run's heartbeat lease via the
    /// public `Db::lease_fence` helper. The `process_locks` row is
    /// keyed by `{run_id}|{holder}` (see `Db::renew_lease`), so the
    /// single-row query returns the live fence after every
    /// heartbeat renewal.
    fn read_heartbeat_fence(
        db: &crate::storage::sqlite::Db,
        run_id: crate::ids::RunId,
        holder: &str,
    ) -> u64 {
        db.lease_fence(run_id, holder)
            .expect("lease_fence succeeds")
            .unwrap_or_else(|| panic!("lease row must exist for run {run_id} / holder {holder}"))
    }

    /// `Pipeline::run` spawns the lease-renewal heartbeat before the
    /// phase loop. Without a SQLite index the helper is a no-op
    /// (legacy runs, dashboard read-only path) so we assert both
    /// branches: indexed → handle is set; unindexed → handle stays
    /// `None`.
    #[tokio::test]
    async fn pipeline_run_spawns_heartbeat_when_db_is_indexed() {
        let db = temp_db();
        let run_id = crate::ids::RunId::new();
        let ctx = ctx_with_db(db.clone(), 30, run_id);

        assert!(
            !ctx.heartbeat_spawned(),
            "heartbeat must not be spawned before pipeline.run"
        );

        let pipe = Pipeline::new().push(StubPhase("a"));
        pipe.run(&ctx).await.expect("pipeline succeeds");

        assert!(
            ctx.heartbeat_spawned(),
            "pipeline.run must record the heartbeat JoinHandle"
        );
        // Drop signals the heartbeat to abort via the cooperative
        // cancel; the task then unwinds and drops its `LeaseGuard`,
        // which deletes the row. Yield a couple of times so the
        // runtime drives the heartbeat to its exit before the
        // assertion reads the DB.
        drop(ctx);
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        let row = db
            .lease_fence(run_id, "heartbeat")
            .expect("lease_fence succeeds");
        assert!(
            row.is_none(),
            "lease row must be released once RunContext drops the heartbeat"
        );
    }

    /// `Pipeline::run` with no SQLite index is a no-op for the
    /// heartbeat helper: the run still completes, no handle is
    /// recorded, no lease is acquired.
    #[tokio::test]
    async fn pipeline_run_skips_heartbeat_when_db_is_unindexed() {
        let ctx = empty_ctx();
        let pipe = Pipeline::new().push(StubPhase("a"));
        pipe.run(&ctx).await.expect("pipeline succeeds");
        assert!(
            !ctx.heartbeat_spawned(),
            "heartbeat must not spawn without a SQLite index"
        );
    }

    /// `Pipeline::run` with a slow phase and the minimum heartbeat
    /// interval observes at least three lease renewals during the
    /// run. This is the v0.5 PR-07 acceptance test: a 60-second run
    /// must show at least 3 distinct `last_heartbeat_unix`
    /// timestamps; here we use a 3.3-second phase at the 1-second
    /// minimum interval and verify the same contract via the
    /// `process_locks.fence` column (which is bumped on every
    /// renewal).
    #[tokio::test]
    async fn pipeline_run_renews_lease_at_least_three_times() {
        let db = temp_db();
        let run_id = crate::ids::RunId::new();
        let ctx = ctx_with_db(db.clone(), 1, run_id);

        // Spawn the heartbeat manually so we can capture the
        // fence count before the pipeline runs.
        ctx.ensure_heartbeat().expect("heartbeat spawns");
        let initial_fence = read_heartbeat_fence(&db, run_id, "heartbeat");
        assert_eq!(
            initial_fence, 1,
            "first lease acquire must yield fence=1, got {initial_fence}"
        );

        let pipe = Pipeline::new().push(SlowHeartbeatPhase);
        let _ = pipe.run(&ctx).await.expect("pipeline succeeds");

        let final_fence = read_heartbeat_fence(&db, run_id, "heartbeat");
        let renewals = final_fence.saturating_sub(initial_fence);
        assert!(
            renewals >= 3,
            "heartbeat must have renewed at least 3 times during the slow phase: \
             initial={initial_fence}, final={final_fence}, renewals={renewals}"
        );
    }

    /// A slow phase tuned so the 1-second renewal interval fires
    /// roughly four times in the test budget. `tokio::time::interval`
    /// fires its first tick immediately after spawn, so a 3300 ms
    /// phase yields 4 ticks (immediate + 1000, 2000, 3000 ms) and
    /// at least 3 renewals. The assertion above uses `>= 3` so
    /// timing jitter cannot flake the test.
    struct SlowHeartbeatPhase;
    #[async_trait]
    impl Phase for SlowHeartbeatPhase {
        fn name(&self) -> &'static str {
            "slow_heartbeat"
        }
        async fn execute(&self, _ctx: &RunContext) -> Result<PhaseOutput> {
            tokio::time::sleep(std::time::Duration::from_millis(3300)).await;
            Ok(PhaseOutput::Intake(std::path::PathBuf::from(
                "slow_heartbeat",
            )))
        }
    }
}
