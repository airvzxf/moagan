//! Pipeline phase trait. Each phase is a unit of work that reads the
//! artefacts left by the previous phase and writes new ones.
//!
//! Compliance: T01-06 §8 (non-discovery pipeline).
//! 10-integrada-v0 §D.12.1 defines `PhaseObject` and the layer graph;
//! the v0.1 MVP uses a flat `Vec<Box<dyn Phase>>` per the baseline.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::execution::Parallelism;
use crate::fs_layout::{MoaganHome, RunDir};
use crate::ids::RunId;
use crate::llm::cache::{Cache, CacheConfig};
use crate::llm::{ProviderRegistry, Request, Response, Role};
use crate::telemetry::{Telemetry, WarningContext};

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
    /// Cross-run LLM cache rooted at `<MOAGAN_HOME>/cache/llm`.
    /// Consulted before every provider call and populated after a
    /// successful call so subsequent runs of the same prompt reuse
    /// the cached response (compliance with T01-06 §3.3).
    pub cache: Cache,
}

impl std::fmt::Debug for RunContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunContext")
            .field("run_id", &self.run_id)
            .field("default_provider", &self.default_provider)
            .field("default_model", &self.default_model)
            .field("mode", &self.mode)
            .field("cache_root", &self.cache.config_for_debug().root)
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
        let cache = Cache::new(CacheConfig {
            root: home.cross_run_cache_dir(),
            cross_run: true,
            no_store: false,
        });
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
            cache,
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
    ///
    /// The cross-run LLM cache is consulted first: a hit short-
    /// circuits the provider call and records a `cache_hit=1` row
    /// in the `calls` table. A miss falls through to the provider
    /// and the response is stored for the next run.
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
        let cache_key = Cache::cache_key(&req, &self.default_provider, &self.default_model);
        let started_unix = crate::time::now_unix_secs();
        if let Some(entry) = self.cache.lookup(&cache_key)? {
            return self.record_cache_hit(entry, role, &cache_key, started_unix);
        }
        self.dispatch_to_provider(req, Some(cache_key), started_unix)
            .await
    }

    /// Provider call without consulting the cache. Used on parse-
    /// failure retries (see `call_with_retry_parse`) so a previously
    /// cached broken response does not poison the retry.
    pub(crate) async fn call_uncached(
        &self,
        role: Role,
        system: String,
        user: String,
        started_unix: i64,
    ) -> Result<Response> {
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
        self.dispatch_to_provider(req, None, started_unix).await
    }

    /// Send the prepared request to the provider, record telemetry,
    /// emit truncation / empty warnings, and (when a `cache_key` is
    /// supplied) persist the response in the cross-run cache.
    async fn dispatch_to_provider(
        &self,
        req: Request,
        cache_key: Option<String>,
        started_unix: i64,
    ) -> Result<Response> {
        let provider = self.provider();
        let call_id = uuid::Uuid::now_v7().to_string();
        let result = provider.send(&req).await;
        let ended_unix = crate::time::now_unix_secs();
        let phase_name = req.role.as_str();
        let ctx = || WarningContext {
            phase: Some(phase_name.to_owned()),
            role: Some(phase_name.to_owned()),
            call_id: Some(call_id.clone()),
            attempt: Some(0),
        };
        match &result {
            Ok((status, response)) => {
                if let Some(ref key) = cache_key {
                    let _ = self.cache.store(
                        key,
                        self.default_provider.as_str(),
                        self.default_model.as_str(),
                        response,
                    );
                }
                let _ = self.telemetry.call(
                    &call_id,
                    phase_name,
                    phase_name,
                    self.default_provider.as_str(),
                    self.default_model.as_str(),
                    cache_key.as_deref().unwrap_or(""),
                    false,
                    Some(*status),
                    response.usage.input_tokens,
                    response.usage.output_tokens,
                    0,
                    response.usage.cache_creation,
                    started_unix,
                    ended_unix,
                    None,
                );
                if response.truncated {
                    let _ = self.telemetry.warn(
                        "model.response_truncated",
                        "warn",
                        "model response ended at max_tokens",
                        serde_json::json!({
                            "text_bytes": response.text.len(),
                            "finish_reason": response.finish_reason,
                        }),
                        ctx(),
                    );
                }
                if response.text.is_empty() {
                    let _ = self.telemetry.warn(
                        "model.response_empty",
                        "warn",
                        "model returned an empty text block",
                        serde_json::json!({
                            "finish_reason": response.finish_reason,
                        }),
                        ctx(),
                    );
                }
            }
            Err(e) => {
                let _ = self.telemetry.call(
                    &call_id,
                    phase_name,
                    phase_name,
                    self.default_provider.as_str(),
                    self.default_model.as_str(),
                    cache_key.as_deref().unwrap_or(""),
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

    /// Record a cache hit and surface the cached response. The cached
    /// `usage` is used to populate `cache_read`/`cache_creation` in
    /// the call record so the run's cache-hit rate is observable.
    fn record_cache_hit(
        &self,
        entry: crate::llm::cache::CacheEntry,
        role: Role,
        cache_key: &str,
        started_unix: i64,
    ) -> Result<Response> {
        let response = entry.response;
        let ended_unix = crate::time::now_unix_secs();
        let call_id = uuid::Uuid::now_v7().to_string();
        let phase_name = role.as_str();
        let _ = self.telemetry.call(
            &call_id,
            phase_name,
            phase_name,
            self.default_provider.as_str(),
            self.default_model.as_str(),
            cache_key,
            true,
            Some(200),
            response.usage.input_tokens,
            response.usage.output_tokens,
            entry.usage.cache_read,
            0,
            started_unix,
            ended_unix,
            None,
        );
        if response.truncated {
            let _ = self.telemetry.warn(
                "model.response_truncated",
                "warn",
                "model response ended at max_tokens",
                serde_json::json!({
                    "text_bytes": response.text.len(),
                    "finish_reason": response.finish_reason,
                    "cache_hit": true,
                }),
                WarningContext {
                    phase: Some(phase_name.to_owned()),
                    role: Some(phase_name.to_owned()),
                    call_id: Some(call_id),
                    attempt: Some(0),
                },
            );
        }
        Ok(response)
    }

    /// Parse an LLM response as JSON. The `role` is used to produce a
    /// role-aware error message when the schema doesn't match. The
    /// `schema_hint` parameter is kept for backwards compatibility;
    /// the role's `schema_description()` is now the canonical source.
    ///
    /// After the direct parse fails, we try the narrow bracket-repair
    /// pass and re-parse. If both fail we run `role.validate_json()`
    /// against the raw payload so the operator sees a message like
    /// `role=Critique schema mismatch: missing field 'verdict'` instead
    /// of `expected ',' or ']' at line 1 column N`.
    ///
    /// Every repair pass that actually modified the model output is
    /// reported to the warnings stream as a `model.json_repair_applied`
    /// event with the kind (colon / separator / bracket) and the
    /// byte delta. Use this method from phase code instead of
    /// `crate::phases::util::parse_model_json` so the post-execution
    /// review can see which m3 pathology was triggered.
    pub fn parse_model_json<T>(&self, role: Role, raw: &str, schema_hint: &str) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let _ = schema_hint;
        let phase_name = role.as_str();
        let parsed = crate::phases::util::parse_model_json_traced::<T, _>(raw, |ev| {
            let _ = self.telemetry.warn(
                "model.json_repair_applied",
                "warn",
                "model JSON was auto-corrected",
                serde_json::json!({
                    "repair_kind": ev.kind.as_str(),
                    "bytes_before": ev.bytes_before,
                    "bytes_after": ev.bytes_after,
                    "bytes_delta": ev.bytes_after as i64 - ev.bytes_before as i64,
                }),
                WarningContext {
                    phase: Some(phase_name.to_owned()),
                    role: Some(phase_name.to_owned()),
                    call_id: None,
                    attempt: None,
                },
            );
        });
        match parsed {
            Ok(v) => Ok(v),
            Err(util_err) => {
                // If the raw can be parsed into a generic JSON value,
                // ask the role for a schema-aware diagnostic. This is
                // done only on the failure path (cost: one extra parse)
                // and the value is dropped before the error propagates.
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw)
                    && let Err(schema_err) = role.validate_json(&value)
                {
                    let _ = self.telemetry.warn(
                        "model.schema_mismatch",
                        "warn",
                        "model JSON failed role schema check",
                        serde_json::json!({
                            "schema_mismatch": schema_err.to_string(),
                        }),
                        WarningContext {
                            phase: Some(phase_name.to_owned()),
                            role: Some(phase_name.to_owned()),
                            call_id: None,
                            attempt: None,
                        },
                    );
                    return Err(schema_err);
                }
                Err(util_err)
            }
        }
    }

    /// Call the model and parse the response, retrying the call up to
    /// `max_retries` additional times if the parse fails. Each attempt
    /// goes through the normal pipeline (provider send + telemetry +
    /// bracket-repair + validator), so retries show up in the call-level
    /// metrics just like any other LLM call.
    ///
    /// Every retry, recovery, and parse failure is recorded as a
    /// structured warning (`model.retry_parse`, `model.retry_provider`,
    /// `model.recovered_after_retry`) so the post-execution review
    /// can answer "did the model fail?" without scraping stderr.
    pub async fn call_with_retry_parse<T>(
        &self,
        role: Role,
        system: String,
        user: String,
        schema_hint: &str,
        max_retries: u32,
    ) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let phase_name = role.as_str();
        for attempt in 0..=max_retries {
            // First attempt uses the cached path so re-running the
            // same prompt reuses the prior response. Retries bypass
            // the cache so a previously cached broken response does
            // not poison the retry loop.
            let started_unix = crate::time::now_unix_secs();
            let response = if attempt == 0 {
                self.call(role, system.clone(), user.clone()).await
            } else {
                self.call_uncached(role, system.clone(), user.clone(), started_unix)
                    .await
            };
            let warn_ctx = || WarningContext {
                phase: Some(phase_name.to_owned()),
                role: Some(phase_name.to_owned()),
                call_id: None,
                attempt: Some(attempt),
            };
            match response {
                Ok(resp) => match self.parse_model_json::<T>(role, &resp.text, schema_hint) {
                    Ok(v) => {
                        if attempt > 0 {
                            let _ = self.telemetry.warn(
                                "model.recovered_after_retry",
                                "info",
                                "model answer recovered after retry",
                                serde_json::json!({
                                    "attempts": attempt + 1,
                                }),
                                warn_ctx(),
                            );
                        }
                        return Ok(v);
                    }
                    Err(e) if attempt < max_retries => {
                        let _ = self.telemetry.warn(
                            "model.retry_parse",
                            "warn",
                            "model response parse failed; retrying",
                            serde_json::json!({
                                "attempt": attempt + 1,
                                "max_attempts": max_retries + 1,
                                "error": e.to_string(),
                            }),
                            warn_ctx(),
                        );
                        continue;
                    }
                    Err(e) => return Err(e),
                },
                Err(e) if attempt < max_retries => {
                    let _ = self.telemetry.warn(
                        "model.retry_provider",
                        "warn",
                        "model call failed; retrying",
                        serde_json::json!({
                            "attempt": attempt + 1,
                            "max_attempts": max_retries + 1,
                            "error": e.to_string(),
                        }),
                        warn_ctx(),
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }
}

/// Per-role `max_tokens` ceiling. Calibrated for the v0.1 smoke
/// (`minimax` Claude-style endpoint that accepts up to 128K output).
/// These ceilings match T01-06 §4.2 — the old `131072` constant was
/// leaving ~99% of the budget unused on roles that emit 1-2 KB JSON
/// (intake, route, judge) and let the model drift into verbose prose
/// on roles that should be terse.
///
/// A future release will let providers override these defaults through
/// the per-role `prompts/registry.rs` configuration block, but the
/// floors here are the contract: an intake never needs more than 1024
/// tokens of output, a sketch never more than 1024, a clarification
/// never more than 2048, etc.
fn max_tokens_for_role(role: Role) -> u32 {
    match role {
        Role::Intake => 1024,
        Role::Clarify => 2048,
        Role::Route => 512,
        Role::Sketch => 1024,
        Role::Propose => 6000,
        Role::Gate => 1024,
        Role::Critique => 4000,
        Role::Repair => 6000,
        Role::Judge => 1500,
        Role::Rank => 1500,
        Role::Deliver => 4000,
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
    /// A list of `sketches/sk_*.json` files. Empty for `fast` mode.
    Sketches(Vec<PathBuf>),
    /// A list of `proposals/p_*.json` files.
    Proposals(Vec<PathBuf>),
    /// A list of `validation/p_*.json` files (one per proposal).
    Validations(Vec<PathBuf>),
    /// A list of `critiques/p_*_critic_*.json` files.
    Critiques(Vec<PathBuf>),
    /// A list of `revisions/p_*_rev_<n>.json` files.
    Repairs(Vec<PathBuf>),
    /// A list of `evaluations/p_*.json` files.
    Evaluations(Vec<PathBuf>),
    /// `rankings/ranking.json` was written.
    Ranking(PathBuf),
    /// `final/portfolio.md` was written.
    Deliver(PathBuf),
}

/// A unit of pipeline work.
#[async_trait]
pub trait Phase: Send + Sync {
    /// Stable phase name (e.g. `"intake"`, `"propose"`).
    fn name(&self) -> &'static str;
    /// Execute the phase. Implementations should record phase start
    /// and end through `ctx.telemetry` and write artefacts under
    /// `ctx.run_dir`. Async so that phases can fan out LLM calls in
    /// parallel via `futures::future::join_all` while respecting the
    /// global `parallelism` cap acquired through `ctx.parallelism`.
    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput>;
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
