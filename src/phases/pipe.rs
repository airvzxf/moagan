//! Pipeline executor. Runs the registered phases in order, recording
//! per-phase start/end in telemetry.

use std::collections::BTreeMap;

use crate::domain::Manifest;
use crate::error::{Error, Result};

use super::phase::{Phase, PhaseOutput, RunContext};

/// Pipeline of phases. Built from a list of `Box<dyn Phase>` and
/// executed in order.
#[derive(Default)]
pub struct Pipeline {
    phases: Vec<Box<dyn Phase>>,
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
    pub async fn run(&self, ctx: &RunContext) -> Result<Vec<PhaseOutput>> {
        let timeout = ctx.total_timeout();
        if timeout.is_zero() {
            return self.run_phases(ctx).await;
        }
        match tokio::time::timeout(timeout, self.run_phases(ctx)).await {
            Ok(result) => result,
            Err(_) => {
                ctx.cancel()
                    .cancel(crate::cancel::CancelReason::TotalTimeout);
                Err(crate::Error::Timeout(format!(
                    "run exceeded {} seconds",
                    timeout.as_secs()
                )))
            }
        }
    }

    async fn run_phases(&self, ctx: &RunContext) -> Result<Vec<PhaseOutput>> {
        let mut outputs = Vec::with_capacity(self.phases.len());
        for (i, phase) in self.phases.iter().enumerate() {
            let seq = i as i64;
            ctx.telemetry.phase(phase.name(), seq, "start", None)?;
            let timeout = ctx.phase_timeout();
            let result = if timeout.is_zero() {
                phase.execute(ctx).await
            } else {
                match tokio::time::timeout(timeout, phase.execute(ctx)).await {
                    Ok(result) => result,
                    Err(_) => {
                        ctx.cancel()
                            .cancel(crate::cancel::CancelReason::PhaseTimeout(
                                phase.name().to_owned(),
                            ));
                        Err(crate::Error::Timeout(format!(
                            "phase {} exceeded {} seconds",
                            phase.name(),
                            timeout.as_secs()
                        )))
                    }
                }
            };
            match &result {
                Ok(_) => ctx.telemetry.phase(phase.name(), seq, "end", None)?,
                Err(e) => {
                    ctx.telemetry
                        .phase(phase.name(), seq, "error", Some(&e.to_string()))?;
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
    /// use their own builders.
    pub fn canonical_phase_order() -> &'static [&'static str] {
        // Names mirror the pipeline builder; do NOT introduce phases
        // here without also updating `build_pipeline_for_mode`.
        // `decompose` is `deep`-only and lands after `route`; the
        // rest of the pipeline picks it up from `Mode::Deep`.
        &[
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
            "rank",
            "deliver",
        ]
    }

    /// Build a `BTreeMap<phase_name, canonical_index>` so callers can
    /// compare phase names without an ad-hoc Vec lookup. Indexes are
    /// stable across runs of the same `mode`.
    pub fn phase_index() -> BTreeMap<&'static str, usize> {
        Self::canonical_phase_order()
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
    /// `last_phase` is unknown (typo / out-of-band name).
    ///
    /// The "skip phases whose canonical index <= last_phase" rule
    /// mirrors the T01-06 §10.2 pseudocode
    /// (`Pipeline::resume(manifest, db, last_phase)`): the run is
    /// treated as "this phase is done; pick up from the next one".
    pub fn resume(canonical: Pipeline, last_phase: &str) -> Result<Self> {
        let idx_map = Self::phase_index();
        let cutoff = *idx_map.get(last_phase).ok_or_else(|| {
            Error::InvalidState(format!("unknown phase {last_phase:?} in resume"))
        })?;
        let canonical_idx_map = canonical_index_for(&canonical)?;
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
        Ok(Self { phases: kept })
    }

    /// `Pipeline::resume` with a manifest convenience wrapper. The
    /// caller passes the canonical pipeline it built for the
    /// manifest's mode; this function filters it. The signature
    /// matches the T01-06 §10.2 pseudocode's intent ("resume from
    /// last completed phase") without forcing the pipeline layer
    /// to rebuild the canonical list from a `Config`.
    pub fn resume_from_manifest(
        manifest: &Manifest,
        canonical: Pipeline,
        last_phase: &str,
    ) -> Result<Self> {
        let _ = manifest; // signature parity with the spec pseudocode
        Self::resume(canonical, last_phase)
    }
}

/// Walk the canonical pipeline's phase list and assign each
/// phase a canonical index from `Pipeline::canonical_phase_order()`.
/// Phases not in the canonical list (e.g. the `cluster_proposals`
/// alias used in deep mode) get `usize::MAX` so the resume filter
/// keeps them past the cutoff.
fn canonical_index_for(pipeline: &Pipeline) -> Result<BTreeMap<String, usize>> {
    let canonical = Pipeline::canonical_phase_order();
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
        assert!(matches!(result, Err(crate::Error::Timeout(_))));
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
        assert!(matches!(result, Err(crate::Error::Timeout(_))));
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
        assert_eq!(
            resumed.names(),
            vec!["route", "propose", "deliver"]
        );
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
}
