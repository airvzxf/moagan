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
    /// response. Every call is mirrored into the call-level telemetry
    /// (JSONL + SQLite when the index is enabled) with a fresh
    /// UUIDv7 id, start/end timestamps, the HTTP status, the
    /// reported token usage, and the model name. The phase name in
    /// the call record is `role.as_str()`, which matches the phase
    /// pipeline name.
    pub async fn call(&self, role: Role, system: String, user: String) -> Result<Response> {
        let req = Request {
            role,
            model: self.default_model.clone(),
            system,
            user,
            max_tokens: max_tokens_for_role(role),
            temperature: Some(0.4),
            top_p: Some(0.95),
            response_schema: None,
        };
        let provider = self.provider();
        let call_id = uuid::Uuid::now_v7().to_string();
        let started_unix = crate::time::now_unix_secs();
        let result = provider.send(&req).await;
        let ended_unix = crate::time::now_unix_secs();
        let phase_name = role.as_str();
        match &result {
            Ok((status, response)) => {
                let _ = self.telemetry.call(
                    &call_id,
                    phase_name,
                    phase_name,
                    self.default_provider.as_str(),
                    self.default_model.as_str(),
                    "",
                    false,
                    Some(*status),
                    response.usage.input_tokens,
                    response.usage.output_tokens,
                    response.usage.cache_read,
                    response.usage.cache_creation,
                    started_unix,
                    ended_unix,
                    None,
                );
            }
            Err(e) => {
                let _ = self.telemetry.call(
                    &call_id,
                    phase_name,
                    phase_name,
                    self.default_provider.as_str(),
                    self.default_model.as_str(),
                    "",
                    false,
                    None,
                    0,
                    0,
                    0,
                    0,
                    started_unix,
                    ended_unix,
                    Some(&e.to_string()),
                );
            }
        }
        result.map(|(_, r)| r)
    }

    /// Parse an LLM response as JSON. The `role` and `schema_hint` are
    /// currently only used for the error message; the actual parsing
    /// is just a `strip_code_fence` + `serde_json::from_str`. If the
    /// model produces invalid JSON, the error carries the full raw
    /// payload so the diagnostic travels with the failure.
    pub fn parse_model_json<T>(&self, role: Role, raw: &str, schema_hint: &str) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let _ = (role, schema_hint);
        crate::phases::util::parse_model_json::<T>(raw)
    }
}

/// Per-role `max_tokens`. The model can produce very verbose JSON for
/// the high-cardinality roles (intake, clarify, deliver); the others
/// stay compact. Calibrated for the v0.1 smoke (minimax Claude-style
/// endpoint). A future release will let providers override this
/// through the per-role config in `prompts/`.
fn max_tokens_for_role(role: Role) -> u32 {
    match role {
        Role::Intake => 131072,
        Role::Clarify => 131072,
        Role::Route => 131072,
        Role::Propose => 131072,
        Role::Gate => 131072,
        Role::Critique => 131072,
        Role::Repair => 131072,
        Role::Judge => 131072,
        Role::Rank => 131072,
        Role::Deliver => 131072,
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
