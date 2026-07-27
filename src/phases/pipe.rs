//! Pipeline executor. Runs the registered phases in order, recording
//! per-phase start/end in telemetry.

use crate::error::Result;

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
}
