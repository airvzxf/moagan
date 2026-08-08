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
use tokio::task::JoinHandle;

use crate::cancel::Cancel;
use crate::cli::Mode;
use crate::config::Config;
use crate::context::ContextRefRecord;
use crate::domain::{JsonRepairV2Report, LineagePaths};
use crate::error::{Error, Result};
use crate::error_code::ErrorCode;
use crate::execution::Parallelism;
use crate::fs_layout::{MoaganHome, RunDir};
use crate::ids::RunId;
use crate::llm::cache::{Cache, CacheConfig};
use crate::llm::prompt_cache::PromptCache;
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
    pub cache: Arc<Cache>,
    /// D.6.4: in-process index over the cross-run cache, keyed by a
    /// stable `(role, cache_key)` `prompt_id`. Consulted before the
    /// content-hash cache lookup so an in-flight repeat of the
    /// same call short-circuits without recomputing the hash.
    /// `Arc<parking_lot::Mutex<...>>` so `RunContext` stays cheaply
    /// cloneable across phases and the lock lives as long as the
    /// last `RunContext` handle. The critical section is just a
    /// `HashMap::get`/`HashMap::insert`.
    pub prompt_cache: Arc<parking_lot::Mutex<PromptCache>>,
    /// Loaded `Config` for the run. Phases that take user-tunable
    /// knobs (gate forbidden-techs / min / max length, validate
    /// sandbox timeout, etc.) MUST read from this field instead of
    /// `Config::defaults()` so changes in `~/.config/moagan/config.toml`
    /// actually take effect. Cloned at the `cli::run` boundary and
    /// shared across every phase via `Arc` so the cost is one
    /// `clone()` of the (small) struct.
    pub config: Arc<Config>,
    /// Whether the human-in-the-loop checkpoints are interactive
    /// (`true`) or auto-suppressed (`false`). Phase D opt-out;
    /// wired from `--non-interactive` and `Mode::Batch`.
    pub interactive: bool,
    /// Phase J: the verbatim context block to prepend to the
    /// intake LLM prompt when `--context <ref>` was used. `None`
    /// for runs that stand on their own.
    pub context_block: Option<String>,
    /// Phase J: the parent run id when `--context <run_id>` was
    /// used (or when this is a `rerun` of an existing run).
    /// `None` for root runs.
    pub parent_run_id: Option<RunId>,
    /// Phase J: SHA-256 over the canonical concatenation of the
    /// context texts that fed into `context_block`. Mirrored into
    /// the SQLite `runs.shared_brief_hash` column.
    pub shared_brief_hash: Option<String>,
    /// Phase J: per-file hashes + byte counts for every context
    /// reference. Mirrored into `run_context_refs`.
    pub context_refs: Vec<ContextRefRecord>,
    /// Phase J: filesystem locations the lineage walked through.
    /// Used by `moagan rerun` to recover the parent dir.
    pub lineage_paths: Option<LineagePaths>,
    cancel: Cancel,
    phase_timeout: Duration,
    total_timeout: Duration,
    /// Interval between lease renewals issued by the heartbeat task.
    /// Default: 30 s. Phases can override via
    /// [`RunContext::with_heartbeat_interval_secs`]; tests that need a
    /// tight loop use a much smaller value (e.g. 50 ms).
    heartbeat_interval_secs: u64,
    /// Holder identity used by the lease the heartbeat renews.
    /// Distinct per run so a paused/resumed run does not collide with
    /// the original heartbeat on the same `run_id`.
    heartbeat_holder: String,
    /// Handle for the lease-renewal heartbeat task spawned by
    /// [`Pipeline::run`](crate::phases::pipe::Pipeline::run). Held
    /// in an `Arc` so the various `RunContext` clones used by the
    /// pipeline share one slot; `Drop` aborts the handle so the
    /// heartbeat cannot outlive the run context.
    heartbeat_handle: Arc<parking_lot::Mutex<Option<JoinHandle<Result<u64>>>>>,
}

/// Default heartbeat interval. Renews the lease well before the
/// 60-second default TTL so a transient `db.renew_lease` failure
/// has a recovery window before the lease expires and the run is
/// flagged as a zombie.
pub const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 30;

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
        Self::new_with_config(
            run_id,
            home,
            providers,
            default_provider,
            default_model,
            parallelism,
            telemetry,
            raw_prompt,
            mode,
            Arc::new(Config::default()),
        )
    }

    /// Build a new run context with an explicit `Config`. Preferred
    /// over `new()` from the `cli::run` boundary so user-tunable
    /// knobs (gate forbidden-techs, validate sandbox timeout, etc.)
    /// reach the phases that consume them.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_config(
        run_id: RunId,
        home: Arc<MoaganHome>,
        providers: Arc<ProviderRegistry>,
        default_provider: String,
        default_model: String,
        parallelism: Parallelism,
        telemetry: Telemetry,
        raw_prompt: String,
        mode: String,
        config: Arc<Config>,
    ) -> Self {
        let cache = Arc::new(Cache::new(CacheConfig {
            root: home.cross_run_cache_dir(),
            cross_run: true,
            ..Default::default()
        }));
        let prompt_cache = Arc::new(parking_lot::Mutex::new(PromptCache::new(Arc::clone(
            &cache,
        ))));
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
            prompt_cache,
            config,
            interactive: true,
            context_block: None,
            parent_run_id: None,
            shared_brief_hash: None,
            context_refs: Vec::new(),
            lineage_paths: None,
            cancel: Cancel::new(),
            phase_timeout: Duration::ZERO,
            total_timeout: Duration::ZERO,
            heartbeat_interval_secs: DEFAULT_HEARTBEAT_INTERVAL_SECS,
            heartbeat_holder: "heartbeat".to_owned(),
            heartbeat_handle: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    /// Toggle the human-checkpoint interactivity. `false` makes
    /// every checkpoint a no-op that persists a `<skipped:non_interactive>`
    /// marker instead of blocking on stdin.
    pub fn with_interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    /// Phase J: attach the context block + lineage fields that
    /// `moagan run --context <ref>` pre-computes before the
    /// pipeline starts. The intake phase prepends `context_block`
    /// to the LLM prompt and the manifest persists the rest.
    #[allow(clippy::too_many_arguments)]
    pub fn with_context(
        mut self,
        context_block: Option<String>,
        parent_run_id: Option<RunId>,
        shared_brief_hash: Option<String>,
        context_refs: Vec<ContextRefRecord>,
        lineage_paths: Option<LineagePaths>,
    ) -> Self {
        self.context_block = context_block;
        self.parent_run_id = parent_run_id;
        self.shared_brief_hash = shared_brief_hash;
        self.context_refs = context_refs;
        self.lineage_paths = lineage_paths;
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

    /// Override the heartbeat renewal interval. Defaults to
    /// [`DEFAULT_HEARTBEAT_INTERVAL_SECS`] (30 s); tests that need a
    /// tight renewal loop pass a sub-second value so the integration
    /// suite can observe multiple renewals inside a short pipeline
    /// run. The interval is fixed at `ensure_heartbeat()` time —
    /// changing it after the heartbeat has been spawned has no
    /// effect on the live task.
    pub fn with_heartbeat_interval_secs(mut self, secs: u64) -> Self {
        self.heartbeat_interval_secs = secs;
        self
    }

    /// Override the holder identity used by the heartbeat's lease.
    /// Distinct per run so a paused/resumed run does not collide
    /// with the original heartbeat on the same `run_id`. Defaults
    /// to `"heartbeat"`.
    pub fn with_heartbeat_holder(mut self, holder: impl Into<String>) -> Self {
        self.heartbeat_holder = holder.into();
        self
    }

    /// True if the lease-renewal heartbeat has been spawned. Used by
    /// the pipeline tests to assert that
    /// [`Pipeline::run`](crate::phases::pipe::Pipeline::run) wired
    /// the task correctly.
    #[allow(dead_code)] // test-only assertion; production never inspects the slot
    pub(crate) fn heartbeat_spawned(&self) -> bool {
        self.heartbeat_handle.lock().is_some()
    }

    /// Spawn the lease-renewal heartbeat task. Idempotent: a second
    /// call without an intervening abort is a no-op so the pipeline
    /// can call it on every `run` without worrying about double
    /// spawns. Acquires a [`LeaseGuard`](crate::storage::lease::LeaseGuard)
    /// via the SQLite index, so this is a no-op when the telemetry
    /// was opened in no-index mode (legacy runs, the dashboard's
    /// read-only path). The spawned task is parented to a child of
    /// [`RunContext::cancel`] so `phase`/`total` timeout and the
    /// CLI shutdown signal all stop the heartbeat cleanly.
    pub(crate) fn ensure_heartbeat(&self) -> Result<()> {
        if self.heartbeat_handle.lock().is_some() {
            return Ok(());
        }
        let Some(db) = self.telemetry.db().cloned() else {
            return Ok(());
        };
        let holder = self.heartbeat_holder.clone();
        // The lease TTL is the renewal budget — it must outlive the
        // longest gap between renewals. Floor at 60 s so a tight
        // 100 ms interval still has plenty of headroom in case the
        // SQLite write stalls; cap at the configured interval so
        // very large intervals don't drag expiry past the next run.
        let ttl_secs = self.heartbeat_interval_secs.max(60);
        let lease = crate::storage::lease::LeaseGuard::acquire(
            &db,
            self.run_id,
            &holder,
            Duration::from_secs(ttl_secs),
        )?;
        // `tokio::time::interval` panics on zero, so floor at 1 s.
        // A user that explicitly wants "as fast as possible" gets
        // a 1-second interval; for tight loops the unit tests
        // override `heartbeat_interval_secs` directly via the
        // builder.
        let interval_secs = self.heartbeat_interval_secs.max(1);
        let interval = Duration::from_secs(interval_secs);
        let cancel = self.cancel.child_token();
        let handle = crate::telemetry::heartbeat::spawn(lease, interval, cancel);
        *self.heartbeat_handle.lock() = Some(handle);
        Ok(())
    }

    /// Abort the heartbeat task and clear the handle slot. Called
    /// from `Drop` so the heartbeat cannot outlive the run context.
    /// Best-effort: the abort is fire-and-forget because we cannot
    /// `.await` from a `Drop` impl. The task is parented to the
    /// `CancellationToken` so it exits promptly once the run context
    /// is gone.
    pub(crate) fn abort_heartbeat(&self) {
        if let Some(handle) = self.heartbeat_handle.lock().take() {
            handle.abort();
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
        let profile_overrides: Option<&std::collections::HashMap<String, f32>> =
            if self.config.profile_temperature_overrides.is_empty() {
                None
            } else {
                Some(&self.config.profile_temperature_overrides)
            };
        let req = Request {
            role,
            model: self.default_model.clone(),
            system,
            user,
            max_tokens: max_tokens_for_role(role),
            temperature: Some(temperature_for_role(role, profile_overrides)),
            top_p: Some(0.95),
            response_schema: None,
            stream: false,
        };
        let cache_key = Cache::cache_key(&req, &self.default_provider, &self.default_model);
        let started_unix = crate::time::now_unix_secs();
        // D.6.4: consult the prompt cache first. The `prompt_id` is
        // `(role, cache_key)` so distinct calls with the same role
        // but different inputs do not collide on the index. The
        // underlying content-hash cache still owns durability; the
        // prompt cache is a hot-path shortcut.
        let prompt_id = format!("{}@{}", role.as_str(), cache_key);
        if let Some(entry) = self.prompt_cache.lock().lookup_by_id(&prompt_id) {
            return self.record_cache_hit(entry, role, &cache_key, started_unix);
        }
        if let Some(entry) = self.cache.lookup(&cache_key)? {
            self.prompt_cache
                .lock()
                .register(&prompt_id, cache_key.clone());
            return self.record_cache_hit(entry, role, &cache_key, started_unix);
        }
        self.dispatch_to_provider(req, Some(cache_key.clone()), started_unix)
            .await
            .inspect(|_response| {
                self.prompt_cache.lock().register(&prompt_id, cache_key);
            })
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
        let profile_overrides: Option<&std::collections::HashMap<String, f32>> =
            if self.config.profile_temperature_overrides.is_empty() {
                None
            } else {
                Some(&self.config.profile_temperature_overrides)
            };
        let req = Request {
            role,
            model: self.default_model.clone(),
            system,
            user,
            max_tokens: max_tokens_for_role(role),
            temperature: Some(temperature_for_role(role, profile_overrides)),
            top_p: Some(0.95),
            response_schema: None,
            stream: false,
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

    async fn call_json_repair_v2<T>(&self, role: Role, raw: &str, schema_hint: &str) -> Option<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let user = serde_json::json!({
            "malformed": raw,
            "target_role": role.as_str(),
            "target_schema": schema_hint,
        })
        .to_string();
        let response = self
            .call_uncached(
                Role::JsonRepairV2,
                crate::llm::prompts::system_prompt(Role::JsonRepairV2).to_owned(),
                user,
                crate::time::now_unix_secs(),
            )
            .await
            .ok()?;
        let report = serde_json::from_str::<JsonRepairV2Report>(&response.text).ok();
        let repaired = report
            .as_ref()
            .map(|value| value.repaired.as_str())
            .filter(|value| !value.trim().is_empty())
            .or_else(|| (!response.text.trim().is_empty()).then_some(response.text.as_str()))?;
        let value = serde_json::from_str::<serde_json::Value>(repaired).ok()?;
        role.validate_json(&value).ok()?;
        serde_json::from_value(value).ok()
    }

    /// Call the model and parse the response, retrying the call up to
    /// `max_retries` additional times if the parse fails. Each attempt
    /// goes through the normal pipeline (provider send + telemetry +
    /// bracket-repair + validator), so retries show up in the call-level
    /// metrics just like any other LLM call.
    ///
    /// The retry cap is now driven by
    /// [`crate::llm::retry_budget::budget_for`] (D.21.6). The
    /// `max_retries` argument is kept as a **hard ceiling**: the
    /// loop's actual cap is `min(budget.max_attempts, max_retries + 1)`.
    /// In practice this means:
    ///
    /// - `Fast`, `Explore`, `Batch`: exactly 1 attempt regardless
    ///   of `max_retries` (the budget wins).
    /// - `Standard` transport / rate-limit / timeout / truncated:
    ///   2 attempts (was 6 with the legacy hard-coded cap).
    /// - `Standard` parse / schema: 1 attempt with the JSON repair
    ///   pass; the repair runs inline in `parse_model_json` so the
    ///   budget's `use_json_repair` flag is reported on the
    ///   warning payload but not re-applied.
    /// - `Deep` rate-limit: 3 attempts.
    /// - `Deep` parse / schema: 2 attempts with repair.
    ///
    /// Callers that explicitly want more attempts (e.g. tests that
    /// exercise the retry loop with a 6-deep mock queue) can pass
    /// a larger `max_retries`; the ceiling is a safety net, not a
    /// guarantee.
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
        use crate::llm::retry_budget::{self, RetryBudget, RetryReason};

        let phase_name = role.as_str();
        let mode = parse_mode_str(&self.mode);
        // The legacy `max_retries` parameter is now a hard ceiling.
        // The actual cap is `min(budget.max_attempts, ceiling + 1)`.
        let ceiling = max_retries;

        // The budget is computed on the FIRST error (see the
        // `unwrap_or_else` below) and then frozen for the rest of
        // the loop. Mixing budgets across attempts would surface a
        // different `RetryReason` mid-loop, which would confuse
        // the post-execution review and produce inconsistent
        // telemetry for what is logically the same failure.
        let mut budget: Option<RetryBudget> = None;
        let mut attempt: u32 = 0;

        loop {
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

            let decision = match response {
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
                    Err(e) => {
                        // `parse_model_json` always wraps its
                        // failure in `Error::SchemaViolation`, so we
                        // re-check the raw payload: parseable as a
                        // generic `serde_json::Value` means the
                        // failure is a schema mismatch (the JSON
                        // shape did not match the role's contract);
                        // not parseable means the failure is a
                        // genuine parse error.
                        let reason =
                            if serde_json::from_str::<serde_json::Value>(&resp.text).is_ok() {
                                RetryReason::Schema
                            } else {
                                RetryReason::Parse
                            };
                        let b = budget.unwrap_or_else(|| retry_budget::budget_for(mode, reason));
                        budget = Some(b);
                        let max_attempts = b.max_attempts.min(ceiling + 1);
                        if attempt + 1 >= max_attempts {
                            if b.use_json_repair
                                && self.config.llm.json_repair_v2_enabled
                                && let Some(value) = self
                                    .call_json_repair_v2::<T>(role, &resp.text, schema_hint)
                                    .await
                            {
                                return Ok(value);
                            }
                            return Err(e);
                        }
                        let _ = self.telemetry.warn(
                            "model.retry_parse",
                            "warn",
                            "model response parse failed; retrying",
                            serde_json::json!({
                                "attempt": attempt + 1,
                                "max_attempts": max_attempts,
                                "use_json_repair": b.use_json_repair,
                                "error": e.to_string(),
                            }),
                            warn_ctx(),
                        );
                        Decision::Retry
                    }
                },
                Err(e) => {
                    if !should_retry_error(&e) {
                        tracing::debug!(error = ?e, code = ?e.code(), "non-retriable error, bailing out");
                        return Err(e);
                    }
                    let reason = retry_budget::reason_from_error(&e);
                    let b = budget.unwrap_or_else(|| retry_budget::budget_for(mode, reason));
                    budget = Some(b);
                    let max_attempts = b.max_attempts.min(ceiling + 1);
                    if attempt + 1 >= max_attempts {
                        return Err(e);
                    }
                    let _ = self.telemetry.warn(
                        "model.retry_provider",
                        "warn",
                        "model call failed; retrying",
                        serde_json::json!({
                            "attempt": attempt + 1,
                            "max_attempts": max_attempts,
                            "use_json_repair": b.use_json_repair,
                            "error": e.to_string(),
                        }),
                        warn_ctx(),
                    );
                    Decision::Retry
                }
            };

            if matches!(decision, Decision::Retry) {
                attempt += 1;
            }
        }
    }
}

/// Abort the heartbeat task when the last `RunContext` clone goes
/// out of scope. Multiple clones share the same `Arc<Mutex<Option<...>>>`
/// handle, so the first clone to drop takes the handle out and the
/// rest see `None` (a no-op). The task itself is also parented to a
/// `CancellationToken`, so even an aborted-but-leaked task exits
/// when the run's cancel fires; this `Drop` just guarantees the
/// task stops before the run's `LeaseGuard` releases its row.
impl Drop for RunContext {
    fn drop(&mut self) {
        self.abort_heartbeat();
    }
}

fn error_code_is_retriable(code: Option<ErrorCode>) -> bool {
    code.map(|c| c.is_retriable()).unwrap_or(true)
}

fn should_retry_error(err: &Error) -> bool {
    error_code_is_retriable(Some(err.code()))
        || matches!(err, Error::Provider(_) | Error::PlanExhausted(_))
}

/// Internal loop-control flag for `call_with_retry_parse`. The
/// loop only ever needs two states — retry or fall through — but
/// naming the case makes the match arms above easier to read and
/// reserves room for a future "abort" branch (e.g. cancellation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// Schedule the next attempt; the per-attempt error has been
    /// reported through the warnings stream.
    Retry,
}

/// Map the run's mode string (the same value stored in the
/// manifest) to the [`Mode`] enum so the retry budget lookup is
/// type-driven. Mirrors `cli::run::parse_mode`; duplicated here
/// because the canonical helper is `pub(crate)` to the cli
/// module and the retry loop lives in `phases::phase`. Unknown
/// strings fall back to `Mode::Standard` (the documented default)
/// so a corrupted manifest cannot crash the pipeline.
fn parse_mode_str(s: &str) -> Mode {
    match s {
        "fast" => Mode::Fast,
        "standard" => Mode::Standard,
        "deep" => Mode::Deep,
        "explore" => Mode::Explore,
        "batch" => Mode::Batch,
        _ => Mode::Standard,
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
        // Reasoning models (DeepSeek v4, qwen3.x, kimi) consume
        // 400-700 tokens on chain-of-thought before the JSON
        // envelope. 1024 was enough for MiniMax M3 but truncates
        // reasoning models mid-string. 2048 covers the
        // reasoning+envelope for both families.
        Role::Intake => 2048,
        Role::Clarify => 2048,
        Role::Route => 2048,
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
        // T01-06 §4.2: facet_deriver has a 1024-token budget — larger
        // than tagger's 512 because the output carries 3-6 facet
        // triples (name + description + required).
        Role::FacetDeriver => 1024,
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
        Role::MergeSynthesizer => 4000,
        Role::RecoveryExplainer => 1000,
        Role::RationaleExtractor => 1500,
        // Track H batch-1: D.7.1 catalog opt-in roles. Each carries
        // its own sampling contract (see `role_settings` in
        // `src/llm/prompts.rs`); these are the runtime ceilings
        // used when the catalog role is invoked outside any phase.
        Role::TiefighterCritic => 2048,
        // persona_picker is a short routing decision; the 512-token
        // ceiling matches its role_settings (see prompts.rs).
        Role::PersonaPicker => 512,
        // angle_picker is a routing decision with a one-line
        // rationale; the 1024-token ceiling matches role_settings.
        Role::AnglePicker => 1024,
        // Track H batch-2: tiebreaker ceiling per D.7.1 (1536).
        Role::FinalDisagreement => 1536,
        // Track H batch-2 (commit 2): LLM re-call for malformed
        // JSON. 1024 is enough to carry a repaired payload plus
        // the audit fields.
        Role::JsonRepairV2 => 1024,
        // Track H batch-2 (commit 3): prompt-injection guard. The
        // 512-token ceiling matches the role contract (verdict +
        // reasons + recommended_action).
        Role::HostilePromptDetector => 512,
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
fn temperature_for_role(
    role: Role,
    profile_overrides: Option<&std::collections::HashMap<String, f32>>,
) -> f32 {
    // Profile-defined overrides (spec D.6 / D.21.x) take precedence
    // over the hard-coded defaults when the active domain profile
    // maps the role name to a different sampling temperature.
    // When the profile map is absent or lacks an entry for this
    // role we fall through to the role-specific hard-coded value.
    if let Some(map) = profile_overrides
        && let Some(v) = map.get(role.as_str())
    {
        return *v;
    }
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
        // FacetDeriver is deterministic for cache stability (the
        // facet list feeds `sha256(brief + category_id)`).
        Role::FacetDeriver => 0.0,
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
        Role::MergeSynthesizer => 0.2,
        Role::RecoveryExplainer => 0.0,
        Role::RationaleExtractor => 0.2,
        // Track H batch-1: TiefighterCritic is fully deterministic
        // (T=0.0) per D.7.1 so re-runs against the same proposal
        // produce identical critiques (useful for snapshot diffs).
        Role::TiefighterCritic => 0.0,
        // persona_picker needs a small amount of variance
        // (T=0.3) to break ties between close candidates without
        // flipping picks across runs of the same brief.
        Role::PersonaPicker => 0.3,
        // angle_picker runs at T=0.7 so the picker escapes the
        // obvious angles and surfaces the *next* one; the high
        // variance is intentional, not noise.
        Role::AnglePicker => 0.7,
        // Track H batch-2: tiebreaker stays low (T=0.2) so re-runs
        // of the same disagreement yield identical winner picks,
        // which is what callers diff when they replay a cluster.
        Role::FinalDisagreement => 0.2,
        // Track H batch-2 (commit 2): JsonRepairV2 is fully
        // deterministic (T=0.0) so re-runs against the same
        // malformed text produce identical repairs.
        Role::JsonRepairV2 => 0.0,
        // Track H batch-2 (commit 3): HostilePromptDetector is
        // fully deterministic (T=0.0) so two detectors on the
        // same input agree — false negatives in the quarantine
        // path are unacceptable.
        Role::HostilePromptDetector => 0.0,
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
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RetryScript {
        outcomes: parking_lot::Mutex<VecDeque<Result<(u16, Response)>>>,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::llm::Provider for RetryScript {
        fn name(&self) -> &str {
            "retry-script"
        }

        fn model(&self) -> &str {
            "retry-model"
        }

        fn endpoint(&self) -> &str {
            "mock://retry"
        }

        async fn send(&self, _req: &Request) -> Result<(u16, Response)> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.outcomes
                .lock()
                .pop_front()
                .unwrap_or(Err(Error::MockExhausted))
        }
    }

    fn retry_context(
        outcomes: Vec<Result<(u16, Response)>>,
    ) -> (tempfile::TempDir, RunContext, Arc<RetryScript>) {
        let temp = tempfile::tempdir().unwrap();
        let home = Arc::new(MoaganHome::at(temp.path().to_path_buf()));
        home.ensure().unwrap();
        let run_id = RunId::new();
        let telemetry = Telemetry::open(
            run_id,
            &home.run_dir(run_id),
            crate::redact::RedactPolicy::default(),
            None,
        )
        .unwrap();
        let script = Arc::new(RetryScript {
            outcomes: parking_lot::Mutex::new(outcomes.into()),
            calls: AtomicUsize::new(0),
        });
        let mut registry = ProviderRegistry::default();
        registry.insert("retry".into(), script.clone());
        let ctx = RunContext::new(
            run_id,
            home,
            Arc::new(registry),
            "retry".into(),
            "retry-model".into(),
            Parallelism::new(1),
            telemetry,
            String::new(),
            "standard".into(),
        );
        (temp, ctx, script)
    }

    fn response(text: &str) -> Response {
        Response {
            text: text.into(),
            finish_reason: Some("end_turn".into()),
            truncated: false,
            usage: Default::default(),
        }
    }

    #[tokio::test]
    async fn call_with_retry_parse_bails_on_non_retriable_error() {
        let (_temp, ctx, script) = retry_context(vec![Err(Error::InvalidApiKey("bad".into()))]);
        let result = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                String::new(),
                String::new(),
                "Value",
                5,
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidApiKey(_))));
        assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn call_with_retry_parse_retries_on_retriable_error() {
        let (_temp, ctx, script) = retry_context(vec![
            Err(Error::Provider("temporary outage".into())),
            Ok((200, response(r#"{"ok":true}"#))),
        ]);
        let result = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                String::new(),
                String::new(),
                "Value",
                5,
            )
            .await
            .unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(script.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn json_repair_v2_succeeds_after_m3_fails() {
        let (_temp, mut ctx, script) = retry_context(vec![
            Ok((200, response(r#"{"broken":"unterminated}"#))),
            Ok((
                200,
                response(
                    r#"{"malformed":"broken","target_schema":"intake","repaired":"{\"ok\":true}","notes":"fixed","schema_version":"json_repair_v2.v1"}"#,
                ),
            )),
        ]);
        Arc::get_mut(&mut ctx.config)
            .unwrap()
            .llm
            .json_repair_v2_enabled = true;
        let result = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                String::new(),
                String::new(),
                "Intake: {problem, objectives[]}",
                5,
            )
            .await
            .unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(script.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn json_repair_v2_falls_back_to_original_on_no_response() {
        let (_temp, mut ctx, script) = retry_context(vec![
            Ok((200, response(r#"{"broken":"unterminated}"#))),
            Ok((200, response(""))),
        ]);
        Arc::get_mut(&mut ctx.config)
            .unwrap()
            .llm
            .json_repair_v2_enabled = true;
        let result = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                String::new(),
                String::new(),
                "Intake: {problem, objectives[]}",
                5,
            )
            .await;
        assert!(matches!(result, Err(Error::SchemaViolation(_))));
        assert_eq!(script.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn json_repair_v2_disabled_by_default() {
        let (_temp, ctx, script) = retry_context(vec![
            Ok((200, response(r#"{"broken":"unterminated}"#))),
            Ok((200, response(r#"{"ok":true}"#))),
        ]);
        assert!(!ctx.config.llm.json_repair_v2_enabled);
        let result = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                String::new(),
                String::new(),
                "Intake: {problem, objectives[]}",
                5,
            )
            .await;
        assert!(result.is_err());
        assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn call_with_retry_parse_handles_error_without_code_as_retriable() {
        assert!(error_code_is_retriable(None));
    }

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
        assert_eq!(temperature_for_role(Role::Sketch, None), 1.0);
    }

    /// Roles whose output is consumed by JSON parsers downstream
    /// (clarify, route, rank, gate) MUST be deterministic. A
    /// non-zero temperature on any of these risks schema drift that
    /// the repair walker cannot recover from.
    #[test]
    fn temperature_deterministic_roles_are_zero() {
        assert_eq!(temperature_for_role(Role::Clarify, None), 0.0);
        assert_eq!(temperature_for_role(Role::Route, None), 0.0);
        assert_eq!(temperature_for_role(Role::Gate, None), 0.0);
        assert_eq!(temperature_for_role(Role::Rank, None), 0.0);
    }

    /// Every role must have a defined temperature so the helper is
    /// total and exhaustive over `Role::all()`.
    #[test]
    fn temperature_defined_for_every_role() {
        for r in Role::all() {
            let t = temperature_for_role(*r, None);
            assert!(
                (0.0..=2.0).contains(&t),
                "{r:?} temperature {t} outside [0, 2]"
            );
        }
    }

    /// Profile-supplied temperature overrides take precedence over
    /// the hard-coded role default. Without an override the helper
    /// returns 1.0 for `Sketch`; with an override it returns the
    /// profile value. Pins the precedence contract from PR D6 so
    /// future refactors cannot silently route around it.
    #[test]
    fn profile_temperature_override_takes_precedence_over_role_default() {
        let mut map = std::collections::HashMap::new();
        map.insert("sketch".to_owned(), 0.25_f32);
        let overridden = temperature_for_role(Role::Sketch, Some(&map));
        assert_eq!(overridden, 0.25);
        let baseline = temperature_for_role(Role::Sketch, None);
        assert_eq!(baseline, 1.0);
    }

    /// When the profile map is absent OR does not contain an entry
    /// for the role, the helper returns the hard-coded default.
    /// Pins both the "no profile" branch and the "missing key"
    /// branch because callers rely on the silent fall-through.
    #[test]
    fn profile_without_overrides_falls_back_to_hardcoded() {
        let empty = std::collections::HashMap::<String, f32>::new();
        // Empty map → fall-through.
        assert_eq!(
            temperature_for_role(Role::Sketch, Some(&empty)),
            1.0,
            "empty profile map must fall through to hard-coded default"
        );
        // Map present but role missing → fall-through.
        let mut other_role = std::collections::HashMap::new();
        other_role.insert("judge".to_owned(), 0.99_f32);
        assert_eq!(
            temperature_for_role(Role::Sketch, Some(&other_role)),
            1.0,
            "profile without an entry for the role must fall through"
        );
        // Same value as the override, but for a different role —
        // sanity-check the lookup is keyed by `role.as_str()`.
        assert_eq!(
            temperature_for_role(Role::Judge, Some(&other_role)),
            0.99,
            "present entry must be returned when the role matches"
        );
    }

    /// `temperature_for_role` looks up by `Role::as_str()`. Unknown
    /// strings in the profile map are silently ignored (so an
    /// operator's typo in `temperature_overrides.toml` does not
    /// blow up every LLM call). This test pins the contract: the
    /// helper only honours keys that match a known role.
    #[test]
    fn profile_overrides_validate_role_names() {
        let mut map = std::collections::HashMap::new();
        // Bogus role names that no real `Role` variant produces.
        map.insert("not_a_role".to_owned(), 0.1_f32);
        map.insert("SkEtCh".to_owned(), 0.5_f32); // case-sensitive too
        assert_eq!(
            temperature_for_role(Role::Sketch, Some(&map)),
            1.0,
            "case-mismatched key must not override"
        );
        assert_eq!(
            temperature_for_role(Role::Clarify, Some(&map)),
            0.0,
            "unknown key must not influence unrelated roles"
        );
    }
}
