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
use crate::llm::capability::CapabilityResolver;
use crate::llm::probe_table::MaxTokensTable;
use crate::llm::prompt_cache::PromptCache;
use crate::llm::prompts::DEFAULT_MAX_TOKENS;
use crate::llm::response_format_opt_out::render_system_prompt_with_prefix;
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
    /// Shared handle into the auto-probe `max_tokens` table. When
    /// `Some`, [`Self::dispatch_to_provider`] consults the table
    /// before clamping `req.max_tokens` to the per-provider TOML
    /// cap so the wire body's `max_tokens` reflects the discovered
    /// upstream ceiling (the primary source of truth) and falls
    /// back to the per-role default (1,000,000) only when the
    /// table has no entry for `(default_provider, default_model)`.
    /// `None` when the run was started without auto-probing (legacy
    /// paths and tests); in that case the clamp reduces to the
    /// per-provider TOML value alone.
    pub max_tokens_table: Option<Arc<MaxTokensTable>>,
    /// PR-3: capability resolver consulted on every LLM call so the
    /// models.dev catalog can drop fields the upstream would reject
    /// (e.g. `temperature` on `kimi-k3`). `None` disables every
    /// capability-aware gate and keeps the legacy "send everything"
    /// behaviour — the same fallback the `max_tokens_table` field
    /// uses. The CLI boundary (`cli::run`) and the integration
    /// tests populate this field; unit tests that exercise the
    /// pre-capability behaviour leave it as `None`.
    pub capability_resolver: Option<Arc<CapabilityResolver>>,
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
            max_tokens_table: None,
            capability_resolver: None,
        }
    }

    /// Attach the auto-probe `max_tokens` table so
    /// [`Self::dispatch_to_provider`] can consult it before clamping
    /// `req.max_tokens`. The CLI boundary (`cli::run`) and the
    /// integration tests call this once after `registry_from_config`
    /// has built the table; tests that exercise the per-provider
    /// TOML cap in isolation leave the field as `None` so the
    /// pre-table behaviour stays bit-for-bit.
    pub fn with_max_tokens_table(mut self, table: Arc<MaxTokensTable>) -> Self {
        self.max_tokens_table = Some(table);
        self
    }

    /// Optional variant of [`Self::with_max_tokens_table`] for the
    /// `Option<Arc<MaxTokensTable>>` carried by [`ProviderRegistry`].
    /// No-op when the table is `None` (mock-only registries and
    /// environments where `max_token_auto = None` everywhere).
    pub fn with_max_tokens_table_opt(mut self, table: Option<Arc<MaxTokensTable>>) -> Self {
        if let Some(t) = table {
            self.max_tokens_table = Some(t);
        }
        self
    }

    /// PR-3: attach a [`CapabilityResolver`] so every LLM call goes
    /// through the capability gate before the wire body is built.
    /// Builder form mirrors [`Self::with_max_tokens_table`] so the
    /// CLI boundary (`cli::run`) can chain it next to the auto-probe
    /// table without changing the rest of the construction order.
    pub fn with_capability_resolver(mut self, resolver: Arc<CapabilityResolver>) -> Self {
        self.capability_resolver = Some(resolver);
        self
    }

    /// PR-3: optional variant of [`Self::with_capability_resolver`]
    /// for callers that already hold an `Option<Arc<...>>`. No-op
    /// when the resolver is `None` so legacy call sites can keep
    /// passing `None` without branching.
    pub fn with_capability_resolver_opt(
        mut self,
        resolver: Option<Arc<CapabilityResolver>>,
    ) -> Self {
        if let Some(r) = resolver {
            self.capability_resolver = Some(r);
        }
        self
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

    /// Resolve the active provider by name. D.19.19: when the
    /// registry has a `ProviderPool` configured (e.g. the config
    /// carries two `mock` instances), the pool's round-robin is
    /// consulted first so consecutive calls land on different
    /// instances. When the pool is empty or returns `None` (e.g.
    /// every entry's breaker is open), the call falls back to a
    /// direct `get(default_provider)` so the legacy single-instance
    /// path stays bit-for-bit equivalent.
    pub async fn provider(&self) -> Arc<dyn crate::llm::Provider> {
        if self.providers.has_pool()
            && let Some(picked) = self.providers.pick(false).await
        {
            return picked;
        }
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
        self.call_with_retry(role, system, user, 0).await
    }

    /// Like [`Self::call`] but tags the resulting `calls` row with
    /// the supplied `retry_count`. The canonical retry loop in
    /// [`Self::call_with_retry_parse`] threads the current attempt
    /// index through this parameter so the JSONL/SQLite `calls` row
    /// mirrors the warnings stream's `attempt` value and the
    /// `retry_count` field every consumer (dashboard, audit,
    /// post-execution review) already aggregates.
    pub(crate) async fn call_with_retry(
        &self,
        role: Role,
        system: String,
        user: String,
        retry_count: u32,
    ) -> Result<Response> {
        let profile_overrides: Option<&std::collections::HashMap<String, f32>> =
            if self.config.profile_temperature_overrides.is_empty() {
                None
            } else {
                Some(&self.config.profile_temperature_overrides)
            };
        // PR-B2: per-provider temperature/top_p from the active
        // provider's `ProviderConfig`. These are consulted BEFORE the
        // hard-coded per-role table so a user who wrote
        // `[providers.minimax] temperature = 0.42` actually gets 0.42.
        let (provider_temperature, provider_top_p) = self
            .config
            .providers
            .get(&self.default_provider)
            .map(|s| (s.temperature, s.top_p))
            .unwrap_or((None, None));
        // PR-C6: for the static opt-out models (e.g. `glm-5.1`,
        // `kimi-k2.6`) the upstream `response_format: json_object`
        // flag is ignored, so the JSON contract rides entirely on
        // the prompt. Prepend a strong `CRITICAL OUTPUT CONTRACT`
        // header to the role's normal system prompt; non-stubborn
        // models get the role's prompt byte-for-byte (zero
        // behaviour change for the other ~15 providers).
        let system = render_system_prompt_with_prefix(&role, &self.default_model, &system);
        let req = Request {
            role,
            model: self.default_model.clone(),
            system,
            user,
            max_tokens: max_tokens_for_role(role),
            temperature: Some(resolve_temperature(
                role,
                profile_overrides,
                provider_temperature,
            )),
            top_p: Some(provider_top_p.unwrap_or(0.95)),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
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
            return self.record_cache_hit(entry, role, &cache_key, started_unix, retry_count);
        }
        if let Some(entry) = self.cache.lookup(&cache_key)? {
            self.prompt_cache
                .lock()
                .register(&prompt_id, cache_key.clone());
            return self.record_cache_hit(entry, role, &cache_key, started_unix, retry_count);
        }
        self.dispatch_to_provider(req, Some(cache_key.clone()), started_unix, retry_count)
            .await
            .inspect(|_response| {
                self.prompt_cache.lock().register(&prompt_id, cache_key);
            })
    }

    /// Like [`Self::call_with_retry`] but stamps the request with an
    /// EXPLICIT sampling temperature (bypassing
    /// [`crate::phases::phase::resolve_temperature`] / the per-role
    /// defaults / the active provider's `ProviderConfig::temperature`
    /// / `profile_temperature_overrides`).
    ///
    /// PR-D1: the discovery matrix phase iterates every `(cell,
    /// replica)` pair against a per-provider
    /// [`crate::discovery::matrix::TemperatureProfile`]; each profile
    /// carries an explicit list of temperatures the loop must walk,
    /// so the resolved temperature must come from the profile — not
    /// the role table. The cache key in [`crate::llm::cache::Cache`]
    /// includes the resolved temperature, so distinct temperatures
    /// cache distinctly and the per-cell temperature buckets form
    /// naturally (the audit confirmed this property; pinned here so
    /// the wire path stays consistent).
    ///
    /// `profile_overrides` (the active domain profile's per-role
    /// overrides) and the per-provider base temperature are still
    /// consulted when the matrix's profile defaults to a single-shot
    /// `[1.0] × 1`, but the explicit `temperature` parameter always
    /// wins — there is no upstream indirection.
    pub(crate) async fn call_with_retry_at_temp(
        &self,
        role: Role,
        system: String,
        user: String,
        retry_count: u32,
        temperature: f32,
    ) -> Result<Response> {
        let (_provider_temperature, provider_top_p) = self
            .config
            .providers
            .get(&self.default_provider)
            .map(|s| (s.temperature, s.top_p))
            .unwrap_or((None, None));
        let system = render_system_prompt_with_prefix(&role, &self.default_model, &system);
        let req = Request {
            role,
            model: self.default_model.clone(),
            system,
            user,
            max_tokens: max_tokens_for_role(role),
            temperature: Some(temperature),
            top_p: Some(provider_top_p.unwrap_or(0.95)),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let cache_key = Cache::cache_key(&req, &self.default_provider, &self.default_model);
        let started_unix = crate::time::now_unix_secs();
        let prompt_id = format!("{}@{}", role.as_str(), cache_key);
        if let Some(entry) = self.prompt_cache.lock().lookup_by_id(&prompt_id) {
            return self.record_cache_hit(entry, role, &cache_key, started_unix, retry_count);
        }
        if let Some(entry) = self.cache.lookup(&cache_key)? {
            self.prompt_cache
                .lock()
                .register(&prompt_id, cache_key.clone());
            return self.record_cache_hit(entry, role, &cache_key, started_unix, retry_count);
        }
        self.dispatch_to_provider(req, Some(cache_key.clone()), started_unix, retry_count)
            .await
            .inspect(|_response| {
                self.prompt_cache.lock().register(&prompt_id, cache_key);
            })
    }

    /// Provider-uncached variant of [`Self::call_with_retry_at_temp`]
    /// used by the discovery matrix's retry path (see
    /// `discover_matrix::retry_sketch_extraction`). Mirrors
    /// [`Self::call_uncached`] but stamps the explicit temperature
    /// instead of consulting `resolve_temperature`. The
    /// `retry_count` parameter tags the resulting `calls` row so
    /// the retry loop's attempt index survives into the JSONL /
    /// SQLite `calls.retry_count` column.
    pub(crate) async fn call_uncached_at_temp(
        &self,
        role: Role,
        system: String,
        user: String,
        started_unix: i64,
        retry_count: u32,
        temperature: f32,
    ) -> Result<Response> {
        let (_provider_temperature, provider_top_p) = self
            .config
            .providers
            .get(&self.default_provider)
            .map(|s| (s.temperature, s.top_p))
            .unwrap_or((None, None));
        let system = render_system_prompt_with_prefix(&role, &self.default_model, &system);
        let req = Request {
            role,
            model: self.default_model.clone(),
            system,
            user,
            max_tokens: max_tokens_for_role(role),
            temperature: Some(temperature),
            top_p: Some(provider_top_p.unwrap_or(0.95)),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        self.dispatch_to_provider(req, None, started_unix, retry_count)
            .await
    }

    /// Provider call without consulting the cache. Used on parse-
    /// failure retries (see `call_with_retry_parse`) so a previously
    /// cached broken response does not poison the retry. The
    /// `retry_count` parameter tags the resulting row so the retry
    /// loop's attempt index survives into the JSONL / SQLite
    /// `calls.retry_count` column.
    pub(crate) async fn call_uncached(
        &self,
        role: Role,
        system: String,
        user: String,
        started_unix: i64,
        retry_count: u32,
    ) -> Result<Response> {
        let profile_overrides: Option<&std::collections::HashMap<String, f32>> =
            if self.config.profile_temperature_overrides.is_empty() {
                None
            } else {
                Some(&self.config.profile_temperature_overrides)
            };
        // PR-B2: per-provider temperature/top_p from the active
        // provider's `ProviderConfig`. Mirrors the lookup in
        // `call_with_retry` so the cache-miss path is consistent.
        let (provider_temperature, provider_top_p) = self
            .config
            .providers
            .get(&self.default_provider)
            .map(|s| (s.temperature, s.top_p))
            .unwrap_or((None, None));
        // PR-C6: mirror the prefix injection from `call_with_retry`
        // so the parse-failure retry path (which bypasses the
        // cache) sends the same prompt as the original call. Without
        // this, retries against stubborn models would silently
        // re-emit the model-breaking output.
        let system = render_system_prompt_with_prefix(&role, &self.default_model, &system);
        let req = Request {
            role,
            model: self.default_model.clone(),
            system,
            user,
            max_tokens: max_tokens_for_role(role),
            temperature: Some(resolve_temperature(
                role,
                profile_overrides,
                provider_temperature,
            )),
            top_p: Some(provider_top_p.unwrap_or(0.95)),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        self.dispatch_to_provider(req, None, started_unix, retry_count)
            .await
    }

    /// Send the prepared request to the provider, record telemetry,
    /// emit truncation / empty warnings, and (when a `cache_key` is
    /// supplied) persist the response in the cross-run cache.
    async fn dispatch_to_provider(
        &self,
        req: Request,
        cache_key: Option<String>,
        started_unix: i64,
        retry_count: u32,
    ) -> Result<Response> {
        // Apply the same per-provider cap that the provider will apply
        // inside `send()`, so the body_sha256 matches the body that
        // actually leaves the process. Cloned here because `req` is
        // consumed by `provider.send(&req)` downstream and we don't
        // want to mutate the caller's view.
        //
        // We delegate the cap chain to `Provider::effective_max_tokens`
        // — the single source of truth that lives next to the
        // provider's own `send()` implementation. Re-deriving the chain
        // here bit us once already: PR #400 raised
        // `DEFAULT_MAX_TOKENS` to 1M and `minimax.send()` clamps via
        // `operator_cap.min(table_cap).min(MINIMAX_MAX_TOKENS_CAP =
        // 524_288)`, while this function used a 2-layer
        // `operator_cap.min(table_cap)` chain. Every `minimax` call
        // landed on the wire with `max_tokens = 524_288` while the
        // audit hash captured `max_tokens = 1_000_000`, and the proxy
        // verify step flagged every request as a body mismatch. The
        // trait method now keeps both ends in lockstep.
        let provider = self.provider().await;
        let effective_max = provider.effective_max_tokens(&req);
        // PR-3: gate the request through the capability resolver so
        // models whose catalog says `temperature: false` (e.g.
        // `kimi-k3`) do not receive the field on the wire. The gated
        // request is what `provider.send` actually transmits AND
        // what feeds into `request_body_sha256` below, so the
        // recorded audit hash matches the body that leaves the
        // process — same contract as the `max_tokens` clamp above.
        // `None` resolver preserves the legacy "send everything"
        // behaviour for hand-rolled tests and the mock-only path.
        let gated = match self.capability_resolver.as_ref() {
            Some(resolver) => resolver.gate_request(
                self.default_provider.as_str(),
                self.default_model.as_str(),
                &req,
            ),
            None => req.clone(),
        };
        let hash_input = if gated.max_tokens != effective_max {
            let mut clamped = gated;
            clamped.max_tokens = effective_max;
            clamped
        } else {
            gated
        };
        let request_body_sha256 = (self.default_provider == "minimax")
            .then(|| crate::llm::http::request_body_sha256(&hash_input))
            .transpose()?;
        let call_id = uuid::Uuid::now_v7().to_string();
        let provider_started = std::time::Instant::now();
        tracing::debug!(
            call_id = %call_id,
            phase = req.role.as_str(),
            stage = "provider.send.started",
            retry_count,
            "LLM call stage"
        );
        let result = provider.send(&req).await;
        tracing::debug!(
            call_id = %call_id,
            phase = req.role.as_str(),
            stage = "provider.send.completed",
            elapsed_ms = provider_started.elapsed().as_millis(),
            success = result.is_ok(),
            retry_count,
            "LLM call stage"
        );
        let ended_unix = crate::time::now_unix_secs();
        let phase_name = req.role.as_str();
        let ctx = || WarningContext {
            phase: Some(phase_name.to_owned()),
            role: Some(phase_name.to_owned()),
            call_id: Some(call_id.clone()),
            attempt: Some(retry_count),
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
                    retry_count,
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
                    retry_count,
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
        retry_count: u32,
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
            retry_count,
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
                    attempt: Some(retry_count),
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

    /// PR-C5 (Track C): strategy-aware parse wrapper. Dispatches the
    /// per-model [`crate::llm::json_strategy::JsonRecoveryStrategy`]
    /// to either [`crate::llm::json_extractor::parse_with_strategy`]
    /// (for `Strict`, which has no repair telemetry) or the
    /// existing traced chain
    /// [`crate::phases::util::parse_model_json_traced`] (for
    /// `Lenient` / `Continuation` / `PromptPrefill`, which
    /// surface `model.json_repair_applied` warnings through the
    /// sink). The wrapper converts the chain's error into
    /// `Error::SchemaViolation` so the existing retry-budget
    /// logic keeps its diagnostic shape, and runs the role-aware
    /// schema validation that [`Self::parse_model_json`] used to
    /// bundle.
    ///
    /// The `Request` parameter is currently unused (the wrapper
    /// keeps it for future diagnostic enrichment); we pass a
    /// placeholder that mirrors the call site so future
    /// `request.extra_messages` plumbing has somewhere to attach.
    async fn parse_with_strategy_inner<T>(
        &self,
        strategy: crate::llm::json_strategy::JsonRecoveryStrategy,
        raw: &str,
        role: Role,
        _schema_hint: &str,
    ) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        use crate::llm::json_extractor::{self, ParseError};
        let phase_name = role.as_str();
        // Build a placeholder Request for the wrapper's
        // diagnostic-context parameter. The wrapper does not
        // inspect the request today; passing a synthesised
        // skeleton keeps the signature future-proof without
        // forcing the caller to thread the original Request
        // through every retry.
        let placeholder = Request {
            role,
            model: self.default_model.clone(),
            system: String::new(),
            user: String::new(),
            max_tokens: 0,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let value: serde_json::Value = match strategy {
            // Strict path: direct parse only. No repair telemetry
            // because the chain is one-shot.
            crate::llm::json_strategy::JsonRecoveryStrategy::Strict => {
                match json_extractor::parse_with_strategy::<serde_json::Value>(
                    strategy,
                    &self.default_model,
                    &placeholder,
                    raw,
                )
                .await
                {
                    Ok(v) => v,
                    Err(ParseError::Strict(msg)) => {
                        return Err(crate::Error::SchemaViolation(format!(
                            "model output is not valid JSON: {msg}"
                        )));
                    }
                    Err(ParseError::Lenient(_)) => {
                        unreachable!("Strict strategy must surface ParseError::Strict on failure")
                    }
                }
            }
            // Lenient / Continuation / PromptPrefill paths: drive
            // the traced chain directly so repair events surface
            // as `model.json_repair_applied` warnings through the
            // telemetry sink. This preserves the contract the
            // `json_repair_emits_model_json_repair_applied_warning`
            // integration test pins.
            crate::llm::json_strategy::JsonRecoveryStrategy::Lenient
            | crate::llm::json_strategy::JsonRecoveryStrategy::Continuation
            | crate::llm::json_strategy::JsonRecoveryStrategy::PromptPrefill => {
                match crate::phases::util::parse_model_json_traced::<serde_json::Value, _>(
                    raw,
                    |ev| {
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
                            crate::telemetry::WarningContext {
                                phase: Some(phase_name.to_owned()),
                                role: Some(phase_name.to_owned()),
                                call_id: None,
                                attempt: None,
                            },
                        );
                    },
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(crate::Error::SchemaViolation(format!(
                            "model output is not valid JSON: {e}"
                        )));
                    }
                }
            }
        };
        // Schema validation: keep the role-aware diagnostic so
        // the post-execution review can see which field tripped
        // the role's contract.
        if let Err(schema_err) = role.validate_json(&value) {
            let _ = self.telemetry.warn(
                "model.schema_mismatch",
                "warn",
                "model JSON failed role schema check",
                serde_json::json!({
                    "schema_mismatch": schema_err.to_string(),
                }),
                crate::telemetry::WarningContext {
                    phase: Some(phase_name.to_owned()),
                    role: Some(phase_name.to_owned()),
                    call_id: None,
                    attempt: None,
                },
            );
            return Err(schema_err);
        }
        serde_json::from_value(value).map_err(|e| {
            crate::Error::SchemaViolation(format!(
                "model output did not deserialise into {}: {e}",
                std::any::type_name::<T>()
            ))
        })
    }

    /// PR-C5: one-shot assistant-prefill retry. Builds a fresh
    /// `Request` with `extra_messages = [{role:"assistant",
    /// content:"{"}]` and dispatches via [`Self::call_uncached`] so
    /// the prefill response does not poison the steady-state cache.
    /// Returns `Ok(Some(v))` on a successful recovery, `Ok(None)`
    /// when the helper decided not to retry (transport error,
    /// schema still wrong after retry), or `Err(e)` when the retry
    /// surfaced a non-recoverable error.
    async fn retry_with_assistant_prefill<T>(
        &self,
        role: Role,
        system: &str,
        user: &str,
        schema_hint: &str,
    ) -> crate::error::Result<Option<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        use crate::llm::json_strategy::strategy_for;
        let prefill_strategy = strategy_for(&self.default_model, None);
        // The prefill path is only meaningful on the OpenAI-compat
        // body builder (Anthropic-compat and the Responses API
        // ignore `extra_messages`). When the body builder does not
        // honour prefill, skip the retry and let the caller fall
        // through to the normal budget logic.
        if !crate::llm::json_strategy::needs_assistant_prefill(prefill_strategy) {
            return Ok(None);
        }
        let mut req = Request {
            role,
            model: self.default_model.clone(),
            system: system.to_owned(),
            user: user.to_owned(),
            max_tokens: crate::phases::phase::max_tokens_for_role(role),
            temperature: None,
            top_p: Some(0.95),
            response_schema: None,
            stream: false,
            extra_messages: vec![crate::llm::wire::Message {
                role: "assistant".to_owned(),
                content: "{".to_owned(),
            }],
            attachments: vec![],
            tool_choice: None,
        };
        // Re-apply the same per-provider cap that the dispatch
        // path applies so the wire body matches what the cache
        // expects on the non-prefill path.
        let provider_top_p = self
            .config
            .providers
            .get(&self.default_provider)
            .and_then(|s| s.top_p);
        req.top_p = Some(provider_top_p.unwrap_or(0.95));
        let started = crate::time::now_unix_secs();
        let response = self.dispatch_to_provider(req, None, started, 0).await?;
        // Run the lenient pipeline against the prefill response.
        // If it still fails to parse, return None so the caller
        // can fall through to the normal parse-failure budget.
        match self
            .parse_with_strategy_inner::<T>(
                crate::llm::json_strategy::JsonRecoveryStrategy::Lenient,
                &response.text,
                role,
                schema_hint,
            )
            .await
        {
            Ok(v) => Ok(Some(v)),
            Err(_) => Ok(None),
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
                u32::MAX,
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

    /// PR-C2: focused continuation on a truncated response.
    ///
    /// When the original response comes back with
    /// [`crate::llm::Response::truncated`] set (Anthropic:
    /// `stop_reason="max_tokens"`, OpenAI-compat:
    /// `finish_reason="length"`), the dispatcher re-issues the call
    /// as [`Role::Continuation`] with the last ~500 bytes of the
    /// truncated payload inlined under `${last_excerpt}` (see
    /// [`crate::llm::prompts::render_continuation_prompt`]). Each
    /// continuation returns a small JSON envelope whose `continued`
    /// payload is appended to the running accumulator; the loop
    /// terminates when the model signals `finished`, when the
    /// continuation itself is non-truncated (we stop regardless of
    /// `finished`), when the continuation response fails to parse
    /// as JSON, when the continuation call returns a transport /
    /// HTTP error, or after [`MAX_CONTINUATIONS`] (2) attempts.
    ///
    /// Behavioural contract:
    ///
    /// - The continuation call is dispatched via
    ///   [`Self::dispatch_to_provider`] with a hand-built
    ///   [`crate::llm::Request`]. It deliberately does NOT go
    ///   through [`Self::call_uncached`] so the stubborn-model
    ///   JSON prefix from
    ///   [`crate::llm::response_format_opt_out::render_system_prompt_with_prefix`]
    ///   is NOT prepended — the continuation prompt IS the JSON
    ///   contract, and prefixing it would add noise the model has
    ///   to ignore.
    /// - Token usage is summed across the original response and
    ///   every successful continuation so the run's
    ///   `telemetry/calls.jsonl.gz` reflects the true cost of the
    ///   call, not just the first round-trip.
    /// - Each attempt emits a `model.continuation_attempt` warning
    ///   so the operator can see the re-call in the warnings
    ///   stream.
    /// - On the failure path (transport error, parse error, cap
    ///   reached) the helper preserves `truncated = true` so the
    ///   rest of the pipeline — `model.response_truncated`, the
    ///   parse layer — sees the same input it sees today.
    /// - This helper does NOT log a `model.response_truncated`
    ///   warning; that is the dispatcher's responsibility and
    ///   fires from the original response only.
    async fn continue_truncated_response(
        &self,
        role: Role,
        original: &Response,
        _started_unix: i64,
    ) -> Response {
        use crate::domain::ContinuationReport;
        use crate::llm::prompts::render_continuation_prompt;

        const MAX_CONTINUATIONS: u8 = 2;
        const EXCERPT_BYTES: usize = 500;

        let mut accumulated = original.text.clone();
        let mut truncated = original.truncated;
        let mut last_finish_reason = original.finish_reason.clone();
        let mut total_input = original.usage.input_tokens;
        let mut total_output = original.usage.output_tokens;
        let mut total_cache_read = original.usage.cache_read;
        let mut total_cache_creation = original.usage.cache_creation;

        let mut attempt_idx: u8 = 0;
        while attempt_idx < MAX_CONTINUATIONS && truncated {
            let last_excerpt: String =
                crate::phases::util::safe_tail(&accumulated, EXCERPT_BYTES).to_string();

            let _ = self.telemetry.warn(
                "model.continuation_attempt",
                "info",
                "focused continuation re-call after truncated response",
                serde_json::json!({
                    "attempt": attempt_idx,
                    "original_output_tokens": original.usage.output_tokens,
                    "excerpt_chars": last_excerpt.chars().count(),
                    "role": role.as_str(),
                }),
                WarningContext {
                    phase: Some(role.as_str().to_owned()),
                    role: Some(role.as_str().to_owned()),
                    call_id: None,
                    attempt: Some(u32::from(attempt_idx)),
                },
            );

            let system = render_continuation_prompt(&last_excerpt);
            let user = "Continue.".to_string();
            let provider_top_p = self
                .config
                .providers
                .get(&self.default_provider)
                .and_then(|s| s.top_p);
            let req = Request {
                role: Role::Continuation,
                model: self.default_model.clone(),
                system,
                user,
                max_tokens: max_tokens_for_role(Role::Continuation),
                temperature: Some(temperature_for_role(Role::Continuation, None)),
                top_p: Some(provider_top_p.unwrap_or(0.95)),
                response_schema: None,
                stream: false,
                extra_messages: vec![],
                attachments: vec![],
                tool_choice: None,
            };

            let started = crate::time::now_unix_secs();
            match self
                .dispatch_to_provider(req, None, started, u32::from(attempt_idx))
                .await
            {
                Ok(cont_resp) => {
                    total_input = total_input.saturating_add(cont_resp.usage.input_tokens);
                    total_output = total_output.saturating_add(cont_resp.usage.output_tokens);
                    total_cache_read = total_cache_read.saturating_add(cont_resp.usage.cache_read);
                    total_cache_creation =
                        total_cache_creation.saturating_add(cont_resp.usage.cache_creation);
                    if let Some(fr) = cont_resp.finish_reason.clone() {
                        last_finish_reason = Some(fr);
                    }

                    match serde_json::from_str::<ContinuationReport>(&cont_resp.text) {
                        Ok(report) => {
                            if !report.continued.is_empty() {
                                accumulated.push_str(&report.continued);
                            }
                            attempt_idx = attempt_idx.saturating_add(1);
                            if report.finished {
                                truncated = false;
                            } else {
                                truncated = cont_resp.truncated;
                            }
                            if !truncated {
                                break;
                            }
                        }
                        Err(_) => {
                            let _ = self.telemetry.warn(
                                "model.continuation_failed",
                                "warn",
                                "continuation response was not parseable as JSON",
                                serde_json::json!({
                                    "attempt": attempt_idx,
                                    "raw_tail": crate::phases::util::safe_tail(
                                        &cont_resp.text,
                                        EXCERPT_BYTES,
                                    ),
                                }),
                                WarningContext {
                                    phase: Some(role.as_str().to_owned()),
                                    role: Some(role.as_str().to_owned()),
                                    call_id: None,
                                    attempt: Some(u32::from(attempt_idx)),
                                },
                            );
                            // Keep `truncated = true` so the
                            // existing parse path sees the same
                            // failure shape it sees today.
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = self.telemetry.warn(
                        "model.continuation_failed",
                        "warn",
                        "continuation call transport/HTTP error",
                        serde_json::json!({
                            "attempt": attempt_idx,
                            "error": e.to_string(),
                        }),
                        WarningContext {
                            phase: Some(role.as_str().to_owned()),
                            role: Some(role.as_str().to_owned()),
                            call_id: None,
                            attempt: Some(u32::from(attempt_idx)),
                        },
                    );
                    break;
                }
            }
        }

        Response {
            text: accumulated,
            finish_reason: last_finish_reason,
            truncated,
            usage: crate::llm::Usage {
                input_tokens: total_input,
                output_tokens: total_output,
                cache_read: total_cache_read,
                cache_creation: total_cache_creation,
            },
        }
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
        use crate::llm::json_strategy::{self, JsonRecoveryStrategy};
        use crate::llm::retry_budget::{self, RetryBudget, RetryReason};

        let phase_name = role.as_str();
        let mode = parse_mode_str(&self.mode);
        // Resolve the JSON recovery strategy once per call. The
        // lookup consults the per-model table plus the (future)
        // profile-overrides hook; the dispatch loop below consults
        // the strategy on every parse attempt.
        let strategy = json_strategy::strategy_for(&self.default_model, None);
        // The legacy `max_retries` parameter is now a hard ceiling.
        // The actual cap is `min(budget.max_attempts, ceiling + 1)`.
        let ceiling = max_retries;
        // Tracks whether the `PromptPrefill` strategy has already
        // fired its one-shot assistant-prefill retry. The retry
        // is a response-side hint, not a transport retry, so it
        // does NOT consume the legacy retry budget. After the
        // first prefill retry, the loop falls back to the normal
        // parse-failure budget.
        let mut prefill_attempted: bool = false;

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
            // not poison the retry loop. `attempt` also becomes the
            // `retry_count` tag on the persisted `calls` row so the
            // JSONL/SQLite telemetry mirrors the warnings stream.
            let started_unix = crate::time::now_unix_secs();
            let response = if attempt == 0 {
                self.call_with_retry(role, system.clone(), user.clone(), attempt)
                    .await
            } else {
                self.call_uncached(role, system.clone(), user.clone(), started_unix, attempt)
                    .await
            };
            // PR-C2: focused continuation on truncation. When the
            // original response came back with `truncated = true`
            // (Anthropic: stop_reason="max_tokens", OpenAI-compat:
            // finish_reason="length") and is non-empty, swap in the
            // continuation-augmented response so the parse pipeline
            // runs on the stitched text. Cap at 2 attempts (D.21.6);
            // after the cap we keep `truncated = true` so the
            // existing parse path sees the same input it sees today
            // (just one-shot, no retry).
            let response = match response {
                Ok(resp) if resp.truncated && !resp.text.is_empty() => Ok(self
                    .continue_truncated_response(role, &resp, started_unix)
                    .await),
                other => other,
            };
            let warn_ctx = || WarningContext {
                phase: Some(phase_name.to_owned()),
                role: Some(phase_name.to_owned()),
                call_id: None,
                attempt: Some(attempt),
            };

            let decision = match response {
                Ok(resp) => {
                    // Strategy-aware parse: the wrapper chooses
                    // Strict (direct parse only) vs Lenient (full
                    // recovery chain). Continuation and PromptPrefill
                    // run Lenient for the single parse attempt and
                    // rely on the post-parse retry paths below.
                    let parse_outcome = self
                        .parse_with_strategy_inner::<T>(strategy, &resp.text, role, schema_hint)
                        .await;
                    match parse_outcome {
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
                            // PR-C5: prompt-prefill retry. When the
                            // strategy is `PromptPrefill` and we
                            // have not yet retried, build a fresh
                            // Request with an assistant prefill of
                            // `{` and dispatch via the cache-bypass
                            // path. The retry is a response-side
                            // hint — it does NOT consume the legacy
                            // retry budget — so we fire exactly
                            // once. If the prefill retry still fails
                            // to parse, fall through to the normal
                            // parse-failure budget below.
                            if strategy == JsonRecoveryStrategy::PromptPrefill && !prefill_attempted
                            {
                                prefill_attempted = true;
                                match self
                                    .retry_with_assistant_prefill::<T>(
                                        role,
                                        &system,
                                        &user,
                                        schema_hint,
                                    )
                                    .await
                                {
                                    Ok(Some(v)) => {
                                        let _ = self.telemetry.warn(
                                            "model.prefill_recovered",
                                            "info",
                                            "model answer recovered after assistant prefill retry",
                                            serde_json::json!({
                                                "attempts": attempt + 1,
                                            }),
                                            warn_ctx(),
                                        );
                                        return Ok(v);
                                    }
                                    Ok(None) | Err(_) => {
                                        // Either the helper decided not
                                        // to retry, or the retry call
                                        // itself failed. Fall through
                                        // to the normal budget logic;
                                        // the error from the helper is
                                        // already logged via the
                                        // helper's own telemetry path
                                        // (or will be surfaced by the
                                        // existing `parse_with_retry`
                                        // retry on the next attempt).
                                    }
                                }
                            }
                            // `parse_with_strategy_inner` always
                            // wraps its failure in
                            // `Error::SchemaViolation`, so we
                            // re-check the raw payload: parseable as
                            // a generic `serde_json::Value` means
                            // the failure is a schema mismatch (the
                            // JSON shape did not match the role's
                            // contract); not parseable means the
                            // failure is a genuine parse error.
                            let reason =
                                if serde_json::from_str::<serde_json::Value>(&resp.text).is_ok() {
                                    RetryReason::Schema
                                } else {
                                    RetryReason::Parse
                                };
                            let b =
                                budget.unwrap_or_else(|| retry_budget::budget_for(mode, reason));
                            budget = Some(b);
                            let max_attempts = b.max_attempts.min(ceiling + 1);
                            if attempt + 1 >= max_attempts {
                                if b.use_json_repair
                                    && self.config.json_repair_v2_enabled_for_mode(&self.mode)
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
                    }
                }
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

/// Per-role `max_tokens` ceiling. As of v0.6 every role uses the same
/// `DEFAULT_MAX_TOKENS` (1_000_000) so prose-heavy outputs are never
/// truncated mid-thought. Calibrated per observation that the model
/// legitimately needs much more headroom than the previous
/// 512..=32_768 ceilings. The Anthropic-compatible request path uses
/// this number verbatim; the OpenAI-compat provider additionally
/// clamps to the per-provider `ProviderConfig::max_tokens` cap.
fn max_tokens_for_role(role: Role) -> u32 {
    match role {
        Role::Intake => DEFAULT_MAX_TOKENS,
        Role::Clarify => DEFAULT_MAX_TOKENS,
        Role::Route => DEFAULT_MAX_TOKENS,
        Role::Sketch => DEFAULT_MAX_TOKENS,
        Role::Propose => DEFAULT_MAX_TOKENS,
        Role::Gate => DEFAULT_MAX_TOKENS,
        Role::Critique => DEFAULT_MAX_TOKENS,
        Role::Repair => DEFAULT_MAX_TOKENS,
        Role::Judge => DEFAULT_MAX_TOKENS,
        Role::Rank => DEFAULT_MAX_TOKENS,
        Role::Deliver => DEFAULT_MAX_TOKENS,
        // Discovery (Plan B sub-phase B). Per docs/v0.2-status.md and
        // proposal-01-concept.md §6.5–§6.10.
        Role::Tagger => DEFAULT_MAX_TOKENS,
        // facet_deriver carries 3-6 facet triples (name + description + required).
        Role::FacetDeriver => DEFAULT_MAX_TOKENS,
        Role::Extractor => DEFAULT_MAX_TOKENS,
        Role::Integrator => DEFAULT_MAX_TOKENS,
        // Phase D (Plan B sub-phase D). Synthesizer reuses the
        // integrator ceiling (markdown body + structured fields).
        // Adversary stays short: it returns weaknesses, not a long
        // report.
        Role::Synthesizer => DEFAULT_MAX_TOKENS,
        Role::Adversary => DEFAULT_MAX_TOKENS,
        // Phase G (v0.3). Decomposer: T01-06 §4.2 originally suggested 3000; with the v0.6 unified ceiling of 1_000_000 this concern is moot.
        Role::Decomposer => DEFAULT_MAX_TOKENS,
        Role::MergeSynthesizer => DEFAULT_MAX_TOKENS,
        Role::RecoveryExplainer => DEFAULT_MAX_TOKENS,
        Role::RationaleExtractor => DEFAULT_MAX_TOKENS,
        // Track H batch-1: D.7.1 catalog opt-in roles. Each carries
        // its own sampling contract (see `role_settings` in
        // `src/llm/prompts.rs`); these are the runtime ceilings
        // used when the catalog role is invoked outside any phase.
        Role::TiefighterCritic => DEFAULT_MAX_TOKENS,
        // persona_picker is a short routing decision; the ceiling matches its role_settings (see prompts.rs).
        Role::PersonaPicker => DEFAULT_MAX_TOKENS,
        // angle_picker is a routing decision with a one-line rationale; the ceiling matches role_settings.
        Role::AnglePicker => DEFAULT_MAX_TOKENS,
        // Track H batch-2: tiebreaker ceiling per D.7.1.
        Role::FinalDisagreement => DEFAULT_MAX_TOKENS,
        // Track H batch-2 (commit 2): LLM re-call for malformed
        // JSON — carries a repaired payload plus the audit fields.
        Role::JsonRepairV2 => DEFAULT_MAX_TOKENS,
        // Track H batch-2 (commit 3): prompt-injection guard. The ceiling matches the role contract (verdict + reasons + recommended_action).
        Role::HostilePromptDetector => DEFAULT_MAX_TOKENS,
        // PR-C2: focused re-call on a truncated response. The
        // continuation role emits a tiny JSON envelope whose
        // payload is appended onto the truncated text — 1M is
        // the same ceiling as the role it continues so a single
        // continuation can finish a long-form response.
        Role::Continuation => DEFAULT_MAX_TOKENS,
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
///
/// `pub` (re-exported via [`crate::phases`]) so persistence helpers
/// outside `phase.rs` — currently
/// [`crate::phases::discover_matrix::DiscoverMatrixPhase::write_draft`]
/// writing the V4 §6.10 `drafts/<sketch_id>.md` sidecar — can
/// stamp the same temperature the LLM call was issued with without
/// having to inline the lookup table.
pub fn temperature_for_role(
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
        // PR-C2: focused re-call on a truncated response. T=0.0
        // so two continuations of the same excerpt produce the
        // same output (snapshot diffs stay stable). top_p is
        // resolved by `role_settings` (0.5) so the call layer
        // does not have to special-case the role.
        Role::Continuation => 0.0,
    }
}

/// PR-B2: extend [`temperature_for_role`] to honour the active
/// provider's `ProviderConfig::temperature` as a base.
///
/// Precedence (highest first):
/// 1. Profile-defined override for `role` (when present).
/// 2. `provider_base` (when `Some`) — the per-provider default from
///    `[providers.<name>].temperature` in the user's TOML.
/// 3. The hard-coded per-role default from [`temperature_for_role`].
///
/// Without this helper, a user that writes
/// `[providers.minimax] temperature = 0.42` sees their value parsed
/// into `ProviderConfig.temperature` but the call layer still stamps
/// the per-role default into every request (the user-reported
/// "config-key ignored" bug class).
pub fn resolve_temperature(
    role: Role,
    profile_overrides: Option<&std::collections::HashMap<String, f32>>,
    provider_base: Option<f32>,
) -> f32 {
    if let Some(map) = profile_overrides
        && let Some(v) = map.get(role.as_str())
    {
        return *v;
    }
    if let Some(base) = provider_base {
        return base;
    }
    temperature_for_role(role, profile_overrides)
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
    /// Phase D follow-up (D.22.1, D.12.5): `rankings/adversary_report.json`
    /// was written. Carries the seven-pattern deterministic verdict
    /// produced by [`crate::phases::adversary::AdversaryPhase`].
    /// Complementary to `Adversaries` (which holds the LLM-emitted
    /// per-proposal adversary reports); the two coexist.
    PatternAdversary(PathBuf),
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
    use crate::config::ProviderConfig;
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
        retry_context_with_model("retry-model", outcomes)
    }

    fn retry_context_with_model(
        model: &str,
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
            model.into(),
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

    fn truncated_response(text: &str) -> Response {
        Response {
            text: text.into(),
            finish_reason: Some("max_tokens".into()),
            truncated: true,
            usage: Default::default(),
        }
    }

    fn continuation_envelope(continued: &str, finished: bool) -> Response {
        let payload = serde_json::json!({
            "continued": continued,
            "finished": finished,
            "raw_excerpt": "",
            "schema_version": "continuation.v1",
        });
        Response {
            text: serde_json::to_string(&payload).expect("envelope serializes"),
            finish_reason: Some("end_turn".into()),
            truncated: false,
            usage: Default::default(),
        }
    }

    fn count_warnings_with_code(temp: &tempfile::TempDir, code: &str) -> usize {
        // The on-disk path is `<MOAGAN_HOME>/.runs/<run_id>/telemetry/warnings.jsonl`.
        // `retry_context` builds the home at `temp.path()` so we
        // scan every `.runs/<id>/telemetry/warnings.jsonl` and
        // count the matching `code` occurrences across all runs.
        // We do NOT `serde_json::from_str` the line because the
        // redactor may have rewritten numeric fields into
        // `[REDACTED:<kind>]` placeholders, which would make a
        // strict JSON parse fail and silently drop the warning.
        let runs_dir = temp.path().join(".runs");
        let entries = match std::fs::read_dir(&runs_dir) {
            Ok(it) => it,
            Err(_) => return 0,
        };
        let needle = format!("\"code\":\"{code}\"");
        let mut count = 0usize;
        for entry in entries.flatten() {
            let path = entry.path().join("telemetry").join("warnings.jsonl");
            let contents = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            count += contents
                .lines()
                .filter(|line| line.contains(&needle))
                .count();
        }
        count
    }

    fn count_warnings_with_code_for_phase(
        temp: &tempfile::TempDir,
        code: &str,
        phase: &str,
    ) -> usize {
        let runs_dir = temp.path().join(".runs");
        let entries = match std::fs::read_dir(&runs_dir) {
            Ok(it) => it,
            Err(_) => return 0,
        };
        let needle = format!("\"code\":\"{code}\"");
        let phase_needle = format!("\"phase\":\"{phase}\"");
        let mut count = 0usize;
        for entry in entries.flatten() {
            let path = entry.path().join("telemetry").join("warnings.jsonl");
            let contents = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            count += contents
                .lines()
                .filter(|line| line.contains(&needle) && line.contains(&phase_needle))
                .count();
        }
        count
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

    /// PR-C1 (cluster C, JSON robustness): `moagan discover` runs
    /// spawn the matrix fan-out at the `RunContext::mode == "discover"`
    /// sentinel, so the `JsonRepairV2` re-call should fire by
    /// default — the operator never had to opt in via
    /// `llm.json_repair_v2_enabled`. Pin that the gate
    /// (`Config::json_repair_v2_enabled_for_mode`) routes the
    /// `discover` mode string to `true` regardless of the typed
    /// config field.
    #[tokio::test]
    async fn json_repair_v2_fires_for_discovery_mode_with_default_config() {
        let (_temp, mut ctx, script) = retry_context(vec![
            Ok((200, response(r#"{"broken":"unterminated}"#))),
            Ok((
                200,
                response(
                    r#"{"malformed":"broken","target_schema":"intake","repaired":"{\"ok\":true}","notes":"fixed","schema_version":"json_repair_v2.v1"}"#,
                ),
            )),
        ]);
        assert!(
            !ctx.config.llm.json_repair_v2_enabled,
            "fixture: typed config flag must stay false so the helper is \
             the only thing that flips the gate for discover"
        );
        ctx.mode = "discover".into();
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
        assert_eq!(
            script.calls.load(Ordering::SeqCst),
            2,
            "first call: original Intake (malformed); second call: JsonRepairV2 re-call"
        );
    }

    /// PR-C1: for non-`discover` modes the helper routes through the
    /// typed config field (default `false`), so a `fast` run with
    /// a broken first response surfaces `SchemaViolation` instead
    /// of paying for the repair re-call. Mirrors today's behaviour
    /// — the `fast` mode cost profile is unchanged.
    #[tokio::test]
    async fn json_repair_v2_skipped_for_fast_mode_with_default_config() {
        let (_temp, mut ctx, script) = retry_context(vec![
            Ok((200, response(r#"{"broken":"unterminated}"#))),
            Ok((200, response(r#"{"ok":true}"#))),
        ]);
        assert!(!ctx.config.llm.json_repair_v2_enabled);
        ctx.mode = "fast".into();
        let result = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                String::new(),
                String::new(),
                "Intake: {problem, objectives[]}",
                5,
            )
            .await;
        assert!(
            matches!(result, Err(Error::SchemaViolation(_))),
            "fast mode with default config must not invoke the repair re-call"
        );
        assert_eq!(
            script.calls.load(Ordering::SeqCst),
            1,
            "fast mode is single-shot (no retry, no repair)"
        );
    }

    #[test]
    fn call_with_retry_parse_handles_error_without_code_as_retriable() {
        assert!(error_code_is_retriable(None));
    }

    /// PR-C2: focused continuation on a truncated response. The
    /// original LLM call returns `truncated = true` with the
    /// payload mid-JSON (`{"a":`). The continuation call returns
    /// a JSON envelope whose `continued` field holds the rest of
    /// the value (`1}`); the dispatcher concatenates the two and
    /// runs the parse pipeline on the joined text. The pipeline
    /// must succeed and exactly one `model.continuation_attempt`
    /// warning must be emitted with `attempt = 0`.
    #[tokio::test]
    async fn phase_continuation_loop_fires_when_truncated() {
        let (temp, ctx, script) = retry_context(vec![
            Ok((200, truncated_response(r#"{"a":"#))),
            Ok((200, continuation_envelope("1}", true))),
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
            .expect("parse must succeed after continuation");
        assert_eq!(result["a"], 1, "concatenated text parses as JSON");
        // 1 original call + 1 continuation call.
        assert_eq!(script.calls.load(Ordering::SeqCst), 2);
        let count = count_warnings_with_code(&temp, "model.continuation_attempt");
        assert_eq!(count, 1, "exactly one continuation_attempt event");
    }

    /// PR-C2: the continuation loop caps at 2 attempts. When every
    /// continuation call comes back truncated itself, the
    /// dispatcher bails out, marks the response `truncated = true`
    /// (so the existing parse path sees the same input it sees
    /// today), and emits exactly 2 `model.continuation_attempt`
    /// events plus a `model.response_truncated` warning for the
    /// original call.
    #[tokio::test]
    async fn phase_continuation_loop_caps_at_two_attempts() {
        fn continuation_envelope_truncated(continued: &str, finished: bool) -> Response {
            let payload = serde_json::json!({
                "continued": continued,
                "finished": finished,
                "raw_excerpt": "",
                "schema_version": "continuation.v1",
            });
            Response {
                text: serde_json::to_string(&payload).expect("envelope serializes"),
                finish_reason: Some("max_tokens".into()),
                truncated: true,
                usage: Default::default(),
            }
        }
        let (temp, ctx, script) = retry_context(vec![
            Ok((200, truncated_response("abc"))),
            Ok((200, continuation_envelope_truncated("abc", false))),
            Ok((200, continuation_envelope_truncated("abc", false))),
        ]);
        let result = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                String::new(),
                String::new(),
                "Value",
                5,
            )
            .await;
        assert!(
            matches!(result, Err(Error::SchemaViolation(_))),
            "truncated text is not JSON; parse fails as SchemaViolation"
        );
        // 1 original + 2 continuations = 3 calls total.
        assert_eq!(script.calls.load(Ordering::SeqCst), 3);
        let cont_count = count_warnings_with_code(&temp, "model.continuation_attempt");
        assert_eq!(
            cont_count, 2,
            "exactly two continuation_attempt events (the cap)"
        );
        let truncated_count =
            count_warnings_with_code_for_phase(&temp, "model.response_truncated", "intake");
        assert_eq!(
            truncated_count, 1,
            "the original model.response_truncated warning fires once for the intake phase"
        );
    }

    /// PR-C2: when the response is NOT truncated, the dispatcher
    /// skips the continuation loop entirely. No
    /// `model.continuation_attempt` events are emitted and the
    /// provider sees exactly one call.
    #[tokio::test]
    async fn phase_skips_continuation_when_response_not_truncated() {
        let (temp, ctx, script) = retry_context(vec![Ok((200, response(r#"{"ok":true}"#)))]);
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
        assert_eq!(script.calls.load(Ordering::SeqCst), 1);
        let cont_count = count_warnings_with_code(&temp, "model.continuation_attempt");
        assert_eq!(cont_count, 0, "no continuation events on clean path");
    }

    /// PR-C2: when the continuation call itself returns a
    /// transport / HTTP error, the loop bails out instead of
    /// retrying. The dispatcher emits a `model.continuation_failed`
    /// warning and the parse pipeline sees the original truncated
    /// text (which fails to parse).
    #[tokio::test]
    async fn phase_skips_continuation_on_first_transport_error() {
        let (temp, ctx, script) = retry_context(vec![
            Ok((200, truncated_response(r#"{"x":"#))),
            Err(Error::Provider("transport died".into())),
        ]);
        let result = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                String::new(),
                String::new(),
                "Value",
                5,
            )
            .await;
        assert!(
            matches!(result, Err(Error::SchemaViolation(_))),
            "transport error in continuation falls back to today's parse-fail behaviour"
        );
        assert_eq!(
            script.calls.load(Ordering::SeqCst),
            2,
            "1 original + 1 continuation that errored"
        );
        let cont_count = count_warnings_with_code(&temp, "model.continuation_attempt");
        assert_eq!(cont_count, 1, "exactly one continuation_attempt event");
        let failed_count = count_warnings_with_code(&temp, "model.continuation_failed");
        assert_eq!(failed_count, 1, "the transport error is reported");
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

    // ----------------------------------------------------------------
    // PR-B2: `resolve_temperature` and per-provider temperature /
    // top_p wiring into `call_with_retry`.
    //
    // These tests pin the precedence contract end-to-end (the
    // helper + the call site) so a future refactor cannot silently
    // route around the user's `[providers.X].temperature` knob.
    // ----------------------------------------------------------------

    /// Profile override still wins over the per-provider base.
    /// (Spec: "profile overrides still win over the per-provider
    /// value".)
    #[test]
    fn resolve_temperature_profile_overrides_provider_base() {
        let map = std::collections::HashMap::new();
        let mut map = map;
        map.insert("sketch".to_owned(), 0.25_f32);
        assert_eq!(
            resolve_temperature(Role::Sketch, Some(&map), Some(0.9)),
            0.25,
            "profile override must beat the per-provider base"
        );
    }

    /// Provider base wins over the role default when no profile
    /// override is present.
    #[test]
    fn resolve_temperature_provider_base_beats_role_default() {
        assert_eq!(
            resolve_temperature(Role::Sketch, None, Some(0.42)),
            0.42,
            "per-provider temperature must beat the per-role default (Sketch=1.0)"
        );
        assert_eq!(
            resolve_temperature(Role::Clarify, None, Some(0.42)),
            0.42,
            "per-provider temperature must beat the per-role default (Clarify=0.0)"
        );
    }

    /// When no profile override AND no provider base are present,
    /// the helper falls through to the existing role defaults.
    #[test]
    fn resolve_temperature_no_base_falls_back_to_role_default() {
        assert_eq!(
            resolve_temperature(Role::Sketch, None, None),
            1.0,
            "no profile, no provider base → role default (Sketch)"
        );
        assert_eq!(
            resolve_temperature(Role::Clarify, None, None),
            0.0,
            "no profile, no provider base → role default (Clarify)"
        );
    }

    /// A `RecordingProvider` that captures the `Request` it received.
    /// Used by the call-layer test below to assert that the user's
    /// `[providers.X].temperature = 0.42` actually reaches the wire.
    /// The `captured` slot is shared via `Arc<Mutex<...>>` so the
    /// test body can read what `send` recorded.
    struct RecordingProvider {
        captured: Arc<parking_lot::Mutex<Option<crate::llm::Request>>>,
    }

    #[async_trait::async_trait]
    impl crate::llm::Provider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }
        fn model(&self) -> &str {
            "recording-model"
        }
        fn endpoint(&self) -> &str {
            "mock://recording"
        }
        async fn send(&self, req: &crate::llm::Request) -> Result<(u16, crate::llm::Response)> {
            *self.captured.lock() = Some(req.clone());
            Ok((
                200,
                crate::llm::Response {
                    text: r#"{"ok":true}"#.into(),
                    finish_reason: Some("end_turn".into()),
                    truncated: false,
                    usage: Default::default(),
                },
            ))
        }
    }

    /// End-to-end: a `Config` that has `temperature = 0.42` on the
    /// active provider must reach `Request.temperature` inside
    /// `call_with_retry`, NOT the per-role default (Sketch = 1.0).
    /// This is the bug the audit surfaced: the field was parsed but
    /// the call layer stamped the role default instead.
    #[tokio::test]
    async fn call_with_retry_honors_provider_temperature() {
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
        .expect("Telemetry::open");

        let captured = Arc::new(parking_lot::Mutex::new(None));
        let provider: Arc<RecordingProvider> = Arc::new(RecordingProvider {
            captured: Arc::clone(&captured),
        });
        let provider_dyn: Arc<dyn crate::llm::Provider> = provider.clone();
        let mut registry = ProviderRegistry::default();
        registry.insert("recording".into(), provider_dyn);

        // Build a Config with the active provider carrying
        // temperature=0.42 and top_p=0.5.
        let mut cfg = Config::default();
        cfg.providers.insert(
            "recording".to_owned(),
            ProviderConfig {
                kind: "mock".to_owned(),
                endpoint: "mock://recording".to_owned(),
                model: "recording-model".to_owned(),
                max_tokens: Some(1024),
                temperature: Some(0.42),
                top_p: Some(0.5),
                ..ProviderConfig::default()
            },
        );
        let ctx = RunContext::new_with_config(
            run_id,
            home,
            Arc::new(registry),
            "recording".into(),
            "recording-model".into(),
            Parallelism::new(1),
            telemetry,
            String::new(),
            "standard".into(),
            Arc::new(cfg),
        );

        let result = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Sketch,
                String::new(),
                String::new(),
                "Value",
                1,
            )
            .await;
        assert!(result.is_ok(), "call should succeed: {result:?}");
        let recorded = captured
            .lock()
            .clone()
            .expect("provider captured the request");
        assert_eq!(
            recorded.temperature,
            Some(0.42),
            "call_with_retry must stamp per-provider temperature (0.42), NOT the role default (Sketch=1.0)"
        );
        assert_eq!(
            recorded.top_p,
            Some(0.5),
            "call_with_retry must stamp per-provider top_p (0.5), NOT the hard-coded 0.95"
        );
    }

    /// When no provider temperature is set, the call layer falls
    /// back to the per-role default (the legacy behaviour). Pins
    /// the "no field → no behaviour change" requirement.
    #[tokio::test]
    async fn call_with_retry_falls_back_to_role_default_when_provider_temperature_absent() {
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
        .expect("Telemetry::open");

        let captured = Arc::new(parking_lot::Mutex::new(None));
        let provider: Arc<RecordingProvider> = Arc::new(RecordingProvider {
            captured: Arc::clone(&captured),
        });
        let provider_dyn: Arc<dyn crate::llm::Provider> = provider.clone();
        let mut registry = ProviderRegistry::default();
        registry.insert("recording".into(), provider_dyn);

        // Provider has NO temperature/top_p set (None).
        let mut cfg = Config::default();
        cfg.providers.insert(
            "recording".to_owned(),
            ProviderConfig {
                kind: "mock".to_owned(),
                endpoint: "mock://recording".to_owned(),
                model: "recording-model".to_owned(),
                max_tokens: Some(1024),
                temperature: None,
                top_p: None,
                ..ProviderConfig::default()
            },
        );
        let ctx = RunContext::new_with_config(
            run_id,
            home,
            Arc::new(registry),
            "recording".into(),
            "recording-model".into(),
            Parallelism::new(1),
            telemetry,
            String::new(),
            "standard".into(),
            Arc::new(cfg),
        );

        let _ = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Sketch,
                String::new(),
                String::new(),
                "Value",
                1,
            )
            .await
            .expect("call should succeed");
        let recorded = captured.lock().clone().expect("captured");
        assert_eq!(
            recorded.temperature,
            Some(1.0),
            "without provider temperature, the per-role default (Sketch=1.0) must apply"
        );
        assert_eq!(
            recorded.top_p,
            Some(0.95),
            "without provider top_p, the hard-coded 0.95 must apply"
        );
    }

    /// PR-D1: `call_with_retry_at_temp` stamps the explicit
    /// `temperature` parameter straight into `Request.temperature`
    /// — bypassing `resolve_temperature`, the per-role default,
    /// and `ProviderConfig::temperature`. The discovery matrix
    /// phase relies on this property to drive every `(cell,
    /// temperature, replica)` triple with the operator-chosen
    /// temperature; any indirection (e.g. consulting the role
    /// default or the provider config) would silently collapse
    /// the fan-out back to v0.5.
    #[tokio::test]
    async fn call_with_retry_at_temp_stamps_provided_temperature() {
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
        .expect("Telemetry::open");

        let captured = Arc::new(parking_lot::Mutex::new(None));
        let provider: Arc<RecordingProvider> = Arc::new(RecordingProvider {
            captured: Arc::clone(&captured),
        });
        let provider_dyn: Arc<dyn crate::llm::Provider> = provider.clone();
        let mut registry = ProviderRegistry::default();
        registry.insert("recording".into(), provider_dyn);

        // Config sets a different per-provider temperature; the
        // explicit parameter must still win. This proves
        // `call_with_retry_at_temp` does NOT consult the provider
        // config — only the `temperature` parameter.
        let mut cfg = Config::default();
        cfg.providers.insert(
            "recording".to_owned(),
            ProviderConfig {
                kind: "mock".to_owned(),
                endpoint: "mock://recording".to_owned(),
                model: "recording-model".to_owned(),
                max_tokens: Some(1024),
                temperature: Some(0.99),
                top_p: Some(0.5),
                ..ProviderConfig::default()
            },
        );
        let ctx = RunContext::new_with_config(
            run_id,
            home,
            Arc::new(registry),
            "recording".into(),
            "recording-model".into(),
            Parallelism::new(1),
            telemetry,
            String::new(),
            "standard".into(),
            Arc::new(cfg),
        );

        let result = ctx
            .call_with_retry_at_temp(Role::Sketch, String::new(), String::new(), 0, 0.3)
            .await;
        assert!(result.is_ok(), "call should succeed: {result:?}");
        let recorded = captured
            .lock()
            .clone()
            .expect("provider captured the request");
        assert_eq!(
            recorded.temperature,
            Some(0.3),
            "call_with_retry_at_temp must stamp the explicit parameter \
             (0.3), NOT the per-provider config (0.99) and NOT the \
             per-role default (Sketch=1.0)"
        );
    }

    // ===========================================================
    // PR-C5: per-model JSON recovery strategy on the retry loop
    // ===========================================================
    //
    // Each test stands up a `RetryScript` mock provider whose
    // queue returns the responses the test wants the loop to
    // observe, then drives `call_with_retry_parse` with the
    // model name pinned to the strategy being tested.

    /// `Strict` strategy: a malformed payload fails fast — the
    /// dispatcher does NOT consult tolerant extraction, m3
    /// repair, continuation, or prefill. The retry loop returns
    /// the schema-violation error after the first attempt.
    #[tokio::test]
    async fn strategy_strict_fails_fast_on_malformed_payload() {
        let bad = "this is not json at all";
        let (temp, ctx, script) =
            retry_context_with_model("gpt-5.6-luna", vec![Ok((200, response(bad)))]);
        let result: Result<serde_json::Value> = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                "system".into(),
                "user".into(),
                "hint",
                0,
            )
            .await;
        assert!(
            result.is_err(),
            "Strict must fail fast on bad input, got: {result:?}"
        );
        // Exactly one provider call — no recovery retries.
        assert_eq!(
            script.calls.load(Ordering::SeqCst),
            1,
            "Strict must NOT retry on parse failure"
        );
        let _ = temp;
    }

    /// `Lenient` strategy: a prose-prefixed payload is recovered
    /// by the tolerant extractor. No retry needed; the loop
    /// succeeds on the first attempt.
    #[tokio::test]
    async fn strategy_lenient_recovers_prose_prefixed_payload() {
        let good = "Sure, here you go: {\"answer\": 42}";
        let (temp, ctx, script) =
            retry_context_with_model("kimi-k3", vec![Ok((200, response(good)))]);
        let result: serde_json::Value = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                "system".into(),
                "user".into(),
                "hint",
                0,
            )
            .await
            .expect("Lenient must recover the prose-prefixed JSON");
        assert_eq!(result, serde_json::json!({"answer": 42}));
        assert_eq!(
            script.calls.load(Ordering::SeqCst),
            1,
            "Lenient must NOT retry on successful recovery"
        );
        let _ = temp;
    }

    /// `Continuation` strategy: a malformed payload triggers the
    /// focused continuation re-call. The retry path issues the
    /// `Role::Continuation` call (which uses a separate script
    /// queue here) and parses its envelope. This test stubs
    /// the second call to return a valid continuation envelope.
    #[tokio::test]
    async fn strategy_continuation_re_calls_on_truncated_response() {
        // First call: truncated response that the dispatcher will
        // hand to `continue_truncated_response` to extend.
        let first = truncated_response("{\"answer\": 42");
        // The continuation re-call returns a JSON envelope with
        // the missing tail plus `finished: true`.
        let second = continuation_envelope(", \"trail\": true}", true);
        let (temp, ctx, script) =
            retry_context_with_model("minimax-m3", vec![Ok((200, first)), Ok((200, second))]);
        let result: serde_json::Value = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                "system".into(),
                "user".into(),
                "hint",
                0,
            )
            .await
            .expect("Continuation must stitch the truncated payload");
        assert_eq!(
            result,
            serde_json::json!({"answer": 42, "trail": true}),
            "stitched payload must round-trip"
        );
        assert!(
            script.calls.load(Ordering::SeqCst) >= 2,
            "Continuation must fire at least one re-call"
        );
        let _ = temp;
    }

    /// `PromptPrefill` strategy: when the first call returns
    /// genuinely malformed JSON (not recoverable by the lenient
    /// chain), the dispatcher retries ONCE with an assistant
    /// prefill of `{` appended via `Request::extra_messages`.
    /// The retry uses `call_uncached` so the prefill response
    /// does not poison the steady-state cache.
    #[tokio::test]
    async fn strategy_prompt_prefill_retries_with_assistant_brace() {
        // First call returns genuinely-broken JSON that the
        // lenient chain cannot repair.
        let bad = "totally not json: <<<>>>";
        // Second call (the prefill retry) returns valid JSON so
        // the parse pipeline recovers.
        let good = "{\"answer\": 42}";
        let (temp, ctx, script) = retry_context_with_model(
            "deepseek-v4-flash",
            vec![Ok((200, response(bad))), Ok((200, response(good)))],
        );
        let result: serde_json::Value = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                "system".into(),
                "user".into(),
                "hint",
                0,
            )
            .await
            .expect("PromptPrefill must recover via the prefill retry");
        assert_eq!(result, serde_json::json!({"answer": 42}));
        assert_eq!(
            script.calls.load(Ordering::SeqCst),
            2,
            "PromptPrefill must fire exactly one prefill retry"
        );
        let _ = temp;
    }
}
