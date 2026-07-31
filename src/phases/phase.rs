//! Pipeline phase trait. Each phase is a unit of work that reads the
//! artefacts left by the previous phase and writes new ones.
//!
//! Compliance: T01-06 §8 (non-discovery pipeline).
//! 10-integrada-v0 §D.12.1 defines `PhaseObject` and the layer graph;
//! the v0.1 MVP uses a flat `Vec<Box<dyn Phase>>` per the baseline.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::cancel::Cancel;
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
    /// Whether the human-in-the-loop checkpoints are interactive
    /// (`true`) or auto-suppressed (`false`). Phase D opt-out;
    /// wired from `--non-interactive` and `Mode::Batch`.
    pub interactive: bool,
    cancel: Cancel,
    phase_timeout: Duration,
    total_timeout: Duration,
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
            ..Default::default()
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
            interactive: true,
            cancel: Cancel::new(),
            phase_timeout: Duration::ZERO,
            total_timeout: Duration::ZERO,
        }
    }

    /// Toggle the human-checkpoint interactivity. `false` makes
    /// every checkpoint a no-op that persists a `<skipped:non_interactive>`
    /// marker instead of blocking on stdin.
    pub fn with_interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    pub(crate) fn with_timeouts(self, phase_secs: u64, total_secs: u64) -> Self {
        self.with_timeout_durations(
            Duration::from_secs(phase_secs),
            Duration::from_secs(total_secs),
        )
    }

    pub(crate) fn with_timeout_durations(
        mut self,
        phase_timeout: Duration,
        total_timeout: Duration,
    ) -> Self {
        self.phase_timeout = phase_timeout;
        self.total_timeout = total_timeout;
        self
    }

    pub(crate) fn phase_timeout(&self) -> Duration {
        self.phase_timeout
    }

    pub(crate) fn total_timeout(&self) -> Duration {
        self.total_timeout
    }

    pub(crate) fn cancel(&self) -> &Cancel {
        &self.cancel
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
            temperature: Some(temperature_for_role(role)),
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
            temperature: Some(temperature_for_role(role)),
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
        let request_body_sha256 = (self.default_provider == "minimax")
            .then(|| crate::llm::http::request_body_sha256(&req))
            .transpose()?;
        let provider = self.provider();
        let call_id = uuid::Uuid::now_v7().to_string();
        let provider_started = std::time::Instant::now();
        tracing::debug!(
            call_id = %call_id,
            phase = req.role.as_str(),
            stage = "provider.send.started",
            "LLM call stage"
        );
        let result = provider.send(&req).await;
        tracing::debug!(
            call_id = %call_id,
            phase = req.role.as_str(),
            stage = "provider.send.completed",
            elapsed_ms = provider_started.elapsed().as_millis(),
            success = result.is_ok(),
            "LLM call stage"
        );
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
                    let cache_started = std::time::Instant::now();
                    tracing::debug!(
                        call_id = %call_id,
                        phase = phase_name,
                        stage = "cache.store.started",
                        "LLM call stage"
                    );
                    match self.cache.store(
                        key,
                        self.default_provider.as_str(),
                        self.default_model.as_str(),
                        response,
                    ) {
                        Ok(()) => tracing::debug!(
                            call_id = %call_id,
                            phase = phase_name,
                            stage = "cache.store.completed",
                            elapsed_ms = cache_started.elapsed().as_millis(),
                            "LLM call stage"
                        ),
                        Err(e) => tracing::warn!(
                            call_id = %call_id,
                            phase = phase_name,
                            stage = "cache.store.error",
                            error = %e,
                            "LLM call stage"
                        ),
                    }
                }
                if let Err(e) = self.telemetry.call(
                    &call_id,
                    phase_name,
                    phase_name,
                    self.default_provider.as_str(),
                    self.default_model.as_str(),
                    cache_key.as_deref().unwrap_or(""),
                    request_body_sha256.as_deref(),
                    false,
                    Some(*status),
                    response.usage.input_tokens,
                    response.usage.output_tokens,
                    0,
                    response.usage.cache_creation,
                    started_unix,
                    ended_unix,
                    None,
                ) {
                    tracing::warn!(
                        call_id = %call_id,
                        phase = phase_name,
                        stage = "telemetry.call.error",
                        error = %e,
                        "LLM call stage"
                    );
                } else {
                    tracing::debug!(
                        call_id = %call_id,
                        phase = phase_name,
                        stage = "telemetry.call.completed",
                        "LLM call stage"
                    );
                }
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
                if let Err(telemetry_error) = self.telemetry.call(
                    &call_id,
                    phase_name,
                    phase_name,
                    self.default_provider.as_str(),
                    self.default_model.as_str(),
                    cache_key.as_deref().unwrap_or(""),
                    request_body_sha256.as_deref(),
                    false,
                    None,
                    0,
                    0,
                    0,
                    0,
                    started_unix,
                    ended_unix,
                    Some(&e.to_string()),
                ) {
                    tracing::warn!(
                        call_id = %call_id,
                        phase = phase_name,
                        stage = "telemetry.call.error",
                        error = %telemetry_error,
                        "LLM call stage"
                    );
                }
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
            None,
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
/// (`minimax` Claude-style endpoint that accepts up to 128K output)
/// and revised upward after empirical observation that the model
/// legitimately needs much more headroom for prose-heavy roles.
///
/// The model is **not** an algorithm with fixed output length —
/// `critique` may produce a paragraph explaining a borderline
/// score; `propose` may include a code block; `repair` may carry
/// the full original proposal plus a diff; `deliver` may render
/// a long portfolio with multiple sections. Hard-coding low
/// ceilings (`max_tokens=1500` for judge, `4000` for critique)
/// truncates these legitimately large responses mid-thought,
/// surfaces `finish_reason=max_tokens` warnings, and forces the
/// retry loop into additional cost.
///
/// New calibration (2026-07-27, powers-of-two so the ceilings are
/// round numbers operators can reason about):
///
///   intake    1024    rephrase + extract, schema is tight
///   clarify   2048    brief with several assumptions
///   route      512    single JSON object
///   sketch    1024    single JSON object, ~500 tokens
///   propose  32768    full approach with code/tradeoffs/evidence
///   gate      1024    single JSON object
///   critique  8192    paragraph-length verdict with suggestions
///   repair   16384    full proposal + diff
///   judge     2048    rubric breakdown + comments
///   rank      2048    ordered ranking + representatives
///   deliver   8192    portfolio with title/summary/recommendation +
///                     alternatives + next_steps
///
/// A future release will let providers override these defaults
/// through the per-role `prompts/registry.rs` configuration block,
/// but the values here are the contract: when the model wants to
/// write more, let it; when it wants less, it returns less.
fn max_tokens_for_role(role: Role) -> u32 {
    match role {
        Role::Intake => 1024,
        Role::Clarify => 2048,
        Role::Route => 512,
        Role::Sketch => 1024,
        Role::Propose => 32768,
        Role::Gate => 1024,
        Role::Critique => 8192,
        Role::Repair => 16384,
        Role::Judge => 2048,
        Role::Rank => 2048,
        Role::Deliver => 8192,
        // Discovery (Plan B sub-phase B). Per docs/v0.2-status.md and
        // proposal-01-concept.md §6.5–§6.10.
        Role::Tagger => 512,
        Role::Extractor => 3000,
        Role::Integrator => 4000,
        // Phase D (Plan B sub-phase D). Synthesizer reuses the
        // integrator ceiling (markdown body + structured fields).
        // Adversary stays short: it returns weaknesses, not a long
        // report.
        Role::Synthesizer => 4000,
        Role::Adversary => 2048,
        // Phase G (v0.3). Decomposer: T01-06 §4.2 says 3000; we
        // round up to 4096 so the JSON can carry a multi-node
        // graph without truncating the `dependencies` arrays.
        Role::Decomposer => 4096,
    }
}

/// Per-role sampling temperature. Replaces the previous v0.1 hardcoded
/// `Some(0.4)` for every role.
///
/// Calibration rationale (empirical, 2026-07-27):
///
/// - **Sketch (`1.0`)**: empirical sweeps showed T=1.0 maximises the
///   standard deviation of thesis vectors across the angle-cycled
///   fan-out — i.e. the largest semantic spread. This is what the
///   `sketches_summary.json` "kept" count and downstream clustering
///   rely on; lower temperatures (0.4, 0.7) produce near-duplicates
///   that waste LLM budget without expanding the search space. The
///   spec §4.2 reference of T=0.7 predates the empirical sweep.
/// - **Clarify / Route / Rank (`0.0`)**: deterministic JSON shape is
///   required so downstream phases (gate, propose, deliver) can rely
///   on a stable contract. Variance in the brief breaks every later
///   phase.
/// - **Gate (`0.0`)**: validation is mechanical; no randomness allowed.
/// - **Judge (`0.2`)**: rubric consistency matters more than novelty;
///   a small amount of variance lets independent judges diverge
///   productively on borderline scores.
/// - **Propose / Critique / Repair / Deliver (`0.4`)**: prose-heavy
///   roles benefit from some variance to escape repetition, but the
///   output must still parse against the schema. The JSON repair
///   walker (`phases/util.rs`) absorbs the occasional drift.
/// - **Intake (`0.4`)**: rephrasing the user prompt needs some
///   variance but should remain faithful.
///
/// A future release will let providers override these defaults through
/// the per-role `prompts/registry.rs` configuration block, but the
/// values here are the contract.
fn temperature_for_role(role: Role) -> f32 {
    match role {
        Role::Intake => 0.4,
        Role::Clarify => 0.0,
        Role::Route => 0.0,
        Role::Sketch => 1.0,
        Role::Propose => 0.4,
        Role::Gate => 0.0,
        Role::Critique => 0.4,
        Role::Repair => 0.4,
        Role::Judge => 0.2,
        Role::Rank => 0.0,
        Role::Deliver => 0.4,
        // Discovery (Plan B sub-phase B). The tagger is
        // deterministic; the extractor and integrator balance
        // variance against prose coherence.
        Role::Tagger => 0.0,
        Role::Extractor => 0.4,
        Role::Integrator => 0.4,
        // Phase D: synthesizer balances prose fluency (0.4) against
        // the integrator contract; adversary is fully deterministic
        // (0.0) so re-runs of the same evaluations produce identical
        // score_deltas — useful for snapshot tests.
        Role::Synthesizer => 0.4,
        Role::Adversary => 0.0,
        // Phase G: decomposer T=0.3 per T01-06 §4.2. The model
        // emits a structured DAG; a small amount of variance is
        // useful when the brief admits multiple valid
        // decompositions, but the cycle-detection guard in
        // `ProblemGraph::topological_layers` rejects anything that
        // doesn't form a valid DAG.
        Role::Decomposer => 0.3,
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
    /// Phase D: a list of `cluster_proposals/cp_*.json` files.
    ClusterProposals(Vec<PathBuf>),
    /// Phase D: a list of `synthesized/s_*.json` files.
    Synthesized(Vec<PathBuf>),
    /// Phase D: a list of `adversaries/p_*.json` files. Empty when
    /// the panel of judges agreed and the adversary never fired.
    Adversaries(Vec<PathBuf>),
    /// Phase G: `problem_graph.json` was written. Empty path means
    /// the phase was skipped or short-circuited to a trivial graph.
    ProblemGraph(PathBuf),
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

    /// Sketch ships T=1.0 because empirical sweeps showed that
    /// maximises semantic spread across the angle-cycled fan-out.
    /// Pinning the value here so a future change cannot silently
    /// reduce the diversity that v0.2's `SketchPhase` relies on.
    #[test]
    fn temperature_sketch_is_one() {
        assert_eq!(temperature_for_role(Role::Sketch), 1.0);
    }

    /// Roles whose output is consumed by JSON parsers downstream
    /// (clarify, route, rank, gate) MUST be deterministic. A
    /// non-zero temperature on any of these risks schema drift that
    /// the repair walker cannot recover from.
    #[test]
    fn temperature_deterministic_roles_are_zero() {
        assert_eq!(temperature_for_role(Role::Clarify), 0.0);
        assert_eq!(temperature_for_role(Role::Route), 0.0);
        assert_eq!(temperature_for_role(Role::Gate), 0.0);
        assert_eq!(temperature_for_role(Role::Rank), 0.0);
    }

    /// Every role must have a defined temperature so the helper is
    /// total and exhaustive over `Role::all()`.
    #[test]
    fn temperature_defined_for_every_role() {
        for r in Role::all() {
            let t = temperature_for_role(*r);
            assert!(
                (0.0..=2.0).contains(&t),
                "{r:?} temperature {t} outside [0, 2]"
            );
        }
    }
}
