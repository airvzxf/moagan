//! Pipeline phase trait. Each phase is a unit of work that reads the
//! artefacts left by the previous phase and writes new ones.
//!
//! Compliance: T01-06 §8 (non-discovery pipeline).
//! 10-integrada-v0 §D.11 defines `PhaseObject` and the layer graph;
//! the v0.1 MVP uses a flat `Vec<Box<dyn Phase>>` per the baseline.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::execution::Parallelism;
use crate::fs_layout::{MoaganHome, RunDir};
use crate::ids::RunId;
use crate::llm::{ProviderRegistry, Request, Response, Role};
use crate::telemetry::Telemetry;

/// Shared state every phase can read.
#[derive(Clone)]
pub struct RunContext {
    /// Run identifier.
    pub run_id: RunId,
    /// Moagan home, kept alive for the run's lifetime.
    pub home: Arc<MoaganHome>,
    /// Provider registry.
    pub providers: Arc<ProviderRegistry>,
    /// Default provider name.
    pub default_provider: String,
    /// Default model name.
    pub default_model: String,
    /// Global concurrency cap.
    pub parallelism: Parallelism,
    /// Telemetry handle.
    pub telemetry: Telemetry,
    /// Raw user prompt.
    pub raw_prompt: String,
    /// Original mode string (e.g. "fast" or "standard").
    pub mode: String,
}

impl std::fmt::Debug for RunContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunContext")
            .field("run_id", &self.run_id)
            .field("default_provider", &self.default_provider)
            .field("default_model", &self.default_model)
            .field("mode", &self.mode)
            .finish()
    }
}

impl RunContext {
    /// Build a new run context.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run_id: RunId,
        home: Arc<MoaganHome>,
        providers: Arc<ProviderRegistry>,
        default_provider: String,
        default_model: String,
        parallelism: Parallelism,
        telemetry: Telemetry,
        raw_prompt: String,
        mode: String,
    ) -> Self {
        Self {
            run_id,
            home,
            providers,
            default_provider,
            default_model,
            parallelism,
            telemetry,
            raw_prompt,
            mode,
        }
    }

    /// Borrow the run-specific directory namespace.
    pub fn run_dir(&self) -> RunDir<'_> {
        self.home.run_dir(self.run_id)
    }

    /// Resolve the active provider by name.
    pub fn provider(&self) -> Arc<dyn crate::llm::Provider> {
        self.providers
            .get(&self.default_provider)
            .expect("default provider must be registered")
    }

    /// Send a `Request` through the active provider and surface the
    /// response. Used by every LLM-backed phase.
    pub async fn call(&self, role: Role, system: String, user: String) -> Result<Response> {
        let req = Request {
            role,
            model: self.default_model.clone(),
            system,
            user,
            max_tokens: 1024,
            temperature: Some(0.4),
            top_p: Some(0.95),
            response_schema: None,
        };
        let provider = self.provider();
        provider.send(&req).await
    }
}

/// Outcome of a phase. Each variant corresponds to a sidecar file
/// that the phase is responsible for writing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "path")]
pub enum PhaseOutput {
    /// `intake.json` was written.
    Intake(PathBuf),
    /// `brief.json` was written.
    Brief(PathBuf),
    /// `route.json` was written.
    Route(PathBuf),
    /// A list of `proposals/p_*.json` files.
    Proposals(Vec<PathBuf>),
    /// A list of `validation/p_*.json` files (one per proposal).
    Validations(Vec<PathBuf>),
    /// A list of `critiques/p_*_critic_*.json` files.
    Critiques(Vec<PathBuf>),
    /// A list of `revisions/p_*_rev_*.json` files.
    Repairs(Vec<PathBuf>),
    /// A list of `evaluations/p_*.json` files.
    Evaluations(Vec<PathBuf>),
    /// `rankings/ranking.json` was written.
    Ranking(PathBuf),
    /// `final/portfolio.md` was written.
    Deliver(PathBuf),
}

/// A unit of pipeline work.
pub trait Phase: Send + Sync {
    /// Stable phase name (e.g. `"intake"`, `"propose"`).
    fn name(&self) -> &'static str;
    /// Execute the phase. Implementations should record phase start
    /// and end through `ctx.telemetry` and write artefacts under
    /// `ctx.run_dir`.
    fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_output_serializes_tagged() {
        let out = PhaseOutput::Intake(PathBuf::from("/tmp/intake.json"));
        let j = serde_json::to_string(&out).unwrap();
        assert!(j.contains("Intake"));
        assert!(j.contains("/tmp/intake.json"));
    }
}
