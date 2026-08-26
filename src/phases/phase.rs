//! Pipeline phase trait. Each phase is a unit of work that reads the
//! artefacts left by the previous phase and writes new ones.
//!
//! Compliance: T01-06 §8 (non-discovery pipeline).
//! 10-integrada-v0 §D.12.1 defines `PhaseObject` and the layer graph;
//! the v0.1 MVP uses a flat `Vec<Box<dyn Phase>>` per the baseline.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::error::ProviderCause;
use crate::llm::circuit_breaker::BreakerRegistry;
use crate::llm::governor::{GovernorRegistry, ThrottleGovernor};

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
use crate::llm::models_dev::ModelsDevCatalog;
use crate::llm::param_rejections::{
    PARAM_NAMES, ParamRejectionsTable, audit_unknown_fields, detect_all_rejections,
};
use crate::llm::probe_table::MaxTokensTable;
use crate::llm::prompt_cache::PromptCache;
use crate::llm::prompts::DEFAULT_MAX_TOKENS;
use crate::llm::prompts::top_p_for_role;
use crate::llm::response_format_opt_out::render_system_prompt_with_prefix;
use crate::llm::temperature_probe::TemperatureTable;
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
    /// Per-role rate-limiters, keyed by `Role`. Empty by default =
    /// no per-role limit (the per-provider bucket still applies).
    /// Wired by the CLI boundary from `cfg.rate_limit_per_role` via
    /// [`Self::with_role_rate_limits`]. Acquired once per LLM call
    /// by `call_with_retry` / `call_uncached` before dispatching to
    /// the provider so the operator can throttle a chatty role
    /// (e.g. `tagger` in the post-matrix fan-out) without touching
    /// the per-provider bucket.
    pub rate_limit_per_role:
        std::collections::HashMap<Role, Arc<crate::llm::rate_limiter::RateLimiter>>,
    /// v0.9.6: adaptive throttle governors keyed by
    /// `(provider, role)`. Each LLM call consults the governor for
    /// `(self.default_provider, role)` to apply a per-role adaptive
    /// backoff on transient 429s. The governor is the consumer of
    /// `Error::Throttled` errors; the breaker keyed on
    /// `(provider, role)` (see [`Self::breaker_per_role`]) consumes
    /// `Error::PlanExhausted`. Two-tier separation matches the spec
    /// in `docs/adr/<throttle-governor>`.
    pub throttle: GovernorRegistry,
    /// v0.9.6: per-`(provider, role)` circuit breakers consumed by
    /// `RunContext::call_*` for `PlanExhausted` errors. The earlier
    /// per-provider breaker on `BreakeredProvider` was removed
    /// because it caused the cascade in `discover_facet` (one role's
    /// 429 tripped every role on the same provider). Per-role
    /// scoping keeps each role isolated.
    pub breaker_per_role: BreakerRegistry,
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
    /// PR-7: shared handle into the auto-probe supported-temperatures
    /// table. When `Some`, [`Self::dispatch_to_provider`] consults the
    /// table on every LLM call and clamps `req.temperature` to the
    /// nearest value in the discovered set, emitting a
    /// `tracing::warn!` when the operator's requested temperature
    /// was out of range. `None` disables the clamp so legacy
    /// hand-rolled paths keep the "send whatever the caller asked
    /// for" behaviour. The CLI boundary (`cli::run`) and the
    /// integration tests populate this field; unit tests that
    /// exercise the pre-clamp behaviour leave it as `None`.
    pub temperature_table: Option<Arc<TemperatureTable>>,
    /// PR-3: capability resolver consulted on every LLM call so the
    /// models.dev catalog can drop fields the upstream would reject
    /// (e.g. `temperature` on `kimi-k3`). `None` disables every
    /// capability-aware gate and keeps the legacy "send everything"
    /// behaviour — the same fallback the `max_tokens_table` field
    /// uses. The CLI boundary (`cli::run`) and the integration
    /// tests populate this field; unit tests that exercise the
    /// pre-capability behaviour leave it as `None`.
    pub capability_resolver: Option<Arc<CapabilityResolver>>,
    /// Wire-the-gates plan: handle to the on-disk `models.dev`
    /// catalog refreshed at CLI startup (`cli::run`). `None` means
    /// no catalog is loaded (fresh home, network failure, tests
    /// that skip the refresh); every gate that depends on the
    /// catalog (`ModalityGate::apply`, `cost_estimate`) falls
    /// through to its no-op default in that case. The CLI
    /// boundary populates this field by calling
    /// [`crate::llm::models_dev::load_or_fetch`]; integration tests
    /// and the legacy `moagan run --provider mock` flow leave it
    /// as `None`.
    pub models_dev_catalog: Option<Arc<ModelsDevCatalog>>,
    /// Self-healing param-rejection table. When `Some`,
    /// [`Self::dispatch_to_provider`] consults it before every LLM
    /// call to omit wire fields the upstream rejected on a previous
    /// call, and after every 4xx to auto-detect + record any new
    /// rejection. `None` disables both behaviours (legacy
    /// hand-rolled paths and tests that bypass the registry wiring).
    /// The CLI boundary (`cli::run` / `cli::discover`) populates
    /// this field; unit tests that exercise the pre-omit behaviour
    /// leave it as `None`.
    pub param_rejections: Option<Arc<ParamRejectionsTable>>,
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
        tracing::debug!(
            %run_id,
            default_provider = %default_provider,
            default_model = %default_model,
            mode = %mode,
            "RunContext: new"
        );
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
            rate_limit_per_role: std::collections::HashMap::new(),
            throttle: GovernorRegistry::new(),
            breaker_per_role: BreakerRegistry::new(),
            heartbeat_interval_secs: DEFAULT_HEARTBEAT_INTERVAL_SECS,
            heartbeat_holder: "heartbeat".to_owned(),
            heartbeat_handle: Arc::new(parking_lot::Mutex::new(None)),
            max_tokens_table: None,
            temperature_table: None,
            capability_resolver: None,
            models_dev_catalog: None,
            param_rejections: None,
        }
    }

    /// Attach the per-role rate-limiters wired by the CLI boundary
    /// from `cfg.rate_limit_per_role`. Each entry is a `Role`
    /// key (parsed from the operator's snake_case string) plus the
    /// `Arc<RateLimiter>` that `call_with_retry` / `call_uncached`
    /// acquire before the provider dispatch. Empty map (the
    /// default) means no per-role limit — the per-provider bucket
    /// still applies as before.
    pub fn with_role_rate_limits(
        mut self,
        rate_limit_per_role: std::collections::HashMap<
            Role,
            Arc<crate::llm::rate_limiter::RateLimiter>,
        >,
    ) -> Self {
        self.rate_limit_per_role = rate_limit_per_role;
        self
    }

    /// v0.9.6: attach the per-`(provider, role)` adaptive throttle
    /// governors wired by the CLI boundary from
    /// `cfg.throttle_per_role`. The empty default means no
    /// throttle — `GovernorRegistry::governor_for(role)` lazily
    /// creates a default-config governor the first time any role
    /// is called, so omitting this keeps the v0.9.5 behaviour
    /// (no adaptive backpressure, no governor-side telemetry).
    pub fn with_throttle_governors(mut self, throttle: GovernorRegistry) -> Self {
        self.throttle = throttle;
        self
    }

    /// v0.9.6: attach the per-`(provider, role)` breaker registry
    /// wired by the CLI boundary from `cfg.circuit_breaker_per_role`.
    /// The default-constructed [`BreakerRegistry`] uses
    /// lenient defaults (50/300/60) matching the v0.9.4
    /// per-provider breaker.
    pub fn with_breakers_per_role(mut self, breakers: BreakerRegistry) -> Self {
        self.breaker_per_role = breakers;
        self
    }

    /// v0.9.6: lookup (or lazily create) the breaker for
    /// `(default_provider, role)`. Used by `call_with_retry` to
    /// fast-fail when PlanExhausted was tripped on a previous
    /// call, and by `call_*_at_temp` to record persistent
    /// failures.
    pub fn breaker_for(&self, role: Role) -> crate::llm::circuit_breaker::CircuitBreaker {
        self.breaker_per_role
            .breaker_for(&self.default_provider, role)
    }

    /// v0.9.6: lookup (or lazily create) the throttle governor
    /// for `(default_provider, role)`. The same `Arc` is returned
    /// across all callers so the AIMD state is consistent.
    pub fn governor_for(&self, role: Role) -> Arc<ThrottleGovernor> {
        self.throttle.governor_for(&self.default_provider, role)
    }

    /// v0.9.6: shared AIMD-throttle / per-(provider, role)-breaker
    /// pre-call + post-call wrapper used by every `call_*` method.
    /// Consults the breaker first (fast-fail when PlanExhausted was
    /// tripped on a previous call for this pair), sleeps for the
    /// governor's adaptive backoff, runs the inner dispatch, then
    /// updates both governors based on the result. Centralising the
    /// state machine here keeps the four `call_*` methods free of
    /// the same boilerplate.
    ///
    /// Cascade-avoidance: when the breaker is already open at the
    /// start of the call we return early with the synthetic
    /// "circuit open" error — that error is *self-inflicted*, not
    /// a signal of a real provider failure, so the post-call
    /// `record_failure()` would just re-arm the breaker and feed
    /// itself. We capture `was_open` pre-call and only record
    /// PlanExhausted on the failure path when the breaker was
    /// closed at the start of the call.
    async fn dispatch_with_governors<F, T>(&self, role: Role, inner: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let was_open = self.breaker_for(role).is_open();
        if was_open {
            return Err(Error::PlanExhausted {
                message: format!(
                    "circuit open: provider '{}' role '{}' sidelined",
                    self.default_provider,
                    role.as_str()
                ),
                http_status: None,
            });
        }
        let governor = self.governor_for(role);
        let _throttle_sleep = governor.pre_call().await;
        let result = inner.await;
        match &result {
            Ok(_) => governor.on_success(),
            Err(e) => match (e.provider_cause(), was_open) {
                // 429 throttle: never trips the breaker (per-role throttle
                // governor handles it via AIMD backoff). v0.9.8 also
                // routes `PlanExhausted` here for the same reason —
                // `Error::is_circuit_opening()` no longer returns true
                // for `PlanExhausted` and the upstream does not let us
                // tell saturation from true quota exhaustion, so the
                // breaker stays reserved for unambiguous 5xx/4xx-auth/
                // timeout signals.
                (Some(ProviderCause::Throttled { retry_after, .. }), _) => {
                    governor.on_transient_429(retry_after.map(std::time::Duration::from_millis));
                }
                (Some(ProviderCause::PlanExhausted { .. }), _) => {
                    // Self-inflicted: do nothing on the breaker path.
                    // The throttle governor already saw the 429 via
                    // `pre_call`; counting it again here would just
                    // feed the same code path the user already saw
                    // saturate the breaker in v0.9.7.
                }
                _ => {}
            },
        }
        result
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

    /// PR-7: attach the auto-probe supported-temperatures table so
    /// [`Self::dispatch_to_provider`] can consult it on every LLM
    /// call and clamp `req.temperature` to the nearest value in the
    /// discovered set. The CLI boundary (`cli::run`) and the
    /// integration tests call this once after `registry_from_config`
    /// has built the table; tests that exercise the pre-clamp
    /// behaviour leave the field as `None` so the legacy "send
    /// whatever the operator asked for" path stays bit-for-bit.
    pub fn with_temperature_table(mut self, table: Arc<TemperatureTable>) -> Self {
        self.temperature_table = Some(table);
        self
    }

    /// PR-7: optional variant of [`Self::with_temperature_table`]
    /// for the `Option<Arc<TemperatureTable>>` carried by
    /// [`ProviderRegistry`]. No-op when the table is `None`
    /// (mock-only registries and tests that bypass the probe).
    pub fn with_temperature_table_opt(mut self, table: Option<Arc<TemperatureTable>>) -> Self {
        if let Some(t) = table {
            self.temperature_table = Some(t);
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

    /// Wire-the-gates plan: attach the on-disk `models.dev` catalog
    /// so the modality gate and the cost estimator can resolve
    /// `(provider, model)` rows on every LLM call. Builder form
    /// mirrors [`Self::with_max_tokens_table`] / [`Self::with_capability_resolver`]
    /// so the CLI boundary can chain it next to the other
    /// runtime handles. Tests that exercise the pre-catalog
    /// behaviour leave the field as `None`.
    pub fn with_models_dev_catalog(mut self, catalog: Arc<ModelsDevCatalog>) -> Self {
        self.models_dev_catalog = Some(catalog);
        self
    }

    /// Wire-the-gates plan: optional variant of
    /// [`Self::with_models_dev_catalog`] for callers that already
    /// hold an `Option<Arc<...>>`. No-op when the catalog is
    /// `None` (network failure on first call, a test that skipped
    /// the refresh) so the legacy "no catalog" code path keeps
    /// working untouched.
    pub fn with_models_dev_catalog_opt(mut self, catalog: Option<Arc<ModelsDevCatalog>>) -> Self {
        if let Some(c) = catalog {
            self.models_dev_catalog = Some(c);
        }
        self
    }

    /// Self-healing param rejection: attach the table so
    /// [`Self::dispatch_to_provider`] can pre-call omit and post-call
    /// detect. Mirrors [`Self::with_max_tokens_table`] — the
    /// consuming builder form lets the CLI boundary chain it next
    /// to the other auto-discovered handles.
    pub fn with_param_rejections(mut self, table: Arc<ParamRejectionsTable>) -> Self {
        self.param_rejections = Some(table);
        self
    }

    /// Self-healing param rejection: optional variant of
    /// [`Self::with_param_rejections`] for callers that already hold
    /// an `Option<Arc<...>>` (the `ProviderRegistry::param_rejections`
    /// accessor). No-op when the table is `None` so legacy
    /// hand-rolled paths stay bit-for-bit.
    pub fn with_param_rejections_opt(mut self, table: Option<Arc<ParamRejectionsTable>>) -> Self {
        if let Some(t) = table {
            self.param_rejections = Some(t);
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
        // v0.10: the registry keys every (section, model_id) pair
        // under `"{section}::{model_id}"` unless section ==
        // model_id. Look up the joined key from `(default_provider,
        // default_model)` first; fall back to the bare section name
        // for legacy single-instance callers (hand-rolled test
        // fixtures that register the mock under `"mock"` directly
        // instead of `"mock::mock-model"`).
        let joined =
            crate::llm::ProviderRegistry::registry_key(&self.default_provider, &self.default_model);
        if let Some(p) = self.providers.get(&joined) {
            return p;
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
            max_tokens: Some(max_tokens_for_role(role)),
            temperature: Some(resolve_temperature(
                role,
                profile_overrides,
                provider_temperature,
            )),
            top_p: resolve_top_p(role, provider_top_p),
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
        // Per-role rate-limit (catalog §D.19.6): acquire the bucket
        // for this role before the upstream dispatch. The token
        // sleep serializes the chatty roles (e.g. `tagger` in the
        // post-matrix fan-out) so the upstream provider's quota is
        // respected regardless of `--max-parallelism`. The
        // per-provider bucket still applies inside the wrapper, so
        // the two limits compound: the per-role bucket throttles the
        // role, the per-provider bucket throttles the wire.
        if let Some(rl) = self.rate_limit_per_role.get(&role) {
            let _wait = rl.acquire().await?;
        }
        // v0.9.6: fast-fail when the per-`(provider, role)`
        // breaker is open from a previous `PlanExhausted`. The
        // record-update happens after the dispatch returns, inside
        // the `match` block below. Centralised in
        // `dispatch_with_governors` so the four `call_*` methods
        // share the same AIMD + breaker state machine.
        let dispatch_result = self
            .dispatch_with_governors(role, async {
                self.dispatch_to_provider(req, Some(cache_key.clone()), started_unix, retry_count)
                    .await
            })
            .await;
        dispatch_result.inspect(|_response| {
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
    ///
    /// `pub` (not `pub(crate)`) so the integration tests in
    /// `tests/integration_temperature_clamp.rs` can drive the
    /// dispatch gate end-to-end without a hand-rolled wrapper.
    /// The method is already consumed by `discover_matrix.rs` and
    /// `coordinator.rs` as part of the matrix fan-out; widening
    /// the visibility is the minimum-surface change that
    /// preserves the production contract while exposing the seam
    /// the regression tests need.
    pub async fn call_with_retry_at_temp(
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
            max_tokens: Some(max_tokens_for_role(role)),
            temperature: Some(temperature),
            top_p: resolve_top_p(role, provider_top_p),
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
        // Per-role rate-limit (catalog §D.19.6): acquire the bucket
        // for this role before the upstream dispatch. The token
        // sleep serializes the chatty roles (e.g. `tagger` in the
        // post-matrix fan-out) so the upstream provider's quota is
        // respected regardless of `--max-parallelism`. The
        // per-provider bucket still applies inside the wrapper, so
        // the two limits compound: the per-role bucket throttles the
        // role, the per-provider bucket throttles the wire.
        if let Some(rl) = self.rate_limit_per_role.get(&role) {
            let _wait = rl.acquire().await?;
        }
        // v0.9.6: AIMD-throttle + per-(provider, role)-breaker
        // wrapper. See `dispatch_with_governors` for the full state
        // machine; the four `call_*` methods all share it.
        self.dispatch_with_governors(role, async {
            self.dispatch_to_provider(req, Some(cache_key.clone()), started_unix, retry_count)
                .await
        })
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
            max_tokens: Some(max_tokens_for_role(role)),
            temperature: Some(temperature),
            top_p: resolve_top_p(role, provider_top_p),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        // Per-role rate-limit (catalog §D.19.6): mirror the
        // acquire in `call_with_retry` so the retry path (which
        // bypasses the cache) honours the same per-role bucket.
        if let Some(rl) = self.rate_limit_per_role.get(&role) {
            let _wait = rl.acquire().await?;
        }
        // v0.9.6: AIMD-throttle + breaker. See `dispatch_with_governors`.
        self.dispatch_with_governors(role, async {
            self.dispatch_to_provider(req, None, started_unix, retry_count)
                .await
        })
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
            max_tokens: Some(max_tokens_for_role(role)),
            temperature: Some(resolve_temperature(
                role,
                profile_overrides,
                provider_temperature,
            )),
            top_p: resolve_top_p(role, provider_top_p),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        // Per-role rate-limit (catalog §D.19.6): mirror the
        // acquire in `call_with_retry` so the retry path (which
        // bypasses the cache) honours the same per-role bucket.
        if let Some(rl) = self.rate_limit_per_role.get(&role) {
            let _wait = rl.acquire().await?;
        }
        // v0.9.6: AIMD-throttle + breaker. See `dispatch_with_governors`.
        self.dispatch_with_governors(role, async {
            self.dispatch_to_provider(req, None, started_unix, retry_count)
                .await
        })
        .await
    }

    /// Send the prepared request to the provider, record telemetry,
    /// emit truncation / empty warnings, and (when a `cache_key` is
    /// supplied) persist the response in the cross-run cache.
    async fn dispatch_to_provider(
        &self,
        mut req: Request,
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
        // Wire-the-gates plan, PR-5 follow-up: gate the request
        // through the modality gate so the wire body reflects the
        // catalog's per-model capabilities (attachments, tool
        // choice, input modalities). The gate mutates `req` in
        // place so the body that `provider.send` transmits AND the
        // hash below both observe the gate's effects. A missing
        // catalog row falls through to the conservative default
        // (text-only, no attachments, no tool calls) so a stale
        // snapshot cannot widen the set of capabilities the gate
        // allows.
        if let Some(catalog) = self.models_dev_catalog.as_ref()
            && let Some(entry) = crate::llm::models_dev::lookup(
                catalog,
                self.default_provider.as_str(),
                self.default_model.as_str(),
            )
        {
            let gate = crate::llm::modal_gate::ModalityGate::from_entry(&entry);
            if let Err(e) = gate.apply(&mut req) {
                let ended_unix = crate::time::now_unix_secs();
                let phase_name = req.role.as_str();
                let _ = self.telemetry.call(
                    &uuid::Uuid::now_v7().to_string(),
                    phase_name,
                    phase_name,
                    self.default_provider.as_str(),
                    self.default_model.as_str(),
                    cache_key.as_deref().unwrap_or(""),
                    None,
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
                );
                return Err(e);
            }
        }
        // PR-7: clamp `req.temperature` to the nearest value in
        // the operator's auto-discovered supported set for
        // `(default_provider, default_model)`. The CLI boundary
        // already rewrote the matrix profile for the discovery
        // path (`discovery::coordinator`); this gate is the safety
        // net for every other path (per-role default, profile
        // override, legacy callers that pass `req.temperature =
        // Some(_)` directly). Runs BEFORE the capability resolver
        // because the resolver drops `temperature` outright on
        // models that don't support it (e.g. `kimi-k3`) — clamping
        // first means we still log the out-of-range value when it
        // happens, even though the wire body will drop the field.
        if let (Some(t), Some(table)) = (req.temperature, self.temperature_table.as_ref())
            && let Some(clamped) =
                table.nearest_supported(&self.default_provider, &self.default_model, t)
        {
            if (clamped - t).abs() > f32::EPSILON {
                tracing::warn!(
                    provider = %self.default_provider,
                    model = %self.default_model,
                    role = %req.role.as_str(),
                    requested = %t,
                    clamped_to = %clamped,
                    "temperature outside supported set; clamped at dispatch (safety net)"
                );
            } else {
                // PR-7 (operator-visibility): the operator wants to confirm
                // that the temperature they declared in the matrix profile
                // is the temperature the runtime actually sends. Trace-level
                // so it stays silent at INFO; operators that want to
                // validate the clamp end-to-end set
                // `RUST_LOG=moagan::phases::phase=trace`.
                tracing::trace!(
                    provider = %self.default_provider,
                    model = %self.default_model,
                    role = %req.role.as_str(),
                    temperature = %t,
                    "temperature in supported set; no clamp"
                );
            }
            req.temperature = Some(clamped);
        }
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
        // `effective_max_tokens` treats `None` as `u32::MAX`, so we
        // must compare in the same domain. When `req.max_tokens` is
        // `None` (the auto-healing path) the two are equal and the
        // clamp is a no-op.
        let gated_audit = gated.max_tokens.unwrap_or(u32::MAX);
        let hash_input = if gated_audit != effective_max {
            let mut clamped = gated;
            clamped.max_tokens = Some(effective_max);
            clamped
        } else {
            gated
        };
        // Self-healing param rejection: omit wire fields the
        // upstream rejected on a previous call (per the
        // persisted `param_rejections.toml`). Runs AFTER every
        // other gate so the audit hash and the wire body stay in
        // lock-step: a field the capability resolver already
        // dropped (e.g. `temperature` on `kimi-k3`) does not
        // produce a double-omit; a field the temperature table
        // already clamped stays clamped. `None` table preserves
        // the legacy "send everything" path.
        let mut hash_input = hash_input;
        if let Some(table) = self.param_rejections.as_ref() {
            let mut omitted: Vec<&str> = Vec::new();
            for param in PARAM_NAMES {
                if table.should_omit(
                    self.default_provider.as_str(),
                    self.default_model.as_str(),
                    param,
                ) {
                    crate::llm::wire::omit_param(&mut hash_input, param);
                    omitted.push(param);
                }
            }
            if !omitted.is_empty() {
                tracing::debug!(
                    provider = %self.default_provider,
                    model = %self.default_model,
                    role = %req.role.as_str(),
                    omitted = ?omitted,
                    "omitted known-rejected params before dispatch"
                );
            }
        }
        let request_body_sha256 = (self.default_provider == "minimax")
            .then(|| crate::llm::http::request_body_sha256(&hash_input))
            .transpose()?;
        // Silent-acceptance audit: emit a WARN per non-standard
        // field on the serialised wire body. Some upstreams
        // swallow unknown fields and behave inconsistently on the
        // next call; the audit hint lets operators spot the
        // configuration drift in the run logs. The whitelist
        // lives in `crate::llm::param_rejections::audit_unknown_fields`.
        if let Ok(value) = serde_json::to_value(&hash_input) {
            audit_unknown_fields(&value);
        }
        let call_id = uuid::Uuid::now_v7().to_string();
        let provider_started = std::time::Instant::now();
        tracing::debug!(
            call_id = %call_id,
            phase = req.role.as_str(),
            stage = "provider.send.started",
            retry_count,
            "LLM call stage"
        );
        let mut result = provider.send(&hash_input).await;
        // Self-healing cascade retry: when the upstream rejects
        // wire fields with HTTP 4xx and the body carries one or
        // more rejection signatures, omit every detected name and
        // retry. The legacy single-shot loop only recorded the
        // first match — a single response that lists
        // `"Unknown parameters: 'temperature', 'max_tokens',
        // 'top_p'"` (the canonical `gpt-5.6-luna` cascade) lost
        // the other two names and the next round-trip failed
        // again with the same body, propagating the error to the
        // caller. The bounded `while` below closes the gap by
        // consulting [`detect_all_rejections`] once per iteration
        // and persisting every name in one pass, capped at
        // [`PARAM_NAMES`] entries so an upstream that loops the
        // same response body can never starve the dispatcher.
        let max_rejection_retries = PARAM_NAMES.len();
        let mut rejection_attempts = 0;
        while rejection_attempts < max_rejection_retries {
            // Pull the HTTP status out of the latest result; abort
            // the cascade on any non-4xx or transport-layer
            // failure (the upstream either succeeded or hit a
            // transient error the breaker/governor handles
            // separately — neither is in scope for this loop).
            let status = match result.as_ref().err().and_then(|e| e.http_status()) {
                Some(s) if (400..500).contains(&s) => s,
                _ => break,
            };
            let err = result.as_ref().expect_err("status set implies Err");
            let body = parse_provider_error_body(err, status);
            let detected = detect_all_rejections(status, body.as_ref());
            if detected.is_empty() {
                // The 4xx is something other than a param
                // rejection (auth, model-not-found, generic
                // upstream error). Surface it to the caller
                // untouched.
                break;
            }
            for detected_param in &detected {
                tracing::info!(
                    provider = %self.default_provider,
                    model = %self.default_model,
                    role = %req.role.as_str(),
                    detected_param = %detected_param,
                    "auto-detected param rejection; retrying without it"
                );
                if let Some(table) = self.param_rejections.as_ref()
                    && let Err(rec_err) = table.record(
                        self.default_provider.as_str(),
                        self.default_model.as_str(),
                        detected_param,
                    )
                {
                    tracing::warn!(
                        error = %rec_err,
                        "failed to persist param rejection; in-memory entry still kept"
                    );
                }
                crate::llm::wire::omit_param(&mut hash_input, detected_param);
            }
            // Re-run the silent-acceptance audit on the post-omit
            // body so the operator sees the diagnostic for the
            // body that actually reaches the upstream on the
            // retry.
            if let Ok(value) = serde_json::to_value(&hash_input) {
                audit_unknown_fields(&value);
            }
            tracing::debug!(
                call_id = %call_id,
                phase = req.role.as_str(),
                stage = "provider.send.retry",
                detected_params = ?detected,
                "LLM call stage"
            );
            rejection_attempts += 1;
            result = provider.send(&hash_input).await;
            if result.is_ok() {
                break;
            }
        }
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
                    // Wire-the-gates plan, PR-6 follow-up: write
                    // the per-call USD estimate to the SQLite
                    // index so `moagan telemetry cost` returns
                    // real numbers instead of always zero. The
                    // catalog is the source of truth for the
                    // rate; a missing catalog or missing
                    // `(provider, model)` row returns 0.0 and the
                    // `record_call_cost` helper itself skips the
                    // UPDATE for zero/NaN so the column stays
                    // `NULL` (not "zero dollars billed") on
                    // unknown models.
                    if let Some(db) = self.telemetry.db() {
                        let cost_usd = crate::llm::cost::cost_estimate(
                            self.models_dev_catalog.as_deref(),
                            self.default_provider.as_str(),
                            self.default_model.as_str(),
                            &response.usage,
                        );
                        if let Err(e) = db.record_call_cost(&call_id, cost_usd) {
                            tracing::warn!(
                                call_id = %call_id,
                                phase = phase_name,
                                stage = "cost.record.error",
                                error = %e,
                                "LLM call stage"
                            );
                        }
                    }
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
                    e.http_status(),
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
            max_tokens: Some(0),
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
            max_tokens: Some(crate::phases::phase::max_tokens_for_role(role)),
            temperature: None,
            top_p: None,
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
        req.top_p = resolve_top_p(role, provider_top_p);
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
    /// HTTP error, or after `max_continuation_attempts` re-calls.
    ///
    /// The cap is **passed in by the caller**, sourced from
    /// [`crate::llm::json_strategy::max_continuation_attempts`] for
    /// the resolved [`crate::llm::json_strategy::JsonRecoveryStrategy`].
    /// In production this is `2` for
    /// [`JsonRecoveryStrategy::Continuation`](crate::llm::json_strategy::JsonRecoveryStrategy::Continuation)
    /// (the D.21.6 default) and `0` for every other strategy. The
    /// call site gates invocation on `max_continuation_attempts > 0`,
    /// so a `0` value is a no-op signal — the original (truncated)
    /// response falls through to the parse-failure retry budget
    /// instead of going through this loop. After the cap is reached
    /// (or any other termination branch fires), the helper preserves
    /// `truncated = true` so the existing parse path sees the same
    /// input it sees today.
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
        max_continuation_attempts: u8,
    ) -> Response {
        use crate::domain::ContinuationReport;
        use crate::llm::prompts::render_continuation_prompt;

        const EXCERPT_BYTES: usize = 500;

        let mut accumulated = original.text.clone();
        let mut truncated = original.truncated;
        let mut last_finish_reason = original.finish_reason.clone();
        let mut total_input = original.usage.input_tokens;
        let mut total_output = original.usage.output_tokens;
        let mut total_cache_read = original.usage.cache_read;
        let mut total_cache_creation = original.usage.cache_creation;

        let mut attempt_idx: u8 = 0;
        while attempt_idx < max_continuation_attempts && truncated {
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
                max_tokens: Some(max_tokens_for_role(Role::Continuation)),
                temperature: Some(temperature_for_role(Role::Continuation, None)),
                top_p: resolve_top_p(Role::Continuation, provider_top_p),
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
    /// - `Fast`, `Explore`, `Batch` parse / schema: 5 attempts with
    ///   the JSON repair pass (was 1; pre-fix budget propagated
    ///   upstream non-determinism as fatal `SchemaViolation`).
    /// - `Fast`, `Explore`, `Batch` transport / rate-limit / timeout:
    ///   3 attempts (was 1).
    /// - `Fast`, `Explore`, `Batch` truncated: 1 attempt (the model
    ///   already cut output; a re-issue would truncate again).
    /// - `Standard` parse / schema: 5 attempts with repair.
    /// - `Standard` rate-limit: 4 attempts (one extra over the
    ///   transient baseline for quota headroom).
    /// - `Standard` transport / timeout: 3 attempts.
    /// - `Standard` truncated: 2 attempts.
    /// - `Deep` parse / schema: 5 attempts with repair.
    /// - `Deep` rate-limit: 6 attempts (most generous row; deep
    ///   restart is expensive).
    /// - `Deep` transport / timeout: 4 attempts.
    /// - `Deep` truncated: 2 attempts.
    ///
    /// Callers that explicitly want a lower cap (single-shot tests,
    /// mocks that need deterministic failure on attempt N) can pass
    /// a smaller `max_retries`; the ceiling is a safety net, not a
    /// guarantee — the budget only widens the loop's window, it
    /// never narrows the caller's request.
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
            // runs on the stitched text. The cap and the dispatch
            // gate both come from
            // [`crate::llm::json_strategy::max_continuation_attempts`]
            // for the resolved strategy: `Continuation` returns `2`
            // (D.21.6 default), every other strategy returns `0`
            // and the helper is skipped — the truncated response
            // then falls through to the normal parse-failure retry
            // budget. After the cap is reached the helper preserves
            // `truncated = true` so the existing parse path sees the
            // same input it sees today (just one-shot, no retry).
            let max_cont = json_strategy::max_continuation_attempts(strategy);
            let response = match response {
                Ok(resp) if resp.truncated && !resp.text.is_empty() && max_cont > 0 => Ok(self
                    .continue_truncated_response(role, &resp, started_unix, max_cont)
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
        || matches!(err, Error::Provider { .. } | Error::PlanExhausted { .. })
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
        // A#11: discovery-mode LLM-as-judge. The detector emits
        // a findings array; the prompt + schema together bound
        // the response, but we keep the 1M ceiling so a model
        // that wants to surface very long evidence excerpts is
        // not artificially truncated.
        Role::ContradictionJudge => DEFAULT_MAX_TOKENS,
        // F1 (Track G.2 `discover_dimensions`): the
        // dimensions-deriver returns a `Dimensions` envelope with
        // 2-6 dimension triples (each carrying 1-5 facet triples
        // with descriptions). The unified 1M ceiling keeps room
        // for the LLM to surface descriptions without truncation.
        Role::DimensionDeriver => DEFAULT_MAX_TOKENS,
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
        // A#11: discovery-mode LLM-as-judge. T=0.0 so two runs
        // over the same `(focal, candidates)` set produce
        // identical findings — cluster-snapshot diffs rely on
        // the call being stable. top_p is resolved by
        // `role_settings` (0.2) so the call layer doesn't
        // special-case the role.
        Role::ContradictionJudge => 0.0,
        // F1 (Track G.2 `discover_dimensions`): deterministic
        // T=0.0 so two runs against the same brief produce
        // identical dimension lists — the
        // `discovery_dimensions.json` sidecar relies on this
        // for cache-key stability.
        Role::DimensionDeriver => 0.0,
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

/// PR-C3: resolve `top_p` for an LLM call by precedence.
///
/// Precedence (highest first):
///
/// 1. `provider_top_p` (when `Some`) — the operator's override from
///    `[providers.<name>].top_p` in the user's TOML. Wins over the
///    catalogue so the operator can pin a per-provider value without
///    editing the role settings.
/// 2. [`top_p_for_role`] (when `Some`) — the catalogue value
///    registered in [`crate::llm::prompts::role_settings`]. Honours
///    T01-06 §4.2: every role that ships a `RoleSettings` declares
///    its sampling contract.
/// 3. `None` — when neither the provider nor the role declare
///    `top_p`, the field is omitted from the wire entirely via
///    `skip_serializing_if = "Option::is_none"`. This replaces the
///    legacy `unwrap_or(0.95)` that injected a forced 0.95 onto the
///    wire even when no configuration asked for it — a behaviour
///    that crashed any upstream rejecting `top_p`.
///
/// Without this helper, a role like `Role::Sketch` (no
/// `RoleSettings`) plus a provider with `top_p = None` would have
/// forced `Some(0.95)` onto the wire and triggered an immediate
/// 4xx on relays that reject `top_p`. With it, both `None`s
/// collapse to `None` and the wire omits the field end-to-end.
pub fn resolve_top_p(role: Role, provider_top_p: Option<f32>) -> Option<f32> {
    if let Some(p) = provider_top_p {
        return Some(p);
    }
    top_p_for_role(role)
}

/// Strip the transport envelope off a provider-error message so the
/// param-rejection detector parses JSON rather than a labelled
/// string. The error's `Display` is `provider error: {message}`
/// where `message` is built by [`crate::llm::http::classify_status`]
/// as `format!("http {status}: {body}")` with `{status}` formatted
/// via `reqwest::StatusCode`'s `Display` (which expands to
/// `"400 Bad Request"`, not the bare integer). The helper tries the
/// strict envelope first (`provider error: http {status}: `, status
/// as integer — what the scripted provider in tests produces and
/// what the legacy single-shot loop expected), then falls back to
/// the production envelope (`provider error: http {status} <reason
/// phrase>: `), then to `provider error: ` (catch-all when the
/// upstream's body itself starts with the status line), then to a
/// JSON `error.message` extraction, and finally returns the raw
/// `Display` so the detector at least gets *something* to chew on
/// when none of the envelopes match.
pub(crate) fn parse_provider_error_body(err: &Error, status: u16) -> String {
    let raw: String = err.to_string();
    // Strict envelope (legacy / scripted providers): "provider error: http 400: <body>"
    let strict = format!("provider error: http {status}: ");
    if let Some(stripped) = raw.strip_prefix(&strict) {
        return stripped.to_owned();
    }
    // Production envelope (reqwest::StatusCode Display expands to
    // "400 Bad Request"): "provider error: http 400 Bad Request: <body>".
    // The reason phrase is whatever `StatusCode::reason_phrase()`
    // returns — typically one or two ASCII words. Scan for ": "
    // AFTER the "provider error: http " marker so the first `": "`
    // (which lives between "error" and "http") does not pull us
    // out of position. The head slice between the marker and the
    // delimiter must start with the status digits so a stray
    // `": "` deeper in the body cannot be mistaken for the
    // envelope terminator.
    if let Some(http_idx) = raw.find("provider error: http ") {
        let after_http = http_idx + "provider error: http ".len();
        if let Some(colon_offset) = raw[after_http..].find(": ") {
            let colon_idx = after_http + colon_offset;
            let head = &raw[after_http..colon_idx];
            if head.len() >= 3 && head.as_bytes()[..3] == *format!("{status:03}").as_bytes() {
                return raw[colon_idx + 2..].to_owned();
            }
        }
    }
    // Catch-all: drop just the "provider error: " prefix and let
    // the detector try to parse whatever's left.
    if let Some(stripped) = raw.strip_prefix("provider error: ") {
        return stripped.to_owned();
    }
    // Last resort: try to parse the raw as JSON and surface the
    // `error.message` field verbatim. Some transports don't wrap
    // the body at all (e.g. an upstream that returns the JSON
    // envelope directly without the `provider error: http NNN: `
    // prefix).
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
        && let Some(msg) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
    {
        return msg.to_owned();
    }
    raw
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
    /// F1 (Track G.2 `discover_dimensions`):
    /// `<run_dir>/discovery_dimensions.json` was written. Empty
    /// path means the phase was skipped (e.g. the operator
    /// supplied `--matrix-spec` and the LLM-derive path was
    /// short-circuited). The path is the sidecar the matrix
    /// phase reads so the matrix fan-out reuses the same
    /// dimensions without re-issuing the LLM call.
    DiscoveryDimensions(PathBuf),
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
        let (_temp, ctx, script) = retry_context(vec![Err(Error::InvalidApiKey {
            message: "bad".into(),
            http_status: None,
        })]);
        let result = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                String::new(),
                String::new(),
                "Value",
                5,
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidApiKey { .. })));
        assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn call_with_retry_parse_retries_on_retriable_error() {
        let (_temp, ctx, script) = retry_context(vec![
            Err(Error::Provider {
                message: "temporary outage".into(),
                http_status: None,
            }),
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
                0,
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
                0,
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
                0,
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
                0,
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
                0,
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
    ///
    /// The test pins the helper for the `Continuation` strategy,
    /// so the dispatch gate
    /// ([`crate::llm::json_strategy::max_continuation_attempts`])
    /// fires — we drive the context with `minimax-m3`, the
    /// canonical `Continuation` model.
    #[tokio::test]
    async fn phase_continuation_loop_fires_when_truncated() {
        let (temp, ctx, script) = retry_context_with_model(
            "minimax-m3",
            vec![
                Ok((200, truncated_response(r#"{"a":"#))),
                Ok((200, continuation_envelope("1}", true))),
            ],
        );
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
    ///
    /// The cap is sourced from
    /// [`crate::llm::json_strategy::max_continuation_attempts`] —
    /// `Continuation` returns `2`. The context drives `minimax-m3`
    /// so the dispatch gate fires.
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
        let (temp, ctx, script) = retry_context_with_model(
            "minimax-m3",
            vec![
                Ok((200, truncated_response("abc"))),
                Ok((200, continuation_envelope_truncated("abc", false))),
                Ok((200, continuation_envelope_truncated("abc", false))),
            ],
        );
        let result = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                String::new(),
                String::new(),
                "Value",
                0,
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
    ///
    /// Drove via `minimax-m3` so the dispatch gate
    /// ([`crate::llm::json_strategy::max_continuation_attempts`])
    /// fires; the helper is what emits `model.continuation_failed`
    /// on transport errors.
    #[tokio::test]
    async fn phase_skips_continuation_on_first_transport_error() {
        let (temp, ctx, script) = retry_context_with_model(
            "minimax-m3",
            vec![
                Ok((200, truncated_response(r#"{"x":"#))),
                Err(Error::Provider {
                    message: "transport died".into(),
                    http_status: None,
                }),
            ],
        );
        let result = ctx
            .call_with_retry_parse::<serde_json::Value>(
                Role::Intake,
                String::new(),
                String::new(),
                "Value",
                0,
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

    // ----------------------------------------------------------------
    // PR-C3: `resolve_top_p` precedence contract. Three cases pin the
    // order documented in the helper's rustdoc:
    //   1. Provider-set wins over role catalogue.
    //   2. Role catalogue used when provider absent.
    //   3. Both absent → None (wire omits the field).
    // ----------------------------------------------------------------

    /// Provider-set `top_p` wins over the role catalogue.
    /// Mirrors `resolve_temperature_provider_base_beats_role_default`
    /// for the `top_p` axis. `Role::Continuation` ships a catalogue
    /// value (0.5) so this test exercises the precedence directly.
    #[test]
    fn resolve_top_p_provider_overrides_role_settings() {
        assert_eq!(
            resolve_top_p(Role::Continuation, Some(0.42)),
            Some(0.42),
            "provider top_p must beat the catalogue value (0.5)"
        );
    }

    /// When the provider does not declare `top_p`, the role's
    /// catalogue value applies. Same precedence as
    /// `resolve_top_p_provider_overrides_role_settings` but on the
    /// fallback branch.
    #[test]
    fn resolve_top_p_role_settings_used_when_provider_absent() {
        assert_eq!(
            resolve_top_p(Role::Continuation, None),
            Some(0.5),
            "provider None + Continuation catalogue (0.5) → catalogue value"
        );
        assert_eq!(
            resolve_top_p(Role::TiefighterCritic, None),
            Some(0.1),
            "provider None + TiefighterCritic catalogue (0.1) → catalogue value"
        );
    }

    /// When neither the provider nor the role declare `top_p`, the
    /// helper returns `None` so the wire layer omits the field via
    /// `skip_serializing_if = "Option::is_none"`. This is the
    /// contract that closes the cascade gap on upstreams rejecting
    /// `top_p` — a role without a catalogue entry (e.g. `Sketch`)
    /// and a provider without a `top_p` config used to force
    /// `Some(0.95)` onto the wire; the test pins the new behaviour
    /// at the helper boundary.
    #[test]
    fn resolve_top_p_returns_none_when_neither_set() {
        assert_eq!(
            resolve_top_p(Role::Sketch, None),
            None,
            "Sketch has no RoleSettings; resolve_top_p must return None (not 0.95)"
        );
        assert_eq!(
            resolve_top_p(Role::Intake, None),
            None,
            "Intake has no RoleSettings; resolve_top_p must return None"
        );
        assert_eq!(
            resolve_top_p(Role::Clarify, None),
            None,
            "Clarify has no RoleSettings; resolve_top_p must return None"
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
                endpoint: None,
                models: Vec::new(),
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
                endpoint: None,
                models: Vec::new(),
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
        // PR-C3 contract: `top_p` is now opt-in. Neither the
        // provider nor the `Role::Sketch` role declare it, so the
        // wire must omit the field entirely (legacy behaviour was
        // to force `Some(0.95)`). The change closes the cascade
        // gap for any upstream that rejects `top_p`.
        assert_eq!(
            recorded.top_p, None,
            "without provider top_p AND without RoleSettings for the role, top_p must be None (wire omits the field)"
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
                endpoint: None,
                models: Vec::new(),
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

    // ===========================================================
    // PR-7: temperature clamp in `dispatch_to_provider`
    //
    // The gate at the top of `dispatch_to_provider` consults
    // `RunContext::temperature_table` and snaps `req.temperature`
    // to the nearest value in the auto-discovered supported set
    // for `(default_provider, default_model)`. The four tests
    // below pin the contract:
    //
    // 1. `None` table → no clamp (legacy behaviour).
    // 2. Empty set → no clamp (the gate does not interfere with
    //    providers that have not been probed yet).
    // 3. Requested value already in the set → no clamp, no warning.
    // 4. Requested value outside the set → snap to the nearest
    //    value, captured request reflects the snapped value.
    // ===========================================================

    /// Build a `TemperatureTable` from a hand-written TOML
    /// sidecar. The sidecar carries a single entry whose
    /// `temperatures` is exactly the supplied set; `from_path`
    /// hydrates the in-memory table from the file. `save=false`
    /// keeps the test from trying to rewrite the file on
    /// subsequent probes.
    fn temperature_table_for_test(
        provider: &str,
        model: &str,
        temps: &[f32],
    ) -> Arc<crate::llm::temperature_probe::TemperatureTable> {
        use crate::llm::temperature_probe::{Entry, TemperatureTableFile};
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("temperatures_auto.toml");
        let mut file = TemperatureTableFile::new_empty();
        file.providers
            .entry(provider.to_owned())
            .or_default()
            .insert(
                model.to_owned(),
                Entry {
                    temperatures: temps.to_vec(),
                    detected_at: "2026-08-22T00:00:00Z".to_owned(),
                    verified_at: "2026-08-22T00:00:00Z".to_owned(),
                    auto: true,
                    attempts: 1,
                },
            );
        file.save(&path).expect("save temperatures_auto.toml");
        let table = crate::llm::temperature_probe::TemperatureTable::from_path(&path, false)
            .expect("from_path");
        Arc::new(table)
    }

    /// Build a `RunContext` whose default provider is a
    /// freshly-constructed `RecordingProvider` named
    /// `"recording"`. The returned `Arc<Mutex<Option<Request>>>`
    /// is the slot the provider writes into on every `send`,
    /// so each test can read the request body that the dispatch
    /// gate actually transmitted.
    fn temperature_gate_context(
        table: Option<Arc<crate::llm::temperature_probe::TemperatureTable>>,
    ) -> (
        tempfile::TempDir,
        RunContext,
        Arc<parking_lot::Mutex<Option<crate::llm::Request>>>,
    ) {
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

        // Per-provider temperature stays None so the per-role
        // default (1.0 for `Role::Sketch`) does not contaminate
        // the assertion. `call_with_retry_at_temp` stamps the
        // explicit `temperature` parameter straight onto
        // `Request.temperature`, so the gate sees the test's
        // chosen value verbatim.
        let mut cfg = Config::default();
        cfg.providers.insert(
            "recording".to_owned(),
            ProviderConfig {
                endpoint: None,
                models: Vec::new(),
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
        )
        .with_temperature_table_opt(table);

        (temp, ctx, captured)
    }

    /// PR-7: `dispatch_to_provider` does not clamp when the
    /// run context has no `temperature_table` — the legacy
    /// "send whatever the caller asked for" behaviour stays
    /// bit-for-bit. A request at `temperature = 2.5` reaches
    /// the provider untouched.
    #[tokio::test]
    async fn temperature_gate_passes_when_table_is_none() {
        let (_temp, ctx, captured) = temperature_gate_context(None);
        let _ = ctx
            .call_with_retry_at_temp(Role::Sketch, String::new(), String::new(), 0, 2.5)
            .await
            .expect("call should succeed");
        let recorded = captured.lock().clone().expect("captured");
        assert_eq!(
            recorded.temperature,
            Some(2.5),
            "without a temperature_table the gate must leave the request untouched"
        );
    }

    /// PR-7: when the table is wired but the supported set is
    /// empty (no entry for `(default_provider, default_model)`),
    /// the gate stays silent and the request reaches the
    /// provider with its original temperature.
    #[tokio::test]
    async fn temperature_gate_passes_when_set_is_empty() {
        // Entry under (other-provider, other-model) — the
        // (recording, recording-model) lookup returns an empty
        // set so the gate short-circuits without warning.
        let table = temperature_table_for_test("other-provider", "other-model", &[0.0, 0.5, 1.0]);
        let (_temp, ctx, captured) = temperature_gate_context(Some(table));
        let _ = ctx
            .call_with_retry_at_temp(Role::Sketch, String::new(), String::new(), 0, 1.7)
            .await
            .expect("call should succeed");
        let recorded = captured.lock().clone().expect("captured");
        assert_eq!(
            recorded.temperature,
            Some(1.7),
            "empty supported set (entry under different provider/model) → no clamp"
        );
    }

    /// PR-7: when the requested temperature is already a member
    /// of the supported set, the gate is a no-op. The provider
    /// sees the value verbatim and no warning is emitted.
    #[tokio::test]
    async fn temperature_gate_passes_when_temperature_is_in_set() {
        let table = temperature_table_for_test("recording", "recording-model", &[0.0, 0.5, 1.0]);
        let (_temp, ctx, captured) = temperature_gate_context(Some(table));
        let _ = ctx
            .call_with_retry_at_temp(Role::Sketch, String::new(), String::new(), 0, 0.5)
            .await
            .expect("call should succeed");
        let recorded = captured.lock().clone().expect("captured");
        assert_eq!(
            recorded.temperature,
            Some(0.5),
            "value already in the set must reach the provider verbatim"
        );
    }

    /// PR-7: when the requested temperature is outside the
    /// supported set, the gate snaps it to the nearest
    /// supported value. The captured request reflects the
    /// snapped value (`0.7 → 0.5` here).
    #[tokio::test]
    async fn temperature_gate_clamps_to_nearest_with_warning() {
        let table = temperature_table_for_test("recording", "recording-model", &[0.0, 0.5, 1.0]);
        let (_temp, ctx, captured) = temperature_gate_context(Some(table));
        let _ = ctx
            .call_with_retry_at_temp(Role::Sketch, String::new(), String::new(), 0, 0.7)
            .await
            .expect("call should succeed");
        let recorded = captured.lock().clone().expect("captured");
        assert_eq!(
            recorded.temperature,
            Some(0.5),
            "out-of-range 0.7 must snap to the nearest supported value (0.5)"
        );
    }

    // ===========================================================
    // PR-C2: cascade-recovery loop (bounded `while` with cap
    // `PARAM_NAMES.len()`) + `parse_provider_error_body` envelope
    // stripping. Each test drives `call_with_retry` against a
    // scripted provider whose queue returns the responses the
    // cascade should observe.
    // ===========================================================

    /// The legacy single-shot loop only recorded the FIRST
    /// rejection name from a multi-name 4xx body, so a
    /// `"Unknown parameters: 'temperature', 'max_tokens', 'top_p'"`
    /// response burned a round-trip on `temperature` and
    /// propagated the second failure to the caller. The new
    /// cascade loop must:
    /// 1. Detect every name in one pass.
    /// 2. Record every name in `param_rejections.toml`.
    /// 3. Omit every name in one retry iteration.
    /// 4. Recover to a 200 envelope with exactly TWO `send`s
    ///    (1 fail + 1 success).
    #[tokio::test]
    async fn dispatch_recovers_from_three_param_cascade() {
        let body_json = r#"{"error":{"message":"Unknown parameters: 'temperature', 'max_tokens', 'top_p'","type":"invalid_request_error"}}"#;
        let outcomes: Vec<Result<(u16, Response)>> = vec![
            Err(Error::Provider {
                message: format!("http 400: {body_json}"),
                http_status: Some(400),
            }),
            Ok((200, response("ok"))),
        ];
        let (temp, ctx, script) = retry_context(outcomes);
        let home = ctx.home.clone();
        let table = crate::llm::param_rejections::ParamRejectionsTable::from_path(
            &home.param_rejections_path(),
        )
        .expect("from_path on a fresh home");
        let ctx = ctx.with_param_rejections(Arc::new(table));

        let result = ctx.call(Role::Intake, "sys".into(), "user".into()).await;
        assert!(
            result.is_ok(),
            "cascade must recover to 200; got {result:?}"
        );
        assert_eq!(
            script.calls.load(Ordering::SeqCst),
            2,
            "cascade must issue exactly 2 sends (1 initial fail + 1 success after omit-all)"
        );

        let persisted =
            crate::llm::param_rejections::ParamRejectionsFile::load(&home.param_rejections_path())
                .expect("load param_rejections.toml");
        let entry = persisted
            .providers
            .get("retry")
            .and_then(|m| m.get("retry-model"))
            .expect("on-disk entry for (retry, retry-model) after cascade");
        for name in ["temperature", "max_tokens", "top_p"] {
            assert!(
                entry.contains(name),
                "{name} must be persisted; got {entry:?}"
            );
        }
        drop(temp);
    }

    /// A 4xx body that doesn't match any rejection signature
    /// (auth, model-not-found, plain text) must abort the cascade
    /// loop with the upstream error intact — the dispatcher must
    /// NOT record noise into `param_rejections.toml` and must
    /// NOT issue a phantom retry.
    #[tokio::test]
    async fn dispatch_aborts_when_detector_returns_none() {
        let body = r#"{"error":"model not found"}"#;
        let outcomes: Vec<Result<(u16, Response)>> = vec![Err(Error::Provider {
            message: format!("http 404: {body}"),
            http_status: Some(404),
        })];
        let (temp, ctx, script) = retry_context(outcomes);
        let home = ctx.home.clone();
        let table = crate::llm::param_rejections::ParamRejectionsTable::from_path(
            &home.param_rejections_path(),
        )
        .expect("from_path on a fresh home");
        let ctx = ctx.with_param_rejections(Arc::new(table));

        let result = ctx.call(Role::Intake, "sys".into(), "user".into()).await;
        assert!(result.is_err(), "non-rejection 4xx must propagate");
        assert_eq!(
            script.calls.load(Ordering::SeqCst),
            1,
            "cascade must abort on the first attempt when no param is detected"
        );
        assert!(
            !home.param_rejections_path().exists(),
            "param_rejections.toml must NOT be written when the 4xx is unrelated"
        );
        drop(temp);
    }

    /// The cascade cap is `PARAM_NAMES.len()` (3 today): even if
    /// the upstream keeps returning a body with new rejection
    /// signatures on every retry, the loop must bound itself and
    /// surface the final error to the caller instead of looping
    /// forever. Pins the contract that protects the dispatcher
    /// from an upstream that never converges.
    #[tokio::test]
    async fn dispatch_caps_at_param_names_len() {
        // 5 consecutive 4xx responses, each with a fresh param
        // name. The cap is 3 iterations, so the dispatcher must
        // see exactly 4 `send` calls (1 initial + 3 retries) and
        // propagate the 4th error.
        let bodies = [
            r#"{"error":{"param":"temperature is invalid","type":"invalid_request_error","message":"..."}}"#,
            r#"{"error":{"param":"max_tokens is invalid","type":"invalid_request_error","message":"..."}}"#,
            r#"{"error":{"param":"top_p is invalid","type":"invalid_request_error","message":"..."}}"#,
            r#"{"error":{"param":"input is invalid","type":"invalid_request_error","message":"..."}}"#,
            r#"{"error":{"param":"model is invalid","type":"invalid_request_error","message":"..."}}"#,
        ];
        let outcomes: Vec<Result<(u16, Response)>> = bodies
            .iter()
            .map(|b| {
                Err(Error::Provider {
                    message: format!("http 400: {b}"),
                    http_status: Some(400),
                })
            })
            .collect();
        let (temp, ctx, script) = retry_context(outcomes);
        let home = ctx.home.clone();
        let table = crate::llm::param_rejections::ParamRejectionsTable::from_path(
            &home.param_rejections_path(),
        )
        .expect("from_path on a fresh home");
        let ctx = ctx.with_param_rejections(Arc::new(table));

        let result = ctx.call(Role::Intake, "sys".into(), "user".into()).await;
        assert!(
            result.is_err(),
            "loop must surface the final 4xx when the cap is reached"
        );
        assert_eq!(
            script.calls.load(Ordering::SeqCst),
            // 1 initial send + PARAM_NAMES.len() retries before
            // the cap trips (the 4th retry would be iteration
            // index PARAM_NAMES.len() — the loop checks the bound
            // BEFORE sending).
            1 + PARAM_NAMES.len(),
            "cascade must issue exactly 1 + PARAM_NAMES.len() sends before the cap"
        );
        drop(temp);
    }

    // ===========================================================
    // PR-C2 / PR-4b: `parse_provider_error_body` envelope
    // stripping. Pins both the strict envelope (legacy /
    // scripted providers) and the production envelope
    // (reqwest::StatusCode Display expansion).
    // ===========================================================

    /// The production envelope: `Error::Provider` wraps the
    /// transport's `format!("http {status}: {body}")` where
    /// `{status}` is `StatusCode`'s `Display`, which expands to
    /// `"400 Bad Request"`. The detector must parse JSON, not a
    /// labelled string, so the helper must strip
    /// `"provider error: http 400 Bad Request: "` (with reason
    /// phrase) cleanly.
    #[test]
    fn parse_provider_error_body_handles_status_with_reason_phrase() {
        let body = r#"{"error":{"message":"[unknown_parameter] Unknown parameter: 'top_p'","type":"invalid_request_error"}}"#;
        let err = Error::Provider {
            message: format!("http 400 Bad Request: {body}"),
            http_status: Some(400),
        };
        let parsed = parse_provider_error_body(&err, 400);
        assert_eq!(
            parsed, body,
            "reason phrase 'Bad Request' must be stripped along with the envelope"
        );
    }

    /// Multi-word reason phrases (e.g. status codes with longer
    /// IANA reason texts) must also be tolerated.
    #[test]
    fn parse_provider_error_body_handles_multi_word_reason_phrase() {
        let body = r#"{"error":{"param":"temperature is too large","type":"server_error","message":"..."}}"#;
        let err = Error::Provider {
            // 511 "Network Authentication Required" — a multi-word
            // reason phrase that exercises the slice logic beyond
            // a single token.
            message: format!("http 511 Network Authentication Required: {body}"),
            http_status: Some(511),
        };
        let parsed = parse_provider_error_body(&err, 511);
        assert_eq!(
            parsed, body,
            "multi-word reason phrase must be stripped along with the envelope"
        );
    }

    /// The strict envelope (legacy single-shot loop expectation)
    /// must keep working — scripted providers and tests build
    /// the error message by hand without a reason phrase.
    #[test]
    fn parse_provider_error_body_preserves_strict_envelope() {
        let body = r#"{"error":{"message":"Unknown parameter: 'max_tokens'","type":"invalid_request_error"}}"#;
        let err = Error::Provider {
            message: format!("http 400: {body}"),
            http_status: Some(400),
        };
        let parsed = parse_provider_error_body(&err, 400);
        assert_eq!(parsed, body, "strict envelope must keep stripping cleanly");
    }

    /// When no envelope matches, the helper must return the raw
    /// `Display` so the detector at least gets something to chew
    /// on. The detector then parses JSON if possible or returns
    /// `None` if the body is garbage.
    #[test]
    fn parse_provider_error_body_returns_raw_when_no_match() {
        let err = Error::Provider {
            message: "no envelope here at all".to_owned(),
            http_status: Some(400),
        };
        let parsed = parse_provider_error_body(&err, 400);
        assert_eq!(parsed, "no envelope here at all");
    }

    /// End-to-end: build the exact error message that
    /// `classify_status` would build in production, with the
    /// `StatusCode` `Display` expansion, and confirm the helper
    /// recovers the JSON body the detector needs.
    #[test]
    fn parse_provider_error_body_handles_real_classify_status_envelope() {
        use reqwest::StatusCode;
        let body = r#"{"error":{"message":"invalid params, param 'top_p' should be in (0,1]","type":"invalid_request_error"}}"#;
        let envelope = format!("http {sc}: {body}", sc = StatusCode::BAD_REQUEST);
        // Sanity: confirm the production envelope actually
        // contains the reason phrase. If `reqwest` ever drops it
        // this test will need to track that.
        assert!(
            envelope.contains("Bad Request"),
            "StatusCode Display must continue to include the reason phrase; got {envelope}"
        );
        let err = Error::Provider {
            message: envelope,
            http_status: Some(400),
        };
        let parsed = parse_provider_error_body(&err, 400);
        assert_eq!(parsed, body);
    }
}
